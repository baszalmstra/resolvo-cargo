//! Why loading a workspace, loading the index, parsing a lock file or
//! resolving failed. Every failure is a variant callers match on.

use std::{io, path::PathBuf};

use miette::Diagnostic;
use reqwest::StatusCode;
use semver::Version;
use thiserror::Error;
use url::Url;

use crate::names::{DepName, FeatureName, PackageName};

/// Why a text is not a `rust-version`.
#[derive(Debug, Error, Diagnostic)]
#[error("'{text}' is not a rust-version: expected `major[.minor[.patch]]`")]
#[diagnostic(code(resolvo_cargo::invalid_rust_version))]
pub struct RustVersionError {
    /// The text as written.
    pub text: String,
}

/// Why a feature table is one cargo rejects, as checked by
/// [`Summary::new`](crate::Summary::new).
#[derive(Debug, Error, Diagnostic)]
pub enum FeatureMapError {
    /// A dev-dependency is marked `optional`.
    #[error("`{package}`: dev-dependencies are not allowed to be optional: `{dep}`")]
    #[diagnostic(code(resolvo_cargo::optional_dev_dependency))]
    OptionalDevDependency {
        /// The package declaring it.
        package: PackageName,
        /// The dependency key.
        dep: DepName,
    },

    /// A feature value names nothing it may name.
    #[error("`{package}`: feature `{feature}` includes `{value}`, which is {reason}")]
    #[diagnostic(code(resolvo_cargo::invalid_feature_value))]
    InvalidValue {
        /// The package declaring the feature.
        package: PackageName,
        /// The feature.
        feature: FeatureName,
        /// The value as written.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },

    /// An optional dependency is enabled by no feature, so nothing can
    /// ever enable it.
    #[error("`{package}`: optional dependency `{dep}` is not included in any feature")]
    #[diagnostic(
        code(resolvo_cargo::unused_optional_dependency),
        help("include `dep:{dep}` in one of the features in the [features] table")
    )]
    UnusedOptionalDependency {
        /// The package declaring it.
        package: PackageName,
        /// The dependency key.
        dep: DepName,
    },
}

/// A manifest feature this resolver does not implement. Each is rejected
/// rather than ignored, because ignoring it would produce a lock file
/// cargo rewrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedFeature {
    /// A dependency with `git = "..."`.
    GitDependency(DepName),
    /// A dependency with `registry = "..."` or `registry-index = "..."`.
    AlternativeRegistry(DepName),
    /// A dependency with `public = ...`.
    PublicDependency(DepName),
    /// A dependency with `artifact = ...`.
    ArtifactDependency(DepName),
    /// A dependency with `base = ...` (path bases).
    PathBase(DepName),
    /// A `[patch]` table.
    Patch,
    /// A `[replace]` table.
    Replace,
}

impl std::fmt::Display for UnsupportedFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnsupportedFeature::GitDependency(dep) => write!(f, "git dependency `{dep}`"),
            UnsupportedFeature::AlternativeRegistry(dep) => {
                write!(f, "dependency `{dep}` from an alternative registry")
            }
            UnsupportedFeature::PublicDependency(dep) => {
                write!(f, "public/private dependency `{dep}`")
            }
            UnsupportedFeature::ArtifactDependency(dep) => {
                write!(f, "artifact dependency `{dep}`")
            }
            UnsupportedFeature::PathBase(dep) => write!(f, "path base of dependency `{dep}`"),
            UnsupportedFeature::Patch => write!(f, "[patch] table"),
            UnsupportedFeature::Replace => write!(f, "[replace] table"),
        }
    }
}

/// Why a Cargo workspace could not be loaded by
/// [`Workspace::load`](crate::Workspace::load).
#[derive(Debug, Error, Diagnostic)]
pub enum ManifestError {
    /// A manifest could not be read.
    #[error("reading '{}'", path.display())]
    #[diagnostic(code(resolvo_cargo::read_manifest))]
    Read {
        /// The manifest.
        path: PathBuf,
        /// What the filesystem reported.
        #[source]
        source: io::Error,
    },

