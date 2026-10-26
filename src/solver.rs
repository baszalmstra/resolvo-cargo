//! The resolvo encoding of a Cargo resolution.
//!
//! Cargo selects at most one version per semver-compatible range of a
//! package ([`Compat`]) and allows several across ranges. Each range is its
//! own solver name ([`SolverName::Package`]), so resolvo's rule of one
//! candidate per name gives exactly cargo's rule. Features, default
//! features, optional dependencies and `links` values are names of their
//! own whose candidates are the versions of a range, each requiring the
//! [`SolverName::Package`] candidate of the same version.
//!
//! A dependency whose requirement spans several ranges is satisfied
//! through [`SolverName::Proxy`] names, one per range, each requiring the
//! range's package with the dependency's features. Requiring the features
//! of a range only when the dependency resolves to it is what lets cargo
//! use a range selected for another reason that lacks those features while
//! the dependency itself resolves to another range.

use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    collections::HashMap,
    fmt::{self, Display},
    path::Path,
};

use resolvo::{
    Candidates, Condition, ConditionId, ConditionalRequirement, Dependencies, DependencyProvider,
    Interner, KnownDependencies, NameId, Requirement, SolvableId, SolverCache, StringId,
    VersionSetId, VersionSetUnionId,
    utils::{Pool, VersionSet as ResolvoVersionSet},
};
use semver::{Version, VersionReq};

use crate::{
    index::{Index, IndexEntry, IndexRequest},
    lockfile::{Lockfile, PackageId, PackageSource},
    names::{Compat, DepName, FeatureName, PackageName},
    summary::{DepKind, DepSource, Dependency, FeatureValue, RustVersion, Summary},
    workspace::{ResolverVersion, Workspace},
};

/// Why a candidate is excluded, as resolvo reports it.
pub(crate) const YANKED: &str = "yanked";

/// Why an index entry's candidates have unknown dependencies.
const REJECTED: &str = "its index entry has dependencies or features cargo rejects";

/// Why a feature candidate has unknown dependencies: its version does not
/// define the feature.
pub(crate) const MISSING_FEATURE: &str = "it does not have the feature";

/// Why an optional-dependency candidate has unknown dependencies: its
/// version does not have the optional dependency.
const MISSING_OPTIONAL_DEP: &str = "it does not have the optional dependency";

/// One version of a package known to the resolution: an index entry or a
/// path package.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct VersionRef(u32);

impl VersionRef {
    fn index(self) -> usize {
        self.0 as usize
    }
}

impl Display for VersionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Where a package comes from, as far as telling packages apart goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum SourceKey {
    /// The registry.
    Registry,
    /// The path package at this index of [`Workspace::packages`].
    Path(usize),
}

/// One semver-compatible range of a package: what cargo activates at most
/// one version of.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PackageKey {
    pub name: PackageName,
    pub source: SourceKey,
    pub compat: Compat,
}

/// A solver package name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SolverName {
    /// The versions of a range.
    Package(PackageKey),
    /// The versions of a range that define a feature, with it enabled.
    Feature(PackageKey, FeatureName),
    /// The versions of a range, with their `default` feature enabled if
    /// they have one.
    DefaultFeatures(PackageKey),
    /// The versions of a range that have an optional dependency of this
    /// key, with it enabled.
    OptionalDep(PackageKey, DepName),
    /// The package versions declaring this `links` value; one may be
    /// selected.
    Links(String),
    /// Dependency `entry` of `parent` resolved to the range `bucket`.
    Proxy {
        parent: VersionRef,
        entry: usize,
        bucket: PackageKey,
    },
}

/// A set of candidates of one [`SolverName`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum VersionSet {
    /// The candidates whose version matches the requirement.
    Matches(VersionReq),
    /// The candidate of exactly this package version.
    Exactly(VersionRef),
    /// Every candidate.
    Any,
}

impl ResolvoVersionSet for VersionSet {
    type V = VersionRef;
}

