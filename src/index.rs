//! The registry index entries a resolution reaches, fetched before solving
//! so that each solve is synchronous, deterministic, and offline just a
//! cache read.

mod fetch;
mod parse;
mod walk;

use std::{
    ops::Range,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use futures::{StreamExt, stream::FuturesUnordered};
use semver::{Version, VersionReq};
use url::Url;

use self::{fetch::Fetcher, walk::Walk};
use crate::{
    error::IndexError,
    lockfile::{Lockfile, PackageSource},
    names::PackageName,
    summary::{RustVersion, Summary},
    workspace::Workspace,
};

/// How many index files are fetched at once.
const MAX_CONCURRENT_FETCHES: usize = 64;

/// Where index entries come from.
#[derive(Clone, Debug)]
pub enum IndexSource {
    /// A registry speaking the sparse protocol, such as
    /// `https://index.crates.io/`, with entries cached in `cache_dir` and
    /// revalidated with ETag / Last-Modified. `offline` reads the cache only.
    Sparse {
        /// The index root URL, ending in `/`.
        url: Url,
        /// Where fetched entries are cached.
        cache_dir: PathBuf,
        /// Whether to read the cache only.
        offline: bool,
    },
    /// An index on disk with the sparse protocol's layout, for local
    /// registries and test fixtures.
    Directory(PathBuf),
}

impl IndexSource {
    /// crates.io's sparse index, `https://index.crates.io/`.
    pub fn crates_io(cache_dir: PathBuf, offline: bool) -> IndexSource {
        IndexSource::Sparse {
            url: Url::parse("https://index.crates.io/").expect("valid URL"),
            cache_dir,
            offline,
        }
    }
}

/// One version of a crate in the index.
///
/// Only the fields that identify and rank the version are parsed when the
/// index file is read. Dependencies and features are parsed on first use
/// by [`IndexEntry::summary`]: a resolution uses a few versions of each
/// crate it reaches, and parsing them all would dominate its run time.
#[derive(Clone)]
pub(crate) struct IndexEntry {
    name: PackageName,
    version: Version,
    checksum: String,
    yanked: bool,
    links: Option<String>,
    rust_version: Option<RustVersion>,
    /// The index file and the range of this entry's line in it.
    file: Arc<[u8]>,
    line: Range<usize>,
    summary: OnceLock<Option<Summary>>,
}

impl IndexEntry {
    /// The crate name, exactly as published.
    pub(crate) fn name(&self) -> &PackageName {
        &self.name
    }

    /// The version.
    pub(crate) fn version(&self) -> &Version {
        &self.version
    }

    /// The sha256 of the `.crate` file (`cksum`).
    pub(crate) fn checksum(&self) -> &str {
        &self.checksum
    }

    /// Whether the version is yanked.
    pub(crate) fn is_yanked(&self) -> bool {
        self.yanked
    }

    /// The `links` value.
    pub(crate) fn links(&self) -> Option<&str> {
        self.links.as_deref()
    }

    /// The declared `rust-version`.
    pub(crate) fn rust_version(&self) -> Option<&RustVersion> {
        self.rust_version.as_ref()
    }

    /// The version's dependencies and features, parsed on first use.
    /// `None` when cargo rejects them: a malformed dependency list, a
    /// requirement that does not parse, a dependency from another
    /// registry, or a feature table cargo refuses. Such a version takes
    /// no part in a resolution.
    pub(crate) fn summary(&self) -> Option<&Summary> {
        self.summary
            .get_or_init(|| {
                let line = self.file.get(self.line.clone()).unwrap_or_default();
                parse::parse_summary(line)
            })
            .as_ref()
    }
}

impl std::fmt::Debug for IndexEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexEntry")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("yanked", &self.yanked)
            .finish_non_exhaustive()
    }
}

/// What a solve needs loaded beyond what [`Index`] holds, for
/// [`Index::require`] to load before solving again.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum IndexRequest {
    /// The versions of a crate matching `req`: a dependency of a version
    /// the solver reached.
    Dependency {
        /// The crate.
        package: PackageName,
        /// The versions needed.
        req: VersionReq,
    },
    /// What every matching version of a loaded crate depends on, not only
    /// the versions tried first: the solver went past those, and is likely
    /// to try more.
    AllVersions {
        /// The crate.
        package: PackageName,
    },
}

/// The index entries loaded for a resolution: every version of each
/// crate whose index file was read.
#[derive(Default)]
pub(crate) struct Index {
    walk: Walk,
    /// How many times [`Index::require`] was called.
    requires: usize,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("crates", &self.walk.packages().len())
            .finish_non_exhaustive()
    }
}