    /// A manifest is not valid TOML or does not have the shape of a
    /// `Cargo.toml`.
    #[error("parsing '{}'", path.display())]
    #[diagnostic(code(resolvo_cargo::parse_manifest))]
    Parse {
        /// The manifest.
        path: PathBuf,
        /// What the TOML parser reported.
        #[source]
        source: Box<toml::de::Error>,
    },

    /// A workspace member or path dependency has no `[package]` table.
    #[error("'{}' has no [package] table", path.display())]
    #[diagnostic(code(resolvo_cargo::not_a_package))]
    NotAPackage {
        /// The manifest.
        path: PathBuf,
    },

    /// A required field is missing, such as `package.name`.
    #[error("'{}' is missing `{field}`", path.display())]
    #[diagnostic(code(resolvo_cargo::missing_field))]
    MissingField {
        /// The manifest.
        path: PathBuf,
        /// The dotted path of the field.
        field: String,
    },

    /// A field says `workspace = true` but the workspace root does not
    /// define it.
    #[error("'{}' inherits `{field}` from a workspace that does not define it", path.display())]
    #[diagnostic(code(resolvo_cargo::not_inherited))]
    NotInherited {
        /// The manifest.
        path: PathBuf,
        /// The dotted path of the field.
        field: String,
    },

    /// A version or version requirement does not parse.
    #[error("'{}': `{field}` is not a valid version: '{text}'", path.display())]
    #[diagnostic(code(resolvo_cargo::invalid_version))]
    InvalidVersion {
        /// The manifest.
        path: PathBuf,
        /// The dotted path of the field.
        field: String,
        /// The text as written.
        text: String,
        /// What the semver parser reported.
        #[source]
        source: semver::Error,
    },

    /// `rust-version` does not parse.
    #[error("'{}': invalid `rust-version`", path.display())]
    #[diagnostic(code(resolvo_cargo::manifest_rust_version))]
    InvalidRustVersion {
        /// The manifest.
        path: PathBuf,
        /// Why it does not parse.
        #[source]
        source: RustVersionError,
    },

    /// `resolver` or `edition` has a value cargo does not know.
    #[error("'{}': `{field}` has unknown value '{value}'", path.display())]
    #[diagnostic(code(resolvo_cargo::unknown_value))]
    UnknownValue {
        /// The manifest.
        path: PathBuf,
        /// The dotted path of the field.
        field: String,
        /// The value as written.
        value: String,
    },

    /// A `members` or `exclude` pattern is not a valid glob.
    #[error("'{pattern}' is not a valid workspace member pattern")]
    #[diagnostic(code(resolvo_cargo::member_pattern))]
    MemberPattern {
        /// The pattern as written.
        pattern: String,
        /// What the glob parser reported.
        #[source]
        source: glob::PatternError,
    },

    /// Matching a `members` pattern against the filesystem failed.
    #[error("expanding the workspace member pattern '{pattern}'")]
    #[diagnostic(code(resolvo_cargo::member_glob))]
    MemberGlob {
        /// The pattern as written.
        pattern: String,
        /// The directory that could not be read.
        #[source]
        source: glob::GlobError,
    },

    /// Both `[workspace]` and `[package]` of the root manifest set
    /// `resolver`.
    #[error("'{}' specifies `resolver` in both [workspace] and [package]", path.display())]
    #[diagnostic(code(resolvo_cargo::conflicting_resolver))]
    ConflictingResolver {
        /// The root manifest.
        path: PathBuf,
    },

