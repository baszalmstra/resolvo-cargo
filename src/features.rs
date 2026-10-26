//! Feature resolution: which features and optional dependencies are on for
//! each package of one build, cargo's second resolution pass
//! (`core/resolver/features.rs`).
//!
//! A lock file is resolved with every feature of every member, all
//! dev-dependencies and all platforms, so it fits every build. A build
//! narrows that down: to the packages and features asked for, to the
//! dependencies of the platforms built for, and, under resolver 2, keeps
//! the features of build scripts, proc macros and what they depend on
//! apart from those of the target build, and leaves dev-dependencies out
//! unless tests, benches or examples are built. Versions do not change:
//! the walk follows the lock file's edges.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    str::FromStr,
};

use cargo_platform::{Cfg, Platform};

use crate::{
    error::{FeatureError, TargetPlatformError},
    index::{Index, IndexSource},
    lockfile::{Lockfile, PackageId, PackageSource},
    names::{DepName, FeatureName, PackageName},
    summary::{DepKind, DepSource, Dependency, FeatureValue, Summary},
    workspace::{ResolverVersion, Workspace},
};

/// A platform a build compiles for: its target triple and the cfg values
/// `rustc --print cfg --target <triple>` prints for it.
#[derive(Clone, Debug)]
pub struct TargetPlatform {
    triple: String,
    cfgs: Vec<Cfg>,
}

impl TargetPlatform {
    /// The platform `triple` with the cfg values in `rustc_cfg`, the output
    /// of `rustc --print cfg --target <triple>`: one `name` or
    /// `name="value"` per line.
    ///
    /// # Errors
    ///
    /// [`TargetPlatformError`] for a line that is not a cfg.
    pub fn new(
        triple: impl Into<String>,
        rustc_cfg: &str,
    ) -> Result<TargetPlatform, TargetPlatformError> {
        let cfgs = rustc_cfg
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| {
                Cfg::from_str(line).map_err(|_| TargetPlatformError {
                    line: line.to_owned(),
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(TargetPlatform {
            triple: triple.into(),
            cfgs,
        })
    }

    /// The target triple.
    pub fn triple(&self) -> &str {
        &self.triple
    }

    fn matches(&self, platform: &Platform) -> bool {
        platform.matches(&self.triple, &self.cfgs)
    }
}

/// A workspace member a build compiles and the features asked of it, as
/// `cargo build -p <name> --features ... [--all-features]
/// [--no-default-features]` asks.
#[derive(Clone, Debug)]
pub struct RequestedPackage {
    /// The member.
    pub name: PackageName,
    /// Features to enable: `f` for a feature of the member, `d/f` for a
    /// feature of its dependency `d`.
    pub features: Vec<FeatureValue>,
    /// Enables every feature of the member.
    pub all_features: bool,
    /// Enables the member's `default` feature.
    pub default_features: bool,
}

impl RequestedPackage {
    /// The member `name` with its default features.
    pub fn new(name: PackageName) -> RequestedPackage {
        RequestedPackage {
            name,
            features: Vec::new(),
            all_features: false,
            default_features: true,
        }
    }
}

/// What one build compiles: the input of [`resolve_features`].
#[derive(Clone, Debug)]
pub struct FeatureRequest {
    /// The members built, with the features asked of each. Empty builds
    /// every member with its default features, as `cargo build
    /// --workspace` does.
    pub packages: Vec<RequestedPackage>,
    /// Whether tests, benches or examples are built, which need the
    /// members' dev-dependencies (cargo's `HasDevUnits`).
    pub dev_units: bool,
    /// The platforms built for, as `--target` gives them. Empty builds for
    /// the host.
    pub targets: Vec<TargetPlatform>,
    /// The platform build scripts and proc macros are built for and run on.
    pub host: TargetPlatform,
    /// The registry packages whose library is a proc macro. Index entries
    /// do not record it; the package's own `Cargo.toml` does. Path packages
    /// are read from their manifests ([`PathPackage::is_proc_macro`]).
    ///
    /// [`PathPackage::is_proc_macro`]: crate::PathPackage::is_proc_macro
    pub proc_macros: BTreeSet<PackageId>,
}

impl FeatureRequest {
    /// A build of every member with its default features, for `host`, with
    /// no dev units and no registry proc macros.
    pub fn new(host: TargetPlatform) -> FeatureRequest {
        FeatureRequest {
            packages: Vec::new(),
            dev_units: false,
            targets: Vec::new(),
            host,
            proc_macros: BTreeSet::new(),
        }
    }
}

/// Which feature set of a package a build uses. Under resolver 2 a
/// package built for a build script or proc macro gets features of its own;
/// under resolver 1 every package has one feature set, reported as
/// [`FeaturesFor::Target`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FeaturesFor {
    /// Built as a normal or dev-dependency, for the target platforms.
    Target,
    /// Built as a build-dependency, a proc macro or a dependency of either,
    /// for the host.
    Host,
}

/// A dependency a package is built with.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResolvedDependency {
    /// The key the dependency is known by in its dependent's manifest:
    /// the crate name its code uses.
    pub name: DepName,
    /// The locked package it resolves to.
    pub package: PackageId,
    /// Which dependency table it is in. Dev-dependencies are only listed
    /// when [`FeatureRequest::dev_units`] is set.
    pub kind: DepKind,
    /// Which feature set of `package` it is built with.
    pub features_for: FeaturesFor,
}

/// The features one build enables on each package, and the dependencies
/// each package is built with: what [`resolve_features`] gives.
#[derive(Clone, Debug, Default)]
pub struct ResolvedFeatures {
    /// Whether host builds keep features of their own (resolver 2).
    decouple_host: bool,
    units: BTreeMap<(PackageId, FeaturesFor), Unit>,
}

#[derive(Clone, Debug, Default)]
struct Unit {
    features: BTreeSet<FeatureName>,
    dependencies: Vec<ResolvedDependency>,
}

impl ResolvedFeatures {
    /// Every package the build compiles, with the feature set it is built
    /// with, sorted.
    pub fn packages(&self) -> impl Iterator<Item = (&PackageId, FeaturesFor)> {
        self.units
            .keys()
            .map(|(package, features_for)| (package, *features_for))
    }