impl Index {
    /// Fetches the index files of the crates the workspace is expected to
    /// reach.
    ///
    /// The walk starts from the registry dependencies every path package
    /// activates with all its features (dev-dependencies too for workspace
    /// members), and fetches the registry packages of `previous`. Of each
    /// fetched crate, the version the solver tries first for a requirement
    /// (the highest non-yanked match of each semver-compatible range) has
    /// the dependencies the requirement's features activate followed in
    /// turn. Each crate's index file is fetched once, with up to 64
    /// fetches in flight. A resolution that reaches further reports what
    /// it misses, which [`Index::require`] adds.
    ///
    /// # Errors
    ///
    /// [`IndexError`] when an index file cannot be fetched, read or parsed.
    pub(crate) async fn load(
        source: &IndexSource,
        workspace: &Workspace,
        previous: Option<&Lockfile>,
    ) -> Result<Index, IndexError> {
        let mut index = Index::default();
        for package in workspace.packages() {
            index
                .walk
                .require_dependencies_of(package.summary(), package.is_member());
        }
        for package in previous.map(Lockfile::packages).unwrap_or_default() {
            match package.id.source {
                PackageSource::Registry => index.walk.fetch(&package.id.name),
                PackageSource::Path => {}
            }
        }
        index.fetch(source).await?;
        Ok(index)
    }

    /// Fetches the index files of exactly the crates `names`, following
    /// none of their dependencies: what reading the versions of a lock
    /// file needs.
    pub(crate) async fn load_crates<'a>(
        source: &IndexSource,
        names: impl IntoIterator<Item = &'a PackageName>,
    ) -> Result<Index, IndexError> {
        let mut index = Index::default();
        for name in names {
            index.walk.fetch(name);
        }
        index.fetch(source).await?;
        Ok(index)
    }

    /// Adds the crates and versions a resolution reported missing, and
    /// what they in turn are expected to reach.
    ///
    /// From the second call on, every matching version of every crate is
    /// followed, not only the versions tried first. A resolution that
    /// comes back twice is backtracking through versions the walk did not
    /// follow, and each incomplete solve costs about as much as a complete
    /// one, so loading more at once is cheaper than solving again and
    /// again.
    ///
    /// # Errors
    ///
    /// [`IndexError`] when an index file cannot be fetched, read or parsed.
    pub(crate) async fn require(
        &mut self,
        source: &IndexSource,
        requests: &[IndexRequest],
    ) -> Result<(), IndexError> {
        for request in requests {
            match request {
                IndexRequest::Dependency { package, req } => self.walk.require(package, req),
                IndexRequest::AllVersions { package } => self.walk.follow_all(package),
            }
        }
        self.requires += 1;
        if self.requires >= 2 {
            self.walk.follow_all_crates();
        }
        self.fetch(source).await
    }

    /// Fetches index files until the walk needs no more.
    async fn fetch(&mut self, source: &IndexSource) -> Result<(), IndexError> {
        let fetcher = Fetcher::new(source)?;
        let fetcher = &fetcher;
        let mut in_flight = FuturesUnordered::new();
        loop {
            while in_flight.len() < MAX_CONCURRENT_FETCHES
                && let Some(name) = self.walk.next_fetch()
            {
                in_flight.push(async move {
                    let contents = fetcher.fetch(&name).await;
                    (name, contents)
                });
            }
            let Some((name, contents)) = in_flight.next().await else {
                break;
            };
            let entries = match contents? {
                Some(contents) => parse::parse_index_file(&name, contents)?,
                None => Vec::new(),
            };
            self.walk.loaded(&name, entries);
        }
        Ok(())
    }

    /// Whether the index file of `name` was read. A crate that is loaded
    /// but has no entries does not exist in the registry.
    pub(crate) fn is_loaded(&self, name: &PackageName) -> bool {
        self.walk.is_loaded(name)
    }

    /// The entry of exactly `version` of the crate named `name`.
    pub(crate) fn entry(&self, name: &PackageName, version: &Version) -> Option<&IndexEntry> {
        self.walk
            .packages()
            .get(name)?
            .iter()
            .find(|entry| entry.version() == version)
    }

    /// Every loaded crate with its usable versions, ascending, in crate
    /// name order. Entries with a format version (`v`) above 2 are left
    /// out, as cargo leaves them out.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&PackageName, &[IndexEntry])> {
        self.walk
            .packages()
            .iter()
            .map(|(name, entries)| (name, entries.as_slice()))
    }
}