/// Where a package version's metadata lives.
#[derive(Clone, Copy)]
enum Origin<'a> {
    /// A path package's manifest.
    Path(&'a Summary),
    /// An index entry, whose summary is parsed on first use.
    Registry(&'a IndexEntry),
}

/// A package version with what preference and encoding need.
pub(crate) struct PackageVersion<'a> {
    pub key: PackageKey,
    version: &'a Version,
    links: Option<&'a str>,
    origin: Origin<'a>,
    pub checksum: Option<&'a str>,
    pub yanked: bool,
    pub member: bool,
    /// In the previous lock file.
    pub locked: bool,
    /// How many of the workspace's Rust versions it supports (cargo's
    /// `msrv_compat_count`).
    pub rust_version_score: usize,
}

impl<'a> PackageVersion<'a> {
    pub fn version(&self) -> &'a Version {
        self.version
    }

    /// The dependencies and features; `None` for an index entry cargo
    /// rejects, which takes no part in resolution.
    pub fn summary(&self) -> Option<&'a Summary> {
        match self.origin {
            Origin::Path(summary) => Some(summary),
            Origin::Registry(entry) => entry.summary(),
        }
    }

    /// The dependency entries, none for a rejected index entry.
    pub fn dependencies(&self) -> &'a [Dependency] {
        self.summary()
            .map(Summary::dependencies)
            .unwrap_or_default()
    }

    pub fn locked_id(&self) -> PackageId {
        PackageId {
            name: self.key.name.clone(),
            version: self.version().clone(),
            source: match self.key.source {
                SourceKey::Registry => PackageSource::Registry,
                SourceKey::Path(_) => PackageSource::Path,
            },
        }
    }

    /// Whether resolution includes `dependency`: dev-dependencies only of
    /// workspace members.
    pub fn includes(&self, dependency: &Dependency) -> bool {
        match dependency.kind {
            DepKind::Normal | DepKind::Build => true,
            DepKind::Dev => self.member,
        }
    }
}

/// The ranges a dependency entry can resolve to.
#[derive(Clone)]
pub(crate) enum EntryTarget {
    /// Ranges with a matching version, most preferred first.
    Buckets(Vec<PackageKey>),
    /// No version matches; the range the requirement points at, so the
    /// conflict names it.
    Missing(PackageKey),
}

/// The resolvo provider: every package version the resolution can reach,
/// grouped by range.
pub(crate) struct Provider<'a> {
    pool: Pool<VersionSet, SolverName>,
    versions: Vec<PackageVersion<'a>>,
    buckets: HashMap<PackageKey, Vec<VersionRef>>,
    registry: HashMap<&'a PackageName, Vec<VersionRef>>,
    paths: HashMap<&'a Path, VersionRef>,
    links: HashMap<&'a str, Vec<VersionRef>>,
    members: Vec<VersionRef>,
    yanked: StringId,
    /// [`Self::entry_target`] by dependency source, package and requirement.
    entry_targets: RefCell<HashMap<(DepSource, PackageName, VersionReq), EntryTarget>>,
    index: &'a Index,
    /// See [`Self::missing`].
    missing: RefCell<Vec<IndexRequest>>,
    /// The package version whose dependencies are being computed.
    expanding: Cell<Option<VersionRef>>,
}

impl<'a> Provider<'a> {
    pub fn new(
        workspace: &'a Workspace,
        index: &'a Index,
        previous: Option<&Lockfile>,
        rust_version: Option<&Version>,
    ) -> Provider<'a> {
        let locked: std::collections::HashSet<&PackageId> = previous
            .iter()
            .flat_map(|lockfile| lockfile.packages())
            .map(|package| &package.id)
            .collect();
        let rust_versions = workspace_rust_versions(workspace, rust_version);
        let score = |declared: Option<&RustVersion>| match declared {
            None => rust_versions.len(),
            Some(declared) => rust_versions
                .iter()
                .filter(|rustc| declared.is_compatible_with(rustc))
                .count(),
        };

        let pool = Pool::new();
        let yanked = pool.intern_string(YANKED);
        let mut provider = Provider {
            pool,
            versions: Vec::new(),
            buckets: HashMap::new(),
            registry: HashMap::new(),
            paths: HashMap::new(),
            links: HashMap::new(),
            members: Vec::new(),
            yanked,
            entry_targets: RefCell::default(),
            index,
            missing: RefCell::default(),
            expanding: Cell::new(None),
        };

