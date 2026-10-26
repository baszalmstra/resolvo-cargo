//! Loading a Cargo workspace: its members, the path packages they depend
//! on, and each package's manifest normalized into a [`Summary`].

mod loader;
mod manifest;

use std::path::{Path, PathBuf};

use crate::{error::ManifestError, summary::Summary};

/// The `resolver` of a workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResolverVersion {
    /// `resolver = "1"`, the default of virtual workspaces and edition
    /// 2015 and 2018 packages.
    V1,
    /// `resolver = "2"`, the default of edition 2021.
    V2,
    /// `resolver = "3"`, the default of edition 2024: the resolver prefers
    /// versions whose `rust-version` the workspace supports.
    V3,
}

/// A package on disk: a workspace member, or a path dependency of one.
#[derive(Clone, Debug)]
pub struct PathPackage {
    directory: PathBuf,
    summary: Summary,
    member: bool,
    proc_macro: bool,
}

impl PathPackage {
    /// A path package in `directory` with metadata `summary`, a member if
    /// `member`, whose library is not a procedural macro.
    pub fn new(directory: PathBuf, summary: Summary, member: bool) -> PathPackage {
        PathPackage {
            directory,
            summary,
            member,
            proc_macro: false,
        }
    }

    /// This package with its library a procedural macro if `proc_macro`.
    pub fn with_proc_macro(self, proc_macro: bool) -> PathPackage {
        PathPackage { proc_macro, ..self }
    }

    /// The normalized absolute directory holding its `Cargo.toml`. Equals
    /// the [`DepSource::Path`](crate::DepSource::Path) of dependencies on
    /// it.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The normalized manifest: `workspace = true` inheritance applied,
    /// every dependency table of every target.
    pub fn summary(&self) -> &Summary {
        &self.summary
    }

    /// Whether it is a workspace member. Members are resolved with all
    /// features and their dev-dependencies; other path packages only with
    /// what their dependents enable.
    pub fn is_member(&self) -> bool {
        self.member
    }

    /// Whether its library is a procedural macro (`[lib] proc-macro =
    /// true`): built for the host, with features apart from target builds
    /// under resolver 2.
    pub fn is_proc_macro(&self) -> bool {
        self.proc_macro
    }
}

/// A loaded Cargo workspace.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    resolver: ResolverVersion,
    packages: Vec<PathPackage>,
}

impl Workspace {
    /// Loads the workspace whose root manifest is `root/Cargo.toml`;
    /// parent directories are not searched for another root.
    ///
    /// Members are found as cargo finds them: the expanded `members`
    /// patterns, the root package, and path dependencies inside the root
    /// directory that `exclude` does not exclude, transitively. Every other
    /// path dependency is loaded as a non-member path package.
    ///
    /// # Errors
    ///
    /// [`ManifestError`] when a manifest cannot be read or is invalid, or
    /// uses a feature this crate does not support.
    pub fn load(root: &Path) -> Result<Workspace, ManifestError> {
        loader::load(root)
    }

    /// A workspace of packages that are not read from manifests, such as
    /// a registry package resolved as if it were the workspace's only
    /// member. Every [`DepSource::Path`](crate::DepSource::Path) among the
    /// packages' dependencies must be the directory of one of `packages`,
    /// and directories must be normalized and absolute, as
    /// [`Workspace::load`] makes them.
    pub fn from_packages(
        root: PathBuf,
        resolver: ResolverVersion,
        mut packages: Vec<PathPackage>,
    ) -> Workspace {
        packages.sort_by(|a, b| a.directory.cmp(&b.directory));
        Workspace {
            root,
            resolver,
            packages,
        }
    }

    /// The normalized absolute directory of the root manifest.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The resolver version: `workspace.resolver`, else the root
    /// package's `package.resolver` or its edition's default, else `V1`.
    pub fn resolver(&self) -> ResolverVersion {
        self.resolver
    }

    /// Every path package, members and non-members, sorted by directory.
    pub fn packages(&self) -> &[PathPackage] {
        &self.packages
    }

    /// The workspace members.
    pub fn members(&self) -> impl Iterator<Item = &PathPackage> {
        self.packages.iter().filter(|package| package.member)
    }
}
