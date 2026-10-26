//! The set of crates a resolution is expected to reach, grown to a fixed
//! point as index files arrive.
//!
//! A crate is fetched once, the first time anything requires it. The walk
//! follows the dependencies the solver is expected to activate: those of
//! the version it tries first for each requirement (the highest non-yanked
//! match of each semver-compatible range), with the features the
//! requirement enables. An optional dependency is followed only once an
//! enabled feature turns it on. Following every matching version with
//! every optional dependency instead loads several times the crates a
//! resolution touches. Versions a previous lock file pins are fetched but
//! not followed: the lock file lists what they reached. When the solver
//! reaches beyond this set, the resolution asks for the missing crates and
//! the walk grows again (see [`Index::require`](super::Index::require)).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque, hash_map::Entry};

use semver::{Version, VersionReq};

use super::IndexEntry;
use crate::{
    names::{Compat, DepName, FeatureName, PackageName},
    summary::{DepKind, DepSource, FeatureValue, Summary},
};

/// A requirement on a crate, as a dependency entry makes it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Request {
    req: VersionReq,
    features: BTreeSet<FeatureName>,
    default_features: bool,
}

/// One crate of the walk, keyed by its lowercased name as its index file
/// is.
#[derive(Default)]
struct CrateState {
    requests: HashSet<Request>,
    /// The exact names the crate's index file holds entries for, once it
    /// arrived.
    names: Option<Vec<PackageName>>,
    /// The features requested of each followed version, and whether its
    /// default features are.
    followed: HashMap<Version, (BTreeSet<FeatureName>, bool)>,
    /// Whether every matching version is followed, not only the ones
    /// tried first.
    all: bool,
}

/// The walk to a fixed point; see the module documentation.
#[derive(Default)]
pub(super) struct Walk {
    crates: HashMap<String, CrateState>,
    /// Every parsed entry, grouped by exact crate name, ascending.
    packages: BTreeMap<PackageName, Vec<IndexEntry>>,
    /// Crates required but not yet handed out by [`Walk::next_fetch`].
    to_fetch: VecDeque<PackageName>,
    /// Requests not yet recorded.
    pending: Vec<(PackageName, Request)>,
    /// Whether every crate follows all its matching versions; see
    /// [`Walk::follow_all_crates`].
    all: bool,
}

impl Walk {
    /// Requires the registry dependencies of a path package with every
    /// feature enabled, dev-dependencies included if `dev`.
    pub(super) fn require_dependencies_of(&mut self, summary: &Summary, dev: bool) {
        let features = summary.features().keys().cloned().collect();
        enqueue_dependencies(&mut self.pending, summary, &features, true, dev);
        self.settle();
    }

    /// Requires versions of `package` matching `req` with their default
    /// features.
    pub(super) fn require(&mut self, package: &PackageName, req: &VersionReq) {
        let request = Request {
            req: req.clone(),
            features: BTreeSet::new(),
            default_features: true,
        };
        self.pending.push((package.clone(), request));
        self.settle();
    }

    /// Fetches `package` without following any of its versions, as for a
    /// version a previous lock file pins.
    pub(super) fn fetch(&mut self, package: &PackageName) {
        self.state(package);
    }

    /// The next crate whose index file is needed, each crate once.
    pub(super) fn next_fetch(&mut self) -> Option<PackageName> {
        self.to_fetch.pop_front()
    }

    /// Takes in the parsed index file of `package`, as handed out by
    /// [`Walk::next_fetch`], and follows the versions its requests pick.
    pub(super) fn loaded(&mut self, package: &PackageName, entries: Vec<IndexEntry>) {
        let mut names: Vec<PackageName> = Vec::new();
        for entry in entries {
            let name = entry.name();
            if !names.contains(name) {
                names.push(name.clone());
            }
            self.packages.entry(name.clone()).or_default().push(entry);
        }
        let state = self.state(package);
        state.names = Some(names);
        let requests: Vec<Request> = state.requests.iter().cloned().collect();
        for request in &requests {
            self.follow(package, request);
        }
        self.settle();
    }

    /// Whether the index file of `package` arrived.
    pub(super) fn is_loaded(&self, package: &PackageName) -> bool {
        self.crates
            .get(&package.as_str().to_lowercase())
            .is_some_and(|state| state.names.is_some())
    }

    /// Every parsed entry, grouped by exact crate name, each group
    /// ascending by version.
    pub(super) fn packages(&self) -> &BTreeMap<PackageName, Vec<IndexEntry>> {
        &self.packages
    }

    /// Follows every non-yanked version of `package` that matches a
    /// request, not only the versions tried first: the solver went past
    /// those.
    pub(super) fn follow_all(&mut self, package: &PackageName) {
        let state = self.state(package);
        if state.all {
            return;
        }
        state.all = true;
        if state.names.is_some() {
            let requests: Vec<Request> = state.requests.iter().cloned().collect();
            for request in &requests {
                self.follow(package, request);
            }
        }
        self.settle();
    }

    /// Follows every non-yanked matching version of every crate, from now
    /// on, not only the versions tried first.
    pub(super) fn follow_all_crates(&mut self) {
        if self.all {
            return;
        }
        self.all = true;
        let loaded: Vec<(PackageName, Vec<Request>)> = self
            .crates
            .iter()
            .filter(|(_, state)| state.names.is_some())
            .map(|(key, state)| {
                (
                    PackageName::new(key.as_str()),
                    state.requests.iter().cloned().collect(),
                )
            })
            .collect();
        for (package, requests) in &loaded {
            for request in requests {
                self.follow(package, request);
            }
        }
        self.settle();
    }