        for (position, package) in workspace.packages().iter().enumerate() {
            let summary = package.summary();
            let key = PackageKey {
                name: summary.name().clone(),
                source: SourceKey::Path(position),
                compat: Compat::from(summary.version()),
            };
            let reference = provider.push(PackageVersion {
                key,
                version: summary.version(),
                links: summary.links(),
                origin: Origin::Path(summary),
                checksum: None,
                yanked: false,
                member: package.is_member(),
                locked: false,
                rust_version_score: score(summary.rust_version()),
            });
            provider.paths.insert(package.directory(), reference);
            if package.is_member() {
                provider.members.push(reference);
            }
        }

        for (name, entries) in index.iter() {
            for entry in entries {
                let key = PackageKey {
                    name: entry.name().clone(),
                    source: SourceKey::Registry,
                    compat: Compat::from(entry.version()),
                };
                let locked = locked.contains(&PackageId {
                    name: name.clone(),
                    version: entry.version().clone(),
                    source: PackageSource::Registry,
                });
                let reference = provider.push(PackageVersion {
                    key,
                    version: entry.version(),
                    links: entry.links(),
                    origin: Origin::Registry(entry),
                    checksum: Some(entry.checksum()),
                    yanked: entry.is_yanked(),
                    member: false,
                    locked,
                    rust_version_score: score(entry.rust_version()),
                });
                provider.registry.entry(name).or_default().push(reference);
            }
        }
        provider
    }

    fn push(&mut self, version: PackageVersion<'a>) -> VersionRef {
        let reference = VersionRef(u32::try_from(self.versions.len()).unwrap_or(u32::MAX));
        self.buckets
            .entry(version.key.clone())
            .or_default()
            .push(reference);
        if let Some(links) = version.links {
            self.links.entry(links).or_default().push(reference);
        }
        self.versions.push(version);
        reference
    }

    pub fn version(&self, reference: VersionRef) -> &PackageVersion<'a> {
        &self.versions[reference.index()]
    }

    pub fn pool(&self) -> &Pool<VersionSet, SolverName> {
        &self.pool
    }

    pub fn members(&self) -> &[VersionRef] {
        &self.members
    }

    /// The selected candidate's package version and solver name.
    pub fn solvable(&self, solvable: SolvableId) -> (&SolverName, VersionRef) {
        let solvable = self.pool.resolve_solvable(solvable);
        (
            self.pool.resolve_package_name(solvable.name),
            solvable.record,
        )
    }

    /// The root requirements: every member with every feature, as cargo
    /// resolves a lock file with all features of all members.
    pub fn root_requirements(&self) -> Vec<ConditionalRequirement> {
        let mut requirements = Vec::new();
        for &member in &self.members {
            let version = self.version(member);
            requirements.push(self.exactly(SolverName::Package(version.key.clone()), member));
            for feature in version
                .summary()
                .into_iter()
                .flat_map(|summary| summary.features().keys())
            {
                requirements.push(self.exactly(
                    SolverName::Feature(version.key.clone(), feature.clone()),
                    member,
                ));
            }
        }
        requirements
    }

    /// Cargo's candidate order: versions in the previous lock file first,
    /// then the most supported `rust-version`, then the highest version.
    /// Name and source break the remaining ties between `links`
    /// candidates, so the order never depends on which crates are loaded.
    pub fn preference(&self, a: VersionRef, b: VersionRef) -> Ordering {
        let a = self.version(a);
        let b = self.version(b);
        b.locked
            .cmp(&a.locked)
            .then_with(|| b.rust_version_score.cmp(&a.rust_version_score))
            .then_with(|| b.version().cmp(a.version()))
            .then_with(|| a.key.name.cmp(&b.key.name))
            .then_with(|| a.key.source.cmp(&b.key.source))
    }

    /// What the solver reached that the index has not loaded, without
    /// duplicates. While any is, the solver saw those dependencies as
    /// unsatisfiable, so its result does not hold; it is cancelled as soon
    /// as it checks, rather than searching on in a world missing parts.
    pub fn missing(&self) -> Vec<IndexRequest> {
        let mut seen = std::collections::HashSet::new();
        self.missing
            .borrow()
            .iter()
            .filter(|request| seen.insert(*request))
            .cloned()
            .collect()
    }

    /// The ranges `dependency` can resolve to.
    pub fn entry_target(&self, dependency: &Dependency) -> EntryTarget {
        let key = (
            dependency.source.clone(),
            dependency.package.clone(),
            dependency.req.clone(),
        );
        if let Some(target) = self.entry_targets.borrow().get(&key) {
            return target.clone();
        }
        let target = self.find_entry_target(dependency);
        self.entry_targets.borrow_mut().insert(key, target.clone());
        target
    }

    fn find_entry_target(&self, dependency: &Dependency) -> EntryTarget {
        match &dependency.source {
            DepSource::Path(directory) => {
                let Some(&reference) = self.paths.get(directory.as_path()) else {
                    return EntryTarget::Missing(missing_key(dependency));
                };
                let version = self.version(reference);
                if dependency.accepts(version.version()) {
                    EntryTarget::Buckets(vec![version.key.clone()])
                } else {
                    EntryTarget::Missing(version.key.clone())
                }
            }
            DepSource::Registry => {
                if !self.index.is_loaded(&dependency.package) {
                    let mut missing = self.missing.borrow_mut();
                    missing.push(IndexRequest::Dependency {
                        package: dependency.package.clone(),
                        req: dependency.req.clone(),
                    });
                    // The parent is a version the index walk did not
                    // follow, so the solver went past the versions of its
                    // crate tried first.
                    if let Some(parent) = self.expanding.get()
                        && self.version(parent).key.source == SourceKey::Registry
                    {
                        missing.push(IndexRequest::AllVersions {
                            package: self.version(parent).key.name.clone(),
                        });
                    }
                    return EntryTarget::Missing(missing_key(dependency));
                }
                let mut matching: Vec<VersionRef> = self
                    .registry
                    .get(&dependency.package)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|&reference| dependency.accepts(self.version(reference).version()))
                    .collect();
                matching.sort_by(|&a, &b| self.preference(a, b));
                let mut buckets: Vec<PackageKey> = Vec::new();
                for reference in matching {
                    let key = &self.version(reference).key;
                    if !buckets.contains(key) {
                        buckets.push(key.clone());
                    }
                }
                if buckets.is_empty() {
                    EntryTarget::Missing(missing_key(dependency))
                } else {
                    EntryTarget::Buckets(buckets)
                }
            }
        }
    }

    fn name(&self, name: SolverName) -> NameId {
        self.pool.intern_package_name(name)
    }

    fn version_set(&self, name: SolverName, set: VersionSet) -> VersionSetId {
        let name = self.name(name);
        self.pool.intern_version_set(name, set)
    }

    fn exactly(&self, name: SolverName, reference: VersionRef) -> ConditionalRequirement {
        self.version_set(name, VersionSet::Exactly(reference))
            .into()
    }

    /// The requirement of `name` matching what `dependency` accepts.
    fn matching(&self, name: SolverName, dependency: &Dependency) -> ConditionalRequirement {
        self.version_set(name, accepted(dependency)).into()
    }

    fn proxy_condition(
        &self,
        parent: VersionRef,
        entry: usize,
        bucket: &PackageKey,
    ) -> ConditionId {
        let proxy = self.version_set(
            SolverName::Proxy {
                parent,
                entry,
                bucket: bucket.clone(),
            },
            VersionSet::Any,
        );
        self.pool.intern_condition(Condition::Requirement(proxy))
    }

    /// The requirements of including dependency `entry` of `parent`: its
    /// package, its features and its default features.
    fn add_entry(
        &self,
        parent: VersionRef,
        entry: usize,
        requirements: &mut Vec<ConditionalRequirement>,
    ) {
        let Some(dependency) = self.version(parent).dependencies().get(entry) else {
            return;
        };
        match self.entry_target(dependency) {
            EntryTarget::Missing(key) => {
                requirements.push(self.matching(SolverName::Package(key), dependency));
            }
            EntryTarget::Buckets(buckets) => match buckets.as_slice() {
                [bucket] => self.add_bucket(dependency, bucket, requirements),
                several => {
                    let mut proxies = several.iter().map(|bucket| {
                        self.version_set(
                            SolverName::Proxy {
                                parent,
                                entry,
                                bucket: bucket.clone(),
                            },
                            VersionSet::Any,
                        )
                    });
                    if let Some(first) = proxies.next() {
                        let union = self.pool.intern_version_set_union(first, proxies);
                        requirements.push(Requirement::Union(union).into());
                    }
                }
            },
        }
    }

    /// The requirements of `dependency` resolving to `bucket`.
    fn add_bucket(
        &self,
        dependency: &Dependency,
        bucket: &PackageKey,
        requirements: &mut Vec<ConditionalRequirement>,
    ) {
        requirements.push(self.matching(SolverName::Package(bucket.clone()), dependency));
        for feature in &dependency.features {
            requirements.push(self.matching(
                SolverName::Feature(bucket.clone(), feature.clone()),
                dependency,
            ));
        }
        if dependency.default_features {
            requirements
                .push(self.matching(SolverName::DefaultFeatures(bucket.clone()), dependency));
        }
    }

    /// The requirements of enabling `feature` on dependency `entry` of
    /// `parent`, wherever it resolves to.
    fn add_dep_feature(
        &self,
        parent: VersionRef,
        entry: usize,
        feature: &FeatureName,
        requirements: &mut Vec<ConditionalRequirement>,
    ) {
        let Some(dependency) = self.version(parent).dependencies().get(entry) else {
            return;
        };
        match self.entry_target(dependency) {
            // The dependency itself cannot be satisfied, which its own
            // requirement explains.
            EntryTarget::Missing(_) => {}
            EntryTarget::Buckets(buckets) => match buckets.as_slice() {
                [bucket] => requirements.push(self.matching(
                    SolverName::Feature(bucket.clone(), feature.clone()),
                    dependency,
                )),
                several => {
                    for bucket in several {
                        let condition = self.proxy_condition(parent, entry, bucket);
                        let requirement = self.version_set(
                            SolverName::Feature(bucket.clone(), feature.clone()),
                            accepted(dependency),
                        );
                        requirements
                            .push(Requirement::Single(requirement).with_condition(condition));
                    }
                }
            },
        }
    }

    /// Entries of `parent` with key `dep` that resolution includes.
    fn entries_named<'s>(
        &'s self,
        parent: VersionRef,
        dep: &'s DepName,
    ) -> impl Iterator<Item = usize> + 's {
        let version = self.version(parent);
        version
            .dependencies()
            .iter()
            .enumerate()
            .filter(move |(_, dependency)| dependency.name == *dep && version.includes(dependency))
            .map(|(entry, _)| entry)
    }

    /// `None` for an index entry cargo rejects.
    fn package_dependencies(&self, reference: VersionRef) -> Option<Vec<ConditionalRequirement>> {
        let version = self.version(reference);
        let summary = version.summary()?;
        let mut requirements = Vec::new();
        for (entry, dependency) in summary.dependencies().iter().enumerate() {
            if version.includes(dependency) && !dependency.optional {
                self.add_entry(reference, entry, &mut requirements);
            }
        }
        if let Some(links) = version.links {
            requirements.push(self.exactly(SolverName::Links(links.to_owned()), reference));
        }
        Some(requirements)
    }

    /// cargo's `Requirements::require_feature` for one feature
    /// (`core/resolver/dep_cache.rs`). A weak `d?/f` enables `d` too: the
    /// lock file is resolved as if it did, and the feature resolver narrows
    /// it later.
    fn feature_dependencies(
        &self,
        key: &PackageKey,
        feature: &FeatureName,
        reference: VersionRef,
    ) -> Result<Vec<ConditionalRequirement>, String> {
        let version = self.version(reference);
        let Some(summary) = version.summary() else {
            return Err(REJECTED.to_owned());
        };
        let Some(values) = summary.features().get(feature) else {
            return Err(MISSING_FEATURE.to_owned());
        };
        let mut requirements = vec![self.exactly(SolverName::Package(key.clone()), reference)];
        for value in values {
            match value {
                FeatureValue::Feature(other) => {
                    if other == feature {
                        return Err(format!("feature `{feature}` depends on itself"));
                    }
                    requirements.push(
                        self.exactly(SolverName::Feature(key.clone(), other.clone()), reference),
                    );
                }
                FeatureValue::Dep(dep) => {
                    requirements.push(
                        self.exactly(SolverName::OptionalDep(key.clone(), dep.clone()), reference),
                    );
                }
                FeatureValue::DepFeature {
                    dep,
                    feature: dep_feature,
                    weak,
                } => {
                    let optional = summary.is_optional_dep(dep);
                    let implicit = FeatureName::new(dep.as_str());
                    if !weak && optional && summary.features().contains_key(&implicit) {
                        requirements.push(
                            self.exactly(SolverName::Feature(key.clone(), implicit), reference),
                        );
                    }
                    if optional {
                        requirements.push(
                            self.exactly(
                                SolverName::OptionalDep(key.clone(), dep.clone()),
                                reference,
                            ),
                        );
                    }
                    for entry in self.entries_named(reference, dep) {
                        self.add_dep_feature(reference, entry, dep_feature, &mut requirements);
                    }
                }
            }
        }
        Ok(requirements)
    }

    fn candidates(&self, name: &SolverName) -> Vec<VersionRef> {
        let bucket = |key: &PackageKey| self.buckets.get(key).into_iter().flatten().copied();
        match name {
            // Every version of the range is a candidate; versions without
            // the feature or optional dependency have unknown dependencies,
            // which excludes them. Filtering here would parse the feature
            // table of every version of the range, most of which the
            // solver never considers.
            SolverName::Package(key)
            | SolverName::DefaultFeatures(key)
            | SolverName::Feature(key, _)
            | SolverName::OptionalDep(key, _) => bucket(key).collect(),
            SolverName::Links(links) => self
                .links
                .get(links.as_str())
                .into_iter()
                .flatten()
                .copied()
                .collect(),
            SolverName::Proxy { parent, .. } => vec![*parent],
        }
    }

    fn display_version_ref(&self, reference: VersionRef) -> String {
        let version = self.version(reference);
        match version.key.source {
            SourceKey::Registry => format!("{} v{}", version.key.name, version.version()),
            SourceKey::Path(_) => format!("{} v{} (path)", version.key.name, version.version()),
        }
    }
}