    /// An underscore spelling such as `dev_dependencies` or
    /// `default_features`, which edition 2024 rejects in favor of the
    /// dashed spelling.
    #[error(
        "'{}': `{key}` is unsupported as of the 2024 edition; use `{}`",
        path.display(),
        key.replace('_', "-")
    )]
    #[diagnostic(code(resolvo_cargo::underscore_key))]
    UnderscoreKey {
        /// The manifest.
        path: PathBuf,
        /// The key as written.
        key: String,
    },

    /// A dependency names no version, path or workspace dependency.
    #[error("'{}': dependency `{dep}` specifies no version, path or workspace dependency", path.display())]
    #[diagnostic(code(resolvo_cargo::dependency_without_source))]
    DependencyWithoutSource {
        /// The manifest.
        path: PathBuf,
        /// The dependency key.
        dep: DepName,
    },

    /// Edition 2024 rejects `default-features = false` on a dependency
    /// inherited from a workspace dependency that enables default features.
    #[error(
        "'{}': `default-features = false` on `{dep}` is ignored because the workspace dependency enables default features",
        path.display()
    )]
    #[diagnostic(code(resolvo_cargo::inherited_default_features))]
    InheritedDefaultFeatures {
        /// The manifest.
        path: PathBuf,
        /// The dependency key.
        dep: DepName,
    },

    /// The feature table is invalid.
    #[error("'{}': invalid [features]", path.display())]
    #[diagnostic(code(resolvo_cargo::manifest_features))]
    Features {
        /// The manifest.
        path: PathBuf,
        /// What is invalid.
        #[source]
        source: FeatureMapError,
    },

    /// The manifest uses a feature this resolver does not implement.
    #[error("'{}' uses a {feature}, which resolvo-cargo does not support", path.display())]
    #[diagnostic(code(resolvo_cargo::unsupported_manifest_feature))]
    Unsupported {
        /// The manifest.
        path: PathBuf,
        /// What it uses.
        feature: UnsupportedFeature,
    },
}

/// Why the index entries a resolution needs could not be loaded, as part
/// of [`ResolveError::Index`].
#[derive(Debug, Error, Diagnostic)]
pub enum IndexError {
    /// The HTTP client could not be built on this host.
    #[error("building the HTTP client for the registry index")]
    #[diagnostic(code(resolvo_cargo::http_client))]
    Client(#[source] reqwest::Error),

    /// A request for an index file failed before a response arrived.
    #[error("fetching the index entry of `{name}` from {url}")]
    #[diagnostic(code(resolvo_cargo::fetch_index))]
    Fetch {
        /// The crate.
        name: PackageName,
        /// The index file URL.
        url: Url,
        /// What the HTTP client reported.
        #[source]
        source: reqwest::Error,
    },

    /// The registry answered with a status other than success, not
    /// modified, or not found.
    #[error("the registry answered {status} for the index entry of `{name}` at {url}")]
    #[diagnostic(code(resolvo_cargo::index_status))]
    Status {
        /// The crate.
        name: PackageName,
        /// The index file URL.
        url: Url,
        /// The HTTP status.
        status: StatusCode,
    },

    /// The URL of a crate's index file cannot be formed from the index
    /// root, such as a root that cannot be a base URL.
    #[error("forming the index file URL of `{name}` under {root}")]
    #[diagnostic(code(resolvo_cargo::index_url))]
    InvalidUrl {
        /// The crate.
        name: PackageName,
        /// The index root URL.
        root: Url,
        /// What the URL parser reported.
        #[source]
        source: url::ParseError,
    },

    /// Offline, and the index entry of a crate is not in the cache.
    #[error("the index entry of `{name}` is not cached and the index is offline")]
    #[diagnostic(code(resolvo_cargo::not_cached))]
    NotCached {
        /// The crate.
        name: PackageName,
    },

    /// An index or cache file could not be read or written.
    #[error("accessing '{}'", path.display())]
    #[diagnostic(code(resolvo_cargo::index_io))]
    Io {
        /// The file.
        path: PathBuf,
        /// What the filesystem reported.
        #[source]
        source: io::Error,
    },

    /// A line of an index file is not an index entry, not even its `name`
    /// and `vers`.
    #[error("line {line} of the index entry of `{name}` is not valid")]
    #[diagnostic(code(resolvo_cargo::parse_index))]
    Parse {
        /// The crate.
        name: PackageName,
        /// The 1-based line number.
        line: usize,
        /// What the JSON parser reported.
        #[source]
        source: serde_json::Error,
    },
}

/// Why a text is not a `Cargo.lock` this crate reads, as reported by
/// [`Lockfile::parse`](crate::Lockfile::parse).
#[derive(Debug, Error, Diagnostic)]
pub enum LockfileError {
    /// Not valid TOML or not shaped like a lock file.
    #[error("parsing Cargo.lock")]
    #[diagnostic(code(resolvo_cargo::parse_lockfile))]
    Parse(#[source] Box<toml::de::Error>),

    /// A format version other than 3 or 4; `None` is the unversioned
    /// format 1 or 2.
    #[error("Cargo.lock format version {version:?} is not supported, only 3 and 4")]
    #[diagnostic(
        code(resolvo_cargo::lockfile_version),
        help("run `cargo update` with a recent cargo to upgrade the lock file")
    )]
    UnsupportedVersion {
        /// The `version` field.
        version: Option<u32>,
    },

