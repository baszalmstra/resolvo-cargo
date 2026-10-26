//! `cargo generate-lockfile` and `cargo update` without cargo: resolve a
//! Cargo workspace against the crates.io index and write the `Cargo.lock`
//! cargo would write, byte for byte. Then, for one build, resolve which
//! features each locked package is compiled with, as cargo's feature
//! resolver does.
//!
//! ```no_run
//! # async fn run(root: &std::path::Path, cache: std::path::PathBuf) -> miette::Result<()> {
//! use resolvo_cargo::{IndexSource, Lockfile, ResolveOptions, Workspace};
//!
//! let workspace = Workspace::load(root)?;
//! let previous = match std::fs::read_to_string(root.join("Cargo.lock")) {
//!     Ok(text) => Some(Lockfile::parse(&text)?),
//!     Err(_) => None,
//! };
//! let options = ResolveOptions { previous, ..ResolveOptions::default() };
//! let source = IndexSource::crates_io(cache, false);
//! let lockfile = resolvo_cargo::resolve(&workspace, &source, &options).await?;
//! std::fs::write(root.join("Cargo.lock"), lockfile.to_string()).ok();
//! # Ok(())
//! # }
//! ```
//!
//! [`Workspace`] loads the members and path dependencies of a workspace
//! from their manifests. [`resolve`] selects a version for every package
//! the workspace reaches, as cargo does for a lock file: with all features
//! and dev-dependencies of every member, preferring the versions of the
//! previous [`Lockfile`]. Version selection runs on resolvo.
//! [`resolve_features`] narrows a lock file down to one build described by
//! a [`FeatureRequest`]: the members and features asked for, the target
//! and host platforms, and whether dev-dependencies are built.

mod error;
mod features;
mod index;
mod lockfile;
mod names;
mod resolve;
mod solver;
mod summary;
mod workspace;

pub use error::{
    FeatureError, FeatureMapError, IndexError, LockfileError, ManifestError, ResolveError,
    RustVersionError, TargetPlatformError, UnsupportedFeature,
};
pub use features::{
    FeatureRequest, FeaturesFor, RequestedPackage, ResolvedDependency, ResolvedFeatures,
    TargetPlatform, resolve_features,
};
pub use index::IndexSource;
pub use lockfile::{LockedPackage, Lockfile, LockfileVersion, PackageId, PackageSource};
pub use names::{DepName, FeatureName, PackageName};
pub use resolve::{ResolveOptions, resolve};
pub use summary::{DepKind, DepSource, Dependency, FeatureValue, RustVersion, Summary};
pub use workspace::{PathPackage, ResolverVersion, Workspace};

/// The README's code, compiled as a doctest so it stays current.
#[doc = include_str!("../README.md")]
#[cfg(doctest)]
pub struct ReadmeDoctests;
