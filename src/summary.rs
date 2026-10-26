//! What resolution needs to know about one version of a package, whether it
//! comes from a registry index entry or from a `Cargo.toml` on disk: its
//! dependencies and its feature map. This is the input model shared by the
//! version resolver and a later feature resolver.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
    str::FromStr,
};

use semver::{Version, VersionReq};

use crate::{
    error::{FeatureMapError, RustVersionError},
    names::{DepName, FeatureName, PackageName},
};

/// Which kind of dependency table an entry comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepKind {
    /// `[dependencies]`.
    Normal,
    /// `[build-dependencies]`.
    Build,
    /// `[dev-dependencies]`, resolved only for workspace members.
    Dev,
}

/// Where the package a dependency names comes from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepSource {
    /// The registry the resolution runs against (crates.io).
    Registry,
    /// A package on disk: the normalized absolute path of the directory
    /// holding its `Cargo.toml`.
    Path(PathBuf),
}

/// One dependency entry: a key in a dependency table of a manifest, or an
/// element of `deps` in an index entry. A package may have several entries
/// with the same key, for different kinds or targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    /// The key the dependency is known by in its dependent's manifest.
    pub name: DepName,
    /// The crate it resolves to: `package = "..."`, or the key.
    pub package: PackageName,
    /// The versions it accepts. `*` for a path dependency without
    /// `version`.
    pub req: VersionReq,
    /// Where the package comes from.
    pub source: DepSource,
    /// Features it enables on the package.
    pub features: Vec<FeatureName>,
    /// Whether it enables the package's `default` feature.
    pub default_features: bool,
    /// Whether it is only enabled through a feature.
    pub optional: bool,
    /// Which dependency table it is in.
    pub kind: DepKind,
    /// The `cfg(...)` or target triple it is limited to. The lock file is
    /// platform-independent, so the version resolver ignores it.
    pub target: Option<String>,
}

impl Dependency {
    /// Whether `version` satisfies this dependency. A path dependency
    /// without a `version` accepts its path package whatever the version,
    /// pre-releases included, as cargo's unconstrained requirement does.
    pub(crate) fn accepts(&self, version: &Version) -> bool {
        match self.source {
            DepSource::Path(_) if self.req.comparators.is_empty() => true,
            DepSource::Path(_) | DepSource::Registry => self.req.matches(version),
        }
    }
}

/// One value of a feature's list in a `[features]` table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FeatureValue {
    /// `"g"`: another feature of the same package.
    Feature(FeatureName),
    /// `"dep:d"`: the optional dependency `d`, without the implicit
    /// feature of the same name.
    Dep(DepName),
    /// `"d/f"` or, if `weak`, `"d?/f"`: feature `f` of dependency `d`.
    DepFeature {
        /// The dependency key.
        dep: DepName,
        /// The feature enabled on it.
        feature: FeatureName,
        /// `d?/f`: does not enable `d` if it is optional.
        weak: bool,
    },
}

impl FeatureValue {
    /// Parses a feature value as cargo does: `d/f` and `d?/f` first, then
    /// `dep:d`, else a feature name. Never fails; invalid shapes such as
    /// `dep:d/f` are rejected by [`Summary::new`].
    pub fn parse(value: &str) -> FeatureValue {
        match value.split_once('/') {
            Some((dep, feature)) => {
                let stripped = dep.strip_suffix('?');
                FeatureValue::DepFeature {
                    dep: DepName::new(stripped.unwrap_or(dep)),
                    feature: FeatureName::new(feature),
                    weak: stripped.is_some(),
                }
            }
            None => match value.strip_prefix("dep:") {
                Some(dep) => FeatureValue::Dep(DepName::new(dep)),
                None => FeatureValue::Feature(FeatureName::new(value)),
            },
        }
    }
}

impl fmt::Display for FeatureValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeatureValue::Feature(feature) => write!(f, "{feature}"),
            FeatureValue::Dep(dep) => write!(f, "dep:{dep}"),
            FeatureValue::DepFeature { dep, feature, weak } => {
                let weak = if *weak { "?" } else { "" };
                write!(f, "{dep}{weak}/{feature}")
            }
        }
    }
}

/// A `rust-version`: a version with optional minor and patch components,
/// such as `1.70`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RustVersion {
    major: u64,
    minor: Option<u64>,
    patch: Option<u64>,
}