    /// The features enabled on `package` built for `features_for`; `None`
    /// when the build does not compile it so.
    pub fn features(
        &self,
        package: &PackageId,
        features_for: FeaturesFor,
    ) -> Option<&BTreeSet<FeatureName>> {
        self.unit(package, features_for).map(|unit| &unit.features)
    }

    /// The dependencies `package` built for `features_for` is compiled
    /// with: its non-optional dependencies and the optional ones its
    /// features enable, of the platforms built for. Empty when the build
    /// does not compile it so.
    pub fn dependencies(
        &self,
        package: &PackageId,
        features_for: FeaturesFor,
    ) -> &[ResolvedDependency] {
        self.unit(package, features_for)
            .map(|unit| unit.dependencies.as_slice())
            .unwrap_or_default()
    }

    fn unit(&self, package: &PackageId, features_for: FeaturesFor) -> Option<&Unit> {
        let key = if self.decouple_host {
            features_for
        } else {
            FeaturesFor::Target
        };
        self.units.get(&(package.clone(), key))
    }
}

/// Resolves which features one build enables on each package of
/// `lockfile`, and which dependencies each package is built with, as
/// cargo's feature resolver does for `cargo build`. Index entries of the
/// locked registry versions come from `source`.
///
/// # Errors
///
/// [`FeatureError`] when a requested package is not a member, a requested
/// feature does not exist, the lock file lacks a workspace package or has
/// versions the index does not, or index files cannot be loaded.
pub async fn resolve_features(
    workspace: &Workspace,
    source: &IndexSource,
    lockfile: &Lockfile,
    request: &FeatureRequest,
) -> Result<ResolvedFeatures, FeatureError> {
    let registry = lockfile
        .packages()
        .iter()
        .filter(|package| package.id.source == PackageSource::Registry)
        .map(|package| &package.id.name);
    let index = Index::load_crates(source, registry).await?;
    let graph = Graph::new(workspace, &index, lockfile, request)?;
    let roots = roots(workspace, &graph, request)?;

    let opts = match workspace.resolver() {
        ResolverVersion::V1 => Options {
            decouple_host: false,
            decouple_dev: false,
            ignore_inactive_targets: false,
        },
        ResolverVersion::V2 | ResolverVersion::V3 => Options {
            decouple_host: true,
            decouple_dev: !request.dev_units,
            ignore_inactive_targets: true,
        },
    };
    let mut resolver = Resolver {
        graph: &graph,
        request,
        opts,
        track_for_host: opts.decouple_host || opts.ignore_inactive_targets,
        features: HashMap::new(),
        dependencies: HashMap::new(),
        processed: HashSet::new(),
        deferred: HashMap::new(),
    };
    let mut built = Vec::new();
    for (node, values) in roots {
        // A proc macro member is built for the host, and for the target
        // too for its tests and other targets.
        if graph.nodes[node].proc_macro {
            built.push((node, FeaturesFor::Host));
            if resolver.track_for_host {
                resolver.activate_package(node, FeaturesFor::Target, &values);
            }
        }
        built.push((node, FeaturesFor::Target));
        let features_for = if resolver.track_for_host && graph.nodes[node].proc_macro {
            FeaturesFor::Host
        } else {
            FeaturesFor::Target
        };
        resolver.activate_package(node, features_for, &values);
    }
    Ok(resolver.finish(&built))
}

/// A locked package with what feature resolution needs.
struct Node<'a> {
    id: PackageId,
    summary: &'a Summary,
    member: bool,
    proc_macro: bool,
    /// Each dependency entry resolution includes, with the locked package
    /// it resolves to.
    edges: Vec<(&'a Dependency, usize)>,
}

/// The lock file's packages and edges, each edge with the dependency
/// entry it stands for.
struct Graph<'a> {
    nodes: Vec<Node<'a>>,
    by_id: HashMap<&'a PackageId, usize>,
}

