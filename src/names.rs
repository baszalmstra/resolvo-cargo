//! Distinct types for the names a Cargo resolution juggles: crate names,
//! dependency keys and feature names, and the semver-compatible range a
//! version belongs to. A dependency key differs from the crate name under
//! `package = "..."` renames, so mixing them up is the classic bug here.

use std::{borrow::Borrow, fmt};

use semver::Version;

macro_rules! name_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Wraps `name` without validating it.
            pub fn new(name: impl Into<String>) -> Self {
                Self(name.into())
            }

            /// The name as written.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(name: &str) -> Self {
                Self(name.to_owned())
            }
        }
    };
}

name_type!(
    /// The name of a crate: `[package] name` of a manifest, `name` of an index
    /// entry, and the `package = "..."` of a renamed dependency.
    PackageName
);

name_type!(
    /// The key of a dependency in `[dependencies]` (cargo's `name_in_toml`).
    /// Equals the [`PackageName`] of the crate unless the dependency is
    /// renamed with `package = "..."`. Feature values (`dep:x`, `x/f`) and
    /// implicit features refer to dependencies by this name.
    DepName
);

name_type!(
    /// The name of a feature in a `[features]` table.
    FeatureName
);

impl FeatureName {
    /// The feature `default`, enabled unless a dependency says
    /// `default-features = false`.
    pub(crate) fn default_feature() -> Self {
        Self::new("default")
    }
}

/// The semver-compatible range a version belongs to, cargo's
/// `SemverCompatibility`: versions in one range are interchangeable, so
/// cargo selects at most one version per range of a package, while
/// versions of different ranges may coexist.
///
/// The range is the first non-zero component: `1.2.3` is in `Major(1)`,
/// `0.2.3` in `Minor(2)` and `0.0.3` in `Patch(3)`. Pre-release and build
/// metadata are ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Compat {
    /// `major.*.*` for a non-zero major version.
    Major(u64),
    /// `0.minor.*` for a non-zero minor version.
    Minor(u64),
    /// `0.0.patch`.
    Patch(u64),
}

impl From<&Version> for Compat {
    fn from(version: &Version) -> Self {
        if version.major != 0 {
            Compat::Major(version.major)
        } else if version.minor != 0 {
            Compat::Minor(version.minor)
        } else {
            Compat::Patch(version.patch)
        }
    }
}

impl fmt::Display for Compat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Compat::Major(major) => write!(f, "{major}.*"),
            Compat::Minor(minor) => write!(f, "0.{minor}.*"),
            Compat::Patch(patch) => write!(f, "0.0.{patch}"),
        }
    }
}