/// The Rust versions candidates are scored against under resolver 3:
/// every member's `rust-version`, else the toolchain's (cargo's
/// `resolve_with_previous`). Empty under older resolvers.
fn workspace_rust_versions(
    workspace: &Workspace,
    rust_version: Option<&Version>,
) -> Vec<RustVersion> {
    match workspace.resolver() {
        ResolverVersion::V1 | ResolverVersion::V2 => Vec::new(),
        ResolverVersion::V3 => {
            let declared: Vec<RustVersion> = workspace
                .members()
                .filter_map(|member| member.summary().rust_version().copied())
                .collect();
            if declared.is_empty() {
                rust_version.map(RustVersion::from).into_iter().collect()
            } else {
                declared
            }
        }
    }
}

/// The candidates `dependency` accepts. A path dependency without a
/// `version` accepts its path package whatever the version, pre-releases
/// included, as cargo's unconstrained requirement does.
fn accepted(dependency: &Dependency) -> VersionSet {
    match dependency.source {
        DepSource::Path(_) if dependency.req.comparators.is_empty() => VersionSet::Any,
        DepSource::Path(_) | DepSource::Registry => VersionSet::Matches(dependency.req.clone()),
    }
}

/// The range a requirement matching no version points at: that of its
/// first comparator's lowest version.
fn missing_key(dependency: &Dependency) -> PackageKey {
    let compat = dependency
        .req
        .comparators
        .first()
        .map(|comparator| {
            let version = Version::new(
                comparator.major,
                comparator.minor.unwrap_or_default(),
                comparator.patch.unwrap_or_default(),
            );
            Compat::from(&version)
        })
        .unwrap_or(Compat::Patch(0));
    PackageKey {
        name: dependency.package.clone(),
        source: SourceKey::Registry,
        compat,
    }
}