    /// The state of `package`, queuing its index file on first sight.
    fn state(&mut self, package: &PackageName) -> &mut CrateState {
        match self.crates.entry(package.as_str().to_lowercase()) {
            Entry::Occupied(occupied) => occupied.into_mut(),
            Entry::Vacant(vacant) => {
                tracing::trace!(name = %package, "index file needed");
                self.to_fetch.push_back(package.clone());
                vacant.insert(CrateState::default())
            }
        }
    }

    /// Records pending requests until none is left, following the
    /// versions they newly pick.
    fn settle(&mut self) {
        while let Some((package, request)) = self.pending.pop() {
            let state = self.state(&package);
            if state.requests.insert(request.clone()) && state.names.is_some() {
                self.follow(&package, &request);
            }
        }
    }

    /// Follows the dependencies of the versions of the loaded crate
    /// `package` that `request` picks, with the features it enables.
    fn follow(&mut self, package: &PackageName, request: &Request) {
        let all = self.all;
        let Some(state) = self.crates.get_mut(&package.as_str().to_lowercase()) else {
            return;
        };
        for name in state.names.iter().flatten() {
            let entries = self.packages.get(name).map(Vec::as_slice).unwrap_or(&[]);
            let versions = if all || state.all {
                entries
                    .iter()
                    .filter(|entry| !entry.yanked && request.req.matches(entry.version()))
                    .collect()
            } else {
                picked(entries, &request.req)
            };
            for entry in versions {
                let version = entry.version();
                let new = !state.followed.contains_key(version);
                let (features, default_features) = state
                    .followed
                    .entry(version.clone())
                    .or_insert_with(|| (BTreeSet::new(), false));
                let before = (features.len(), *default_features);
                features.extend(request.features.iter().cloned());
                *default_features |= request.default_features;
                if (new || before != (features.len(), *default_features))
                    && let Some(summary) = entry.summary()
                {
                    enqueue_dependencies(
                        &mut self.pending,
                        summary,
                        features,
                        *default_features,
                        false,
                    );
                }
            }
        }
    }
}

/// The highest non-yanked version of each semver-compatible range that
/// matches `req`, out of a crate's ascending `entries`: what the solver
/// tries first.
fn picked<'a>(entries: &'a [IndexEntry], req: &VersionReq) -> Vec<&'a IndexEntry> {
    let mut ranges: Vec<Compat> = Vec::new();
    let mut picked = Vec::new();
    for entry in entries.iter().rev() {
        let version = entry.version();
        let range = Compat::from(version);
        if !entry.yanked && !ranges.contains(&range) && req.matches(version) {
            ranges.push(range);
            picked.push(entry);
        }
    }
    picked
}

/// Queues the registry dependencies `summary` activates with `features`
/// (and `default` if `default_features`) enabled, as cargo's resolver
/// activates them: normal and build dependencies, dev-dependencies if
/// `dev`, and optional ones a feature turns on, weak `d?/f` included.
fn enqueue_dependencies(
    pending: &mut Vec<(PackageName, Request)>,
    summary: &Summary,
    features: &BTreeSet<FeatureName>,
    default_features: bool,
    dev: bool,
) {
    let mut stack: Vec<FeatureName> = features.iter().cloned().collect();
    let default = FeatureName::default_feature();
    if default_features && summary.features().contains_key(&default) {
        stack.push(default);
    }
    let mut enabled: HashSet<FeatureName> = HashSet::new();
    let mut optional: HashSet<DepName> = HashSet::new();
    let mut dep_features: HashMap<DepName, BTreeSet<FeatureName>> = HashMap::new();
    while let Some(feature) = stack.pop() {
        if !enabled.insert(feature.clone()) {
            continue;
        }
        for value in summary.features().get(&feature).into_iter().flatten() {
            match value {
                FeatureValue::Feature(other) => stack.push(other.clone()),
                FeatureValue::Dep(dep) => {
                    optional.insert(dep.clone());
                }
                FeatureValue::DepFeature {
                    dep,
                    feature: dep_feature,
                    weak,
                } => {
                    optional.insert(dep.clone());
                    let implicit = FeatureName::new(dep.as_str());
                    if !weak && summary.features().contains_key(&implicit) {
                        stack.push(implicit);
                    }
                    dep_features
                        .entry(dep.clone())
                        .or_default()
                        .insert(dep_feature.clone());
                }
            }
        }
    }

    for dependency in summary.dependencies() {
        let kind = match dependency.kind {
            DepKind::Normal | DepKind::Build => true,
            DepKind::Dev => dev,
        };
        let registry = match dependency.source {
            DepSource::Registry => true,
            DepSource::Path(_) => false,
        };
        if !kind || !registry || dependency.optional && !optional.contains(&dependency.name) {
            continue;
        }
        let mut features: BTreeSet<FeatureName> = dependency.features.iter().cloned().collect();
        if let Some(extra) = dep_features.get(&dependency.name) {
            features.extend(extra.iter().cloned());
        }
        pending.push((
            dependency.package.clone(),
            Request {
                req: dependency.req.clone(),
                features,
                default_features: dependency.default_features,
            },
        ));
    }
}