impl<'a> Graph<'a> {
    fn new(
        workspace: &'a Workspace,
        index: &'a Index,
        lockfile: &'a Lockfile,
        request: &FeatureRequest,
    ) -> Result<Graph<'a>, FeatureError> {
        for package in workspace.packages() {
            let summary = package.summary();
            let locked = lockfile.packages().iter().any(|locked| {
                locked.id.source == PackageSource::Path
                    && locked.id.name == *summary.name()
                    && locked.id.version == *summary.version()
            });
            if !locked {
                return Err(FeatureError::NotLocked {
                    package: summary.name().clone(),
                    version: summary.version().clone(),
                });
            }
        }

        let mut nodes = Vec::new();
        let mut by_id = HashMap::new();
        for locked in lockfile.packages() {
            let id = &locked.id;
            let node = match id.source {
                PackageSource::Path => {
                    // A path package the workspace no longer has; nothing
                    // the workspace builds reaches it.
                    let Some(package) = workspace.packages().iter().find(|package| {
                        package.summary().name() == &id.name
                            && package.summary().version() == &id.version
                    }) else {
                        continue;
                    };
                    Node {
                        id: id.clone(),
                        summary: package.summary(),
                        member: package.is_member(),
                        proc_macro: package.is_proc_macro(),
                        edges: Vec::new(),
                    }
                }
                PackageSource::Registry => {
                    let summary = index
                        .entry(&id.name, &id.version)
                        .and_then(|entry| entry.summary())
                        .ok_or_else(|| FeatureError::NotInIndex {
                            package: id.name.clone(),
                            version: id.version.clone(),
                        })?;
                    Node {
                        id: id.clone(),
                        summary,
                        member: false,
                        proc_macro: request.proc_macros.contains(id),
                        edges: Vec::new(),
                    }
                }
            };
            by_id.insert(id, nodes.len());
            nodes.push(node);
        }

        // Each entry resolves to the first locked dependency it accepts,
        // as cargo locks dependencies to a previous resolve.
        for locked in lockfile.packages() {
            let Some(&node) = by_id.get(&locked.id) else {
                continue;
            };
            let member = nodes[node].member;
            let mut edges = Vec::new();
            for dependency in nodes[node].summary.dependencies() {
                // Only members' dev-dependencies are resolved.
                if dependency.kind == DepKind::Dev && !member {
                    continue;
                }
                let target = locked.dependencies.iter().find(|candidate| {
                    candidate.name == dependency.package
                        && match (&dependency.source, candidate.source) {
                            (DepSource::Path(_), PackageSource::Path)
                            | (DepSource::Registry, PackageSource::Registry) => true,
                            (DepSource::Path(_), PackageSource::Registry)
                            | (DepSource::Registry, PackageSource::Path) => false,
                        }
                        && dependency.accepts(&candidate.version)
                });
                match target.and_then(|target| by_id.get(target)) {
                    Some(&target) => edges.push((dependency, target)),
                    // An optional dependency nothing enabled when the lock
                    // file was resolved.
                    None => tracing::trace!(
                        package = %locked.id.name,
                        dependency = %dependency.name,
                        "dependency entry without a locked package"
                    ),
                }
            }
            nodes[node].edges = edges;
        }
        Ok(Graph { nodes, by_id })
    }
}