impl Interner for Provider<'_> {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, solvable: SolvableId) -> impl Display + '_ {
        let (name, reference) = self.solvable(solvable);
        match name {
            SolverName::Links(links) => format!(
                "{} linking native library `{links}`",
                self.display_version_ref(reference)
            ),
            SolverName::Proxy { bucket, .. } => format!("{} {}", bucket.name, bucket.compat),
            SolverName::Package(_)
            | SolverName::Feature(..)
            | SolverName::DefaultFeatures(_)
            | SolverName::OptionalDep(..) => {
                format!(
                    "{} {}",
                    self.display_name(self.solvable_name(solvable)),
                    self.version(reference).version()
                )
            }
        }
    }

    fn display_merged_solvables(&self, solvables: &[SolvableId]) -> impl Display + '_ {
        let Some(&first) = solvables.first() else {
            return String::new();
        };
        let (name, _) = self.solvable(first);
        match name {
            SolverName::Links(_) | SolverName::Proxy { .. } => solvables
                .iter()
                .map(|&solvable| self.display_solvable(solvable).to_string())
                .collect::<Vec<_>>()
                .join(" | "),
            SolverName::Package(_)
            | SolverName::Feature(..)
            | SolverName::DefaultFeatures(_)
            | SolverName::OptionalDep(..) => {
                let mut versions: Vec<&Version> = solvables
                    .iter()
                    .map(|&solvable| self.version(self.solvable(solvable).1).version())
                    .collect();
                versions.sort();
                versions.dedup();
                let versions: Vec<String> = versions.iter().map(ToString::to_string).collect();
                format!(
                    "{} {}",
                    self.display_name(self.solvable_name(first)),
                    versions.join(" | ")
                )
            }
        }
    }

    fn display_name(&self, name: NameId) -> impl Display + '_ {
        match self.pool.resolve_package_name(name) {
            SolverName::Package(key) => format!("package `{}`", key.name),
            SolverName::Feature(key, feature) => {
                format!("package `{}` with feature `{feature}`", key.name)
            }
            SolverName::DefaultFeatures(key) => {
                format!("package `{}` with default features", key.name)
            }
            SolverName::OptionalDep(key, dep) => {
                format!("package `{}` with optional dependency `{dep}`", key.name)
            }
            SolverName::Links(links) => format!("native library `{links}`"),
            SolverName::Proxy {
                parent,
                entry,
                bucket,
            } => {
                let dependency = self
                    .version(*parent)
                    .dependencies()
                    .get(*entry)
                    .map(|dependency| dependency.name.to_string())
                    .unwrap_or_default();
                format!(
                    "dependency `{dependency}` of {} as `{}` {}",
                    self.display_version_ref(*parent),
                    bucket.name,
                    bucket.compat
                )
            }
        }
    }

    fn display_version_set(&self, version_set: VersionSetId) -> impl Display + '_ {
        match self.pool.resolve_version_set(version_set) {
            VersionSet::Matches(req) => req.to_string(),
            VersionSet::Exactly(reference) => format!("={}", self.version(*reference).version()),
            VersionSet::Any => String::new(),
        }
    }

    fn display_string(&self, string_id: StringId) -> impl Display + '_ {
        self.pool.resolve_string(string_id).to_owned()
    }

    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.pool.resolve_version_set_package_name(version_set)
    }

    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.pool.resolve_solvable(solvable).name
    }

    fn version_sets_in_union(
        &self,
        version_set_union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        self.pool.resolve_version_set_union(version_set_union)
    }

    fn resolve_condition(&self, condition: ConditionId) -> Condition {
        self.pool.resolve_condition(condition).clone()
    }
}