    /// A package with a source other than crates.io or a path.
    #[error("package `{name}` comes from '{source_id}', which resolvo-cargo does not support")]
    #[diagnostic(code(resolvo_cargo::lockfile_source))]
    UnsupportedSource {
        /// The package.
        name: PackageName,
        /// The `source` field.
        source_id: String,
    },

    /// A package version that is not semver.
    #[error("package `{name}` has version '{text}', which is not semver")]
    #[diagnostic(code(resolvo_cargo::lockfile_package_version))]
    InvalidVersion {
        /// The package.
        name: PackageName,
        /// The version as written.
        text: String,
        /// What the semver parser reported.
        #[source]
        source: semver::Error,
    },

    /// The same package is listed twice.
    #[error("package `{name} {version}` is listed twice")]
    #[diagnostic(code(resolvo_cargo::lockfile_duplicate))]
    DuplicatePackage {
        /// The package.
        name: PackageName,
        /// Its version.
        version: Version,
    },

    /// A `dependencies` entry is not `name`, `name version` or
    /// `name version (source)`.
    #[error("package `{name}` has dependency '{text}', which is not a package id")]
    #[diagnostic(code(resolvo_cargo::lockfile_dependency))]
    InvalidDependency {
        /// The package.
        name: PackageName,
        /// The entry as written.
        text: String,
    },
}

/// Why no `Cargo.lock` could be resolved, as reported by
/// [`resolve`](crate::resolve). Every unsolvable variant carries the
/// solver's explanation in cargo terms in `message`.
#[derive(Debug, Error, Diagnostic)]
pub enum ResolveError {
    /// A feature is requested of a package whose matching versions do not
    /// define it.
    #[error(
        "failed to select a version: package `{package}` has no feature `{feature}`\n{message}"
    )]
    #[diagnostic(code(resolvo_cargo::missing_feature))]
    MissingFeature {
        /// The package.
        package: PackageName,
        /// The feature.
        feature: FeatureName,
        /// The explanation.
        message: String,
    },

    /// Every version matching a requirement is yanked, and none is pinned
    /// by the previous lock file.
    #[error(
        "failed to select a version for `{package}`: every matching version is yanked\n{message}"
    )]
    #[diagnostic(code(resolvo_cargo::only_yanked))]
    OnlyYanked {
        /// The package.
        package: PackageName,
        /// The explanation.
        message: String,
    },

    /// Two packages declare the same `links` value.
    #[error(
        "failed to select a version: more than one package links to native library `{links}`\n{message}"
    )]
    #[diagnostic(code(resolvo_cargo::links_conflict))]
    LinksConflict {
        /// The `links` value.
        links: String,
        /// The explanation.
        message: String,
    },

    /// Any other conflict between requirements.
    #[error("failed to select a version for the requirements\n{message}")]
    #[diagnostic(code(resolvo_cargo::conflict))]
    Conflict {
        /// The explanation.
        message: String,
    },

    /// The selected packages depend on each other in a cycle that does not
    /// go through a dev-dependency.
    #[error("cyclic package dependency: {}", format_cycle(packages))]
    #[diagnostic(code(resolvo_cargo::cycle))]
    Cycle {
        /// The packages on the cycle, starting and ending with the same
        /// package.
        packages: Vec<(PackageName, Version)>,
    },

    /// Loading index files failed.
    #[error("loading the registry index")]
    #[diagnostic(code(resolvo_cargo::index))]
    Index(#[source] Box<IndexError>),

    /// The index lists a different checksum for a version than the
    /// previous lock file: the registry or the lock file changed under it.
    #[error("checksum for `{package} v{version}` changed between lock files")]
    #[diagnostic(code(resolvo_cargo::checksum_changed))]
    ChecksumChanged {
        /// The package.
        package: PackageName,
        /// Its version.
        version: Version,
    },
}