/// The members a request builds, with the feature values asked of each.
fn roots(
    workspace: &Workspace,
    graph: &Graph<'_>,
    request: &FeatureRequest,
) -> Result<Vec<(usize, Vec<FeatureValue>)>, FeatureError> {
    let requested: Vec<RequestedPackage> = if request.packages.is_empty() {
        workspace
            .members()
            .map(|member| RequestedPackage::new(member.summary().name().clone()))
            .collect()
    } else {
        request.packages.clone()
    };
    let mut roots = Vec::new();
    for package in requested {
        let Some(member) = workspace
            .members()
            .find(|member| *member.summary().name() == package.name)
        else {
            return Err(FeatureError::UnknownMember {
                package: package.name,
            });
        };
        let summary = member.summary();
        let id = PackageId {
            name: summary.name().clone(),
            version: summary.version().clone(),
            source: PackageSource::Path,
        };
        let Some(&node) = graph.by_id.get(&id) else {
            return Err(FeatureError::NotLocked {
                package: id.name,
                version: id.version,
            });
        };
        let mut values = package.features.clone();
        for value in &values {
            validate(graph, node, value)?;
        }
        let default = FeatureName::default_feature();
        if package.default_features && summary.features().contains_key(&default) {
            values.push(FeatureValue::Feature(default));
        }
        if package.all_features {
            values.extend(
                summary
                    .features()
                    .keys()
                    .cloned()
                    .map(FeatureValue::Feature),
            );
        }
        roots.push((node, values));
    }
    Ok(roots)
}

/// Checks that a feature value asked of a member names what exists: its
/// feature, its dependency, or a feature of that dependency, as cargo
/// rejects requests that do not.
fn validate(graph: &Graph<'_>, node: usize, value: &FeatureValue) -> Result<(), FeatureError> {
    let member = &graph.nodes[node];
    let dependency_targets = |dep: &DepName| -> Vec<usize> {
        member
            .edges
            .iter()
            .filter(|(dependency, _)| dependency.name == *dep)
            .map(|&(_, target)| target)
            .collect()
    };
    match value {
        FeatureValue::Feature(feature) => {
            if member.summary.features().contains_key(feature) {
                return Ok(());
            }
            Err(FeatureError::MissingFeature {
                package: member.id.name.clone(),
                feature: feature.clone(),
            })
        }
        FeatureValue::Dep(dep) => {
            if !dependency_targets(dep).is_empty() {
                return Ok(());
            }
            Err(FeatureError::MissingDependency {
                package: member.id.name.clone(),
                dependency: dep.clone(),
            })
        }
        FeatureValue::DepFeature { dep, feature, .. } => {
            let targets = dependency_targets(dep);
            let Some(&first) = targets.first() else {
                return Err(FeatureError::MissingDependency {
                    package: member.id.name.clone(),
                    dependency: dep.clone(),
                });
            };
            if targets
                .iter()
                .any(|&target| graph.nodes[target].summary.features().contains_key(feature))
            {
                return Ok(());
            }
            Err(FeatureError::MissingFeature {
                package: graph.nodes[first].id.name.clone(),
                feature: feature.clone(),
            })
        }
    }
}

/// cargo's `FeatureOpts`.
#[derive(Clone, Copy)]
struct Options {
    /// Build-dependencies and proc macros keep features of their own.
    decouple_host: bool,
    /// Dev-dependencies are left out.
    decouple_dev: bool,
    /// Dependencies of platforms not built for are left out.
    ignore_inactive_targets: bool,
}

/// A package version built for one feature set: an index into
/// [`Graph::nodes`] and the feature set.
type Key = (usize, FeaturesFor);

/// cargo's `FeatureResolver`.
struct Resolver<'a> {
    graph: &'a Graph<'a>,
    request: &'a FeatureRequest,
    opts: Options,
    /// Whether the walk tells host builds apart at all, for decoupling or
    /// for platform filtering.
    track_for_host: bool,
    /// Enabled features, by the key as stored ([`Resolver::stored`]).
    features: HashMap<Key, BTreeSet<FeatureName>>,
    /// Enabled optional dependencies, by the key as stored.
    dependencies: HashMap<Key, BTreeSet<DepName>>,
    /// Packages whose dependencies were walked, by the key as walked.
    processed: HashSet<Key>,
    /// `d?/f` features waiting for `d` to be enabled: the package, its
    /// feature set as walked, and `d`.
    deferred: HashMap<(usize, FeaturesFor, DepName), BTreeSet<FeatureName>>,
}