impl DependencyProvider for Provider<'_> {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let set = self.pool.resolve_version_set(version_set);
        candidates
            .iter()
            .copied()
            .filter(|&solvable| {
                let reference = self.pool.resolve_solvable(solvable).record;
                let contained = match set {
                    VersionSet::Matches(req) => req.matches(self.version(reference).version()),
                    VersionSet::Exactly(exact) => reference == *exact,
                    VersionSet::Any => true,
                };
                contained != inverse
            })
            .collect()
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        let solver_name = self.pool.resolve_package_name(name);
        // A range's package and a `links` value admit one candidate. A
        // feature, default-features or optional-dependency candidate
        // requires the package candidate of its version, so at most one is
        // selected anyway; leaving out their pairwise exclusion clauses
        // spares the solver most of its clauses. A proxy has one candidate
        // anyway, and is the condition of conditional requirements, which
        // resolvo 0.12 does not enforce when the condition's name allows
        // several candidates.
        let allow_multiple = match solver_name {
            SolverName::Package(_) | SolverName::Links(_) | SolverName::Proxy { .. } => false,
            SolverName::Feature(..)
            | SolverName::DefaultFeatures(_)
            | SolverName::OptionalDep(..) => true,
        };
        let mut candidates = Candidates {
            allow_multiple,
            ..Candidates::default()
        };
        for reference in self.candidates(solver_name) {
            let solvable = self.pool.intern_solvable(name, reference);
            candidates.candidates.push(solvable);
            let version = self.version(reference);
            let proxy = match solver_name {
                SolverName::Proxy { .. } => true,
                SolverName::Package(_)
                | SolverName::Feature(..)
                | SolverName::DefaultFeatures(_)
                | SolverName::OptionalDep(..)
                | SolverName::Links(_) => false,
            };
            // Yanked versions stay usable only when the previous lock file
            // pins them.
            if !proxy && version.yanked && !version.locked {
                candidates.excluded.push((solvable, self.yanked));
            }
        }
        Some(candidates)
    }

    async fn sort_candidates(&self, _solver: &SolverCache<Self>, solvables: &mut [SolvableId]) {
        solvables.sort_by(|&a, &b| {
            self.preference(
                self.pool.resolve_solvable(a).record,
                self.pool.resolve_solvable(b).record,
            )
        });
    }

    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        let (name, reference) = self.solvable(solvable);
        self.expanding.set(Some(reference));
        let requirements = match name {
            SolverName::Package(_) => match self.package_dependencies(reference) {
                Some(requirements) => requirements,
                None => return Dependencies::Unknown(self.pool.intern_string(REJECTED)),
            },
            SolverName::Feature(key, feature) => {
                match self.feature_dependencies(key, feature, reference) {
                    Ok(requirements) => requirements,
                    Err(reason) => return Dependencies::Unknown(self.pool.intern_string(reason)),
                }
            }
            SolverName::DefaultFeatures(key) => {
                let mut requirements =
                    vec![self.exactly(SolverName::Package(key.clone()), reference)];
                let default = FeatureName::default_feature();
                if self
                    .version(reference)
                    .summary()
                    .is_some_and(|summary| summary.features().contains_key(&default))
                {
                    requirements
                        .push(self.exactly(SolverName::Feature(key.clone(), default), reference));
                }
                requirements
            }
            SolverName::OptionalDep(key, dep) => {
                if !self
                    .version(reference)
                    .summary()
                    .is_some_and(|summary| summary.is_optional_dep(dep))
                {
                    return Dependencies::Unknown(self.pool.intern_string(MISSING_OPTIONAL_DEP));
                }
                let mut requirements =
                    vec![self.exactly(SolverName::Package(key.clone()), reference)];
                for entry in self.entries_named(reference, dep) {
                    self.add_entry(reference, entry, &mut requirements);
                }
                requirements
            }
            SolverName::Links(_) => Vec::new(),
            SolverName::Proxy {
                parent,
                entry,
                bucket,
            } => {
                let mut requirements = Vec::new();
                if let Some(dependency) = self.version(*parent).dependencies().get(*entry) {
                    self.add_bucket(dependency, bucket, &mut requirements);
                }
                requirements
            }
        };
        Dependencies::Known(KnownDependencies {
            requirements,
            constrains: Vec::new(),
        })
    }

    fn should_cancel_with_value(&self) -> Option<Box<dyn std::any::Any>> {
        if self.missing.borrow().is_empty() {
            None
        } else {
            Some(Box::new(()))
        }
    }
}