impl RustVersion {
    /// Whether a toolchain of version `rustc` can build a package declaring
    /// this `rust-version`: `rustc` matches the caret requirement of this
    /// version, pre-release identifiers ignored (cargo's
    /// `RustVersion::is_compatible_with`).
    pub fn is_compatible_with(&self, rustc: &RustVersion) -> bool {
        let rustc = (
            rustc.major,
            rustc.minor.unwrap_or_default(),
            rustc.patch.unwrap_or_default(),
        );
        let minimum = (
            self.major,
            self.minor.unwrap_or_default(),
            self.patch.unwrap_or_default(),
        );
        // A caret requirement on a non-zero major (every Rust version) is
        // the minimum up to the next major.
        rustc.0 == minimum.0 && rustc >= minimum
    }
}

impl From<&Version> for RustVersion {
    fn from(version: &Version) -> Self {
        RustVersion {
            major: version.major,
            minor: Some(version.minor),
            patch: Some(version.patch),
        }
    }
}

impl FromStr for RustVersion {
    type Err = RustVersionError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || RustVersionError {
            text: text.to_owned(),
        };
        let mut parts = text.trim().split('.');
        let mut component = |required: bool| -> Result<Option<u64>, RustVersionError> {
            match parts.next() {
                Some(part) => part.parse().map(Some).map_err(|_| invalid()),
                None if required => Err(invalid()),
                None => Ok(None),
            }
        };
        let major = component(true)?.ok_or_else(invalid)?;
        let minor = component(false)?;
        let patch = match minor {
            Some(_) => component(false)?,
            None => None,
        };
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(RustVersion {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for RustVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.major)?;
        if let Some(minor) = self.minor {
            write!(f, ".{minor}")?;
        }
        if let Some(patch) = self.patch {
            write!(f, ".{patch}")?;
        }
        Ok(())
    }
}

/// One version of a package with what resolution needs: dependencies,
/// features, `links` and `rust-version`.
///
/// The feature map is validated as cargo's `build_feature_map` does and
/// includes implicit features: an optional dependency `d` gets the feature
/// `d = ["dep:d"]` unless a feature is named `d` or some feature value uses
/// `dep:d`.
#[derive(Clone, Debug)]
pub struct Summary {
    name: PackageName,
    version: Version,
    dependencies: Vec<Dependency>,
    features: BTreeMap<FeatureName, Vec<FeatureValue>>,
    links: Option<String>,
    rust_version: Option<RustVersion>,
}

impl Summary {
    /// Builds a summary from a feature table as written (`features` merged
    /// with `features2` for index entries), adding implicit features.
    ///
    /// # Errors
    ///
    /// [`FeatureMapError`] for the feature tables cargo rejects: optional
    /// dev-dependencies, values naming neither a feature nor a dependency,
    /// `dep:` or `?` on dependencies that are not optional, `dep:d/f`, and
    /// optional dependencies no feature enables.
    pub fn new(
        name: PackageName,
        version: Version,
        dependencies: Vec<Dependency>,
        features: &BTreeMap<FeatureName, Vec<String>>,
        links: Option<String>,
        rust_version: Option<RustVersion>,
    ) -> Result<Summary, FeatureMapError> {
        let features = build_feature_map(&name, features, &dependencies)?;
        Ok(Summary {
            name,
            version,
            dependencies,
            features,
            links,
            rust_version,
        })
    }

    /// The crate name.
    pub fn name(&self) -> &PackageName {
        &self.name
    }

    /// The version.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Every dependency entry, of all kinds and targets.
    pub fn dependencies(&self) -> &[Dependency] {
        &self.dependencies
    }

    /// The feature map, implicit features included.
    pub fn features(&self) -> &BTreeMap<FeatureName, Vec<FeatureValue>> {
        &self.features
    }

    /// The `links` value: the native library only one package in a graph
    /// may link.
    pub fn links(&self) -> Option<&str> {
        self.links.as_deref()
    }

    /// The declared `rust-version`.
    pub fn rust_version(&self) -> Option<&RustVersion> {
        self.rust_version.as_ref()
    }

    /// Whether some entry with key `dep` is optional.
    pub(crate) fn is_optional_dep(&self, dep: &DepName) -> bool {
        self.dependencies
            .iter()
            .any(|entry| entry.name == *dep && entry.optional)
    }
}