impl<'a> Resolver<'a> {
    /// The key a feature set is stored under: one set per package without
    /// host decoupling.
    fn stored(&self, node: usize, features_for: FeaturesFor) -> Key {
        if self.opts.decouple_host {
            (node, features_for)
        } else {
            (node, FeaturesFor::Target)
        }
    }

    /// Enables `values` on a package and walks its non-optional
    /// dependencies, once per feature set.
    fn activate_package(
        &mut self,
        node: usize,
        features_for: FeaturesFor,
        values: &[FeatureValue],
    ) {
        let key = self.stored(node, features_for);
        self.features.entry(key).or_default();
        for value in values {
            self.activate_value(node, features_for, value);
        }
        if !self.processed.insert((node, features_for)) {
            // A feature enabled later is followed when it is enabled.
            return;
        }
        for (dependency, target, target_for) in self.edges(node, features_for, Walk::Features) {
            if dependency.optional {
                continue;
            }
            let values = self.dependency_values(target, dependency);
            self.activate_package(target, target_for, &values);
        }
    }

    fn activate_value(&mut self, node: usize, features_for: FeaturesFor, value: &FeatureValue) {
        match value {
            FeatureValue::Feature(feature) => self.activate_feature(node, features_for, feature),
            FeatureValue::Dep(dep) => self.activate_dependency(node, features_for, dep),
            FeatureValue::DepFeature { dep, feature, weak } => {
                self.activate_dep_feature(node, features_for, dep, feature, *weak);
            }
        }
    }

    /// Enables `feature` and what it enables in turn.
    fn activate_feature(&mut self, node: usize, features_for: FeaturesFor, feature: &FeatureName) {
        let key = self.stored(node, features_for);
        if !self
            .features
            .entry(key)
            .or_default()
            .insert(feature.clone())
        {
            return;
        }
        let summary = self.graph.nodes[node].summary;
        let Some(values) = summary.features().get(feature) else {
            return;
        };
        for value in values {
            self.activate_value(node, features_for, value);
        }
    }

    /// Enables the optional dependency `dep` (`dep:d`), with the `d?/f`
    /// features waiting for it.
    fn activate_dependency(&mut self, node: usize, features_for: FeaturesFor, dep: &DepName) {
        let key = self.stored(node, features_for);
        self.dependencies
            .entry(key)
            .or_default()
            .insert(dep.clone());
        let deferred = self.deferred.remove(&(node, features_for, dep.clone()));
        for (dependency, target, target_for) in self.edges(node, features_for, Walk::Features) {
            if dependency.name != *dep {
                continue;
            }
            for feature in deferred.iter().flatten() {
                let value = FeatureValue::parse(feature.as_str());
                self.activate_value(target, target_for, &value);
            }
            let values = self.dependency_values(target, dependency);
            self.activate_package(target, target_for, &values);
        }
    }

    /// Enables `feature` on dependency `dep` (`d/f`, or `d?/f` if `weak`).
    fn activate_dep_feature(
        &mut self,
        node: usize,
        features_for: FeaturesFor,
        dep: &DepName,
        feature: &FeatureName,
        weak: bool,
    ) {
        for (dependency, target, target_for) in self.edges(node, features_for, Walk::Features) {
            if dependency.name != *dep {
                continue;
            }
            if dependency.optional {
                let key = self.stored(node, features_for);
                let enabled = self
                    .dependencies
                    .get(&key)
                    .is_some_and(|enabled| enabled.contains(dep));
                if weak && !enabled {
                    self.deferred
                        .entry((node, features_for, dep.clone()))
                        .or_default()
                        .insert(feature.clone());
                    continue;
                }
                self.activate_value(node, features_for, &FeatureValue::Dep(dep.clone()));
                // `d/f` also enables the feature `d`, unless `dep:d`
                // hides it.
                let implicit = FeatureName::new(dep.as_str());
                if !weak
                    && self.graph.nodes[node]
                        .summary
                        .features()
                        .contains_key(&implicit)
                {
                    self.activate_feature(node, features_for, &implicit);
                }
            }
            let value = FeatureValue::parse(feature.as_str());
            self.activate_value(target, target_for, &value);
        }
    }