fn format_cycle(packages: &[(PackageName, Version)]) -> String {
    packages
        .iter()
        .map(|(name, version)| format!("`{name} v{version}`"))
        .collect::<Vec<_>>()
        .join(" -> ")
}

impl From<IndexError> for ResolveError {
    fn from(error: IndexError) -> Self {
        ResolveError::Index(Box::new(error))
    }
}

/// Why a line of `rustc --print cfg` output is not a cfg, as reported by
/// [`TargetPlatform::new`](crate::TargetPlatform::new).
#[derive(Debug, Error, Diagnostic)]
#[error("'{line}' is not a cfg as `rustc --print cfg` prints them")]
#[diagnostic(code(resolvo_cargo::invalid_cfg))]
pub struct TargetPlatformError {
    /// The line as given.
    pub line: String,
}

/// Why the features of a build could not be resolved, as reported by
/// [`resolve_features`](crate::resolve_features).
#[derive(Debug, Error, Diagnostic)]
pub enum FeatureError {
    /// A requested package is not a workspace member.
    #[error("`{package}` is not a member of the workspace")]
    #[diagnostic(code(resolvo_cargo::unknown_member))]
    UnknownMember {
        /// The requested package.
        package: PackageName,
    },

    /// A requested feature is not defined by the package it is requested
    /// of.
    #[error("package `{package}` does not have the feature `{feature}`")]
    #[diagnostic(code(resolvo_cargo::requested_feature_missing))]
    MissingFeature {
        /// The package.
        package: PackageName,
        /// The feature.
        feature: FeatureName,
    },

    /// A requested `d/f` or `d?/f` names no dependency of the member.
    #[error("package `{package}` does not have a dependency named `{dependency}`")]
    #[diagnostic(code(resolvo_cargo::requested_dependency_missing))]
    MissingDependency {
        /// The member.
        package: PackageName,
        /// The dependency name as requested.
        dependency: DepName,
    },

    /// A workspace package is not in the lock file, which is out of date.
    #[error("`{package} v{version}` is not in the lock file")]
    #[diagnostic(
        code(resolvo_cargo::not_locked),
        help("resolve the workspace again to update the lock file")
    )]
    NotLocked {
        /// The package.
        package: PackageName,
        /// Its version.
        version: Version,
    },

    /// A locked registry version is not in the index, or its index entry
    /// is one cargo rejects.
    #[error("`{package} v{version}` of the lock file is not usable from the index")]
    #[diagnostic(code(resolvo_cargo::not_in_index))]
    NotInIndex {
        /// The package.
        package: PackageName,
        /// Its version.
        version: Version,
    },

    /// Loading index files failed.
    #[error("loading the registry index")]
    #[diagnostic(code(resolvo_cargo::feature_index))]
    Index(#[source] Box<IndexError>),
}

impl From<IndexError> for FeatureError {
    fn from(error: IndexError) -> Self {
        FeatureError::Index(Box::new(error))
    }
}