/// cargo's `build_feature_map` (`core/summary.rs`): adds implicit features
/// and rejects the tables cargo rejects.
fn build_feature_map(
    package: &PackageName,
    features: &BTreeMap<FeatureName, Vec<String>>,
    dependencies: &[Dependency],
) -> Result<BTreeMap<FeatureName, Vec<FeatureValue>>, FeatureMapError> {
    let invalid = |feature: &FeatureName, value: String, reason| FeatureMapError::InvalidValue {
        package: package.clone(),
        feature: feature.clone(),
        value,
        reason,
    };

    for dependency in dependencies {
        if dependency.optional && dependency.kind == DepKind::Dev {
            return Err(FeatureMapError::OptionalDevDependency {
                package: package.clone(),
                dep: dependency.name.clone(),
            });
        }
    }

    // Whether any entry of each dependency key is optional.
    let mut optional_by_name: BTreeMap<&DepName, bool> = BTreeMap::new();
    for dependency in dependencies {
        *optional_by_name.entry(&dependency.name).or_default() |= dependency.optional;
    }

    let mut map: BTreeMap<FeatureName, Vec<FeatureValue>> = features
        .iter()
        .map(|(feature, values)| {
            let values = values.iter().map(|value| FeatureValue::parse(value));
            (feature.clone(), values.collect())
        })
        .collect();

    let explicitly_listed: BTreeSet<DepName> = map
        .values()
        .flatten()
        .filter_map(|value| match value {
            FeatureValue::Dep(dep) => Some(dep.clone()),
            FeatureValue::Feature(_) | FeatureValue::DepFeature { .. } => None,
        })
        .collect();
    for dependency in dependencies {
        let implicit = FeatureName::new(dependency.name.as_str());
        if dependency.optional
            && !features.contains_key(&implicit)
            && !explicitly_listed.contains(&dependency.name)
        {
            map.insert(implicit, vec![FeatureValue::Dep(dependency.name.clone())]);
        }
    }

    for (feature, values) in &map {
        for value in values {
            let text = value.to_string();
            match value {
                FeatureValue::Feature(name) => {
                    if features.contains_key(name) {
                        continue;
                    }
                    match optional_by_name.get(&DepName::new(name.as_str())) {
                        None => {
                            return Err(invalid(
                                feature,
                                text,
                                "neither a dependency nor a feature",
                            ));
                        }
                        Some(true) if !map.contains_key(name) => {
                            return Err(invalid(
                                feature,
                                text,
                                "an optional dependency without an implicit feature",
                            ));
                        }
                        Some(true) => {}
                        Some(false) => {
                            return Err(invalid(feature, text, "not an optional dependency"));
                        }
                    }
                }
                FeatureValue::Dep(dep) => match optional_by_name.get(dep) {
                    None => return Err(invalid(feature, text, "not a dependency")),
                    Some(false) => {
                        return Err(invalid(feature, text, "not an optional dependency"));
                    }
                    Some(true) => {}
                },
                FeatureValue::DepFeature {
                    dep,
                    feature: dep_feature,
                    weak,
                } => {
                    if dep_feature.as_str().contains('/') {
                        return Err(invalid(feature, text, "multiple slashes"));
                    }
                    if dep.as_str().starts_with("dep:") {
                        return Err(invalid(feature, text, "both `dep:` and `/`"));
                    }
                    match optional_by_name.get(dep) {
                        None => return Err(invalid(feature, text, "not a dependency")),
                        Some(false) if *weak => {
                            return Err(invalid(
                                feature,
                                text,
                                "`?` on a dependency that is not optional",
                            ));
                        }
                        Some(_) => {}
                    }
                }
            }
        }
    }

    let used: BTreeSet<&DepName> = map
        .values()
        .flatten()
        .filter_map(|value| match value {
            FeatureValue::Dep(dep) | FeatureValue::DepFeature { dep, .. } => Some(dep),
            FeatureValue::Feature(_) => None,
        })
        .collect();
    if let Some((dep, _)) = optional_by_name
        .iter()
        .find(|(dep, optional)| **optional && !used.contains(*dep))
    {
        return Err(FeatureMapError::UnusedOptionalDependency {
            package: package.clone(),
            dep: (*dep).clone(),
        });
    }

    Ok(map)
}