    /// The features a dependency entry enables on its package.
    fn dependency_values(&self, target: usize, dependency: &Dependency) -> Vec<FeatureValue> {
        let mut values: Vec<FeatureValue> = dependency
            .features
            .iter()
            .map(|feature| FeatureValue::parse(feature.as_str()))
            .collect();
        let default = FeatureName::default_feature();
        if dependency.default_features
            && self.graph.nodes[target]
                .summary
                .features()
                .contains_key(&default)
        {
            values.push(FeatureValue::Feature(default));
        }
        values
    }

    /// The dependency entries of a package built for `features_for` that
    /// `walk` follows, each with its package and the feature set it is
    /// built with.
    fn edges(
        &self,
        node: usize,
        features_for: FeaturesFor,
        walk: Walk,
    ) -> Vec<(&'a Dependency, usize, FeaturesFor)> {
        let (filter_platforms, skip_dev, track_for_host) = match walk {
            Walk::Features => (
                self.opts.ignore_inactive_targets,
                self.opts.decouple_dev,
                self.track_for_host,
            ),
            Walk::Build => (true, !self.request.dev_units, true),
        };
        let graph = self.graph;
        let mut edges = Vec::new();
        for &(dependency, target) in &graph.nodes[node].edges {
            if filter_platforms
                && let Some(platform) = &dependency.target
                && !self.platform_active(platform, dependency, features_for)
            {
                continue;
            }
            if skip_dev && dependency.kind == DepKind::Dev {
                continue;
            }
            let target_for = if features_for != FeaturesFor::Host
                && track_for_host
                && (dependency.kind == DepKind::Build || graph.nodes[target].proc_macro)
            {
                FeaturesFor::Host
            } else {
                features_for
            };
            edges.push((dependency, target, target_for));
        }
        edges
    }

    /// Whether a dependency limited to `platform` is built: build
    /// dependencies and host builds for the host, others for any target.
    fn platform_active(
        &self,
        platform: &str,
        dependency: &Dependency,
        features_for: FeaturesFor,
    ) -> bool {
        let Ok(platform) = Platform::from_str(platform) else {
            tracing::debug!(platform, "dependency platform does not parse; left out");
            return false;
        };
        if dependency.kind == DepKind::Build || features_for == FeaturesFor::Host {
            return self.request.host.matches(&platform);
        }
        if self.request.targets.is_empty() {
            self.request.host.matches(&platform)
        } else {
            self.request
                .targets
                .iter()
                .any(|target| target.matches(&platform))
        }
    }

    /// The packages the build compiles, reached from `roots` over the
    /// dependencies of the platforms built for, each with its enabled
    /// features and the dependencies it is built with. Under resolver 1
    /// features are unified over every platform, but only the packages of
    /// the platforms built for are compiled.
    fn finish(self, roots: &[(usize, FeaturesFor)]) -> ResolvedFeatures {
        let mut units: BTreeMap<(PackageId, FeaturesFor), Unit> = BTreeMap::new();
        let mut visited: HashSet<(usize, FeaturesFor)> = roots.iter().copied().collect();
        let mut queue: Vec<(usize, FeaturesFor)> = roots.to_vec();
        while let Some((node, built_for)) = queue.pop() {
            let key = self.stored(node, built_for);
            let enabled = self.dependencies.get(&key);
            let unit = units
                .entry((self.graph.nodes[node].id.clone(), key.1))
                .or_insert_with(|| Unit {
                    features: self.features.get(&key).cloned().unwrap_or_default(),
                    dependencies: Vec::new(),
                });
            for (dependency, target, target_for) in self.edges(node, built_for, Walk::Build) {
                if dependency.optional
                    && !enabled.is_some_and(|enabled| enabled.contains(&dependency.name))
                {
                    continue;
                }
                unit.dependencies.push(ResolvedDependency {
                    name: dependency.name.clone(),
                    package: self.graph.nodes[target].id.clone(),
                    kind: dependency.kind,
                    features_for: self.stored(target, target_for).1,
                });
                if visited.insert((target, target_for)) {
                    queue.push((target, target_for));
                }
            }
        }
        for unit in units.values_mut() {
            unit.dependencies.sort();
            unit.dependencies.dedup();
        }
        ResolvedFeatures {
            decouple_host: self.opts.decouple_host,
            units,
        }
    }
}

/// Which walk over the dependency graph [`Resolver::edges`] serves.
#[derive(Clone, Copy)]
enum Walk {
    /// Enabling features, as the resolver version decides: under resolver
    /// 1 over every platform, dev-dependency and build kind at once.
    Features,
    /// Collecting what is compiled: only the platforms built for, and
    /// dev-dependencies only for dev units.
    Build,
}
