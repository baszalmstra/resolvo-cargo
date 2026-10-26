//! Running the solver and turning its selection into a lock file: the
//! dependency edges between selected packages, cargo's cycle check, and
//! the `Cargo.lock` encoding.

use std::collections::{BTreeMap, HashMap, HashSet};

use petgraph::{Direction, visit::EdgeRef};
use resolvo::{
    Interner, Problem, Requirement, Solver, UnsolvableOrCancelled,
    conflict::{Conflict, ConflictCause, ConflictEdge, ConflictNode},
};
use semver::Version;

use crate::{
    error::ResolveError,
    index::{Index, IndexRequest, IndexSource},
    lockfile::{LockedPackage, Lockfile, LockfileVersion, PackageId, PackageSource},
    names::{DepName, FeatureName, PackageName},
    solver::{EntryTarget, MISSING_FEATURE, PackageKey, Provider, SolverName, VersionRef, YANKED},
    summary::{DepKind, Dependency},
    workspace::Workspace,
};

/// How to resolve.
#[derive(Clone, Debug, Default)]
pub struct ResolveOptions {
    /// The previous lock file. Its versions are tried first, so an
    /// unchanged workspace resolves to the same lock file and a changed one
    /// changes as little as possible; its yanked versions stay usable; its
    /// format version and extra header comments are kept.
    pub previous: Option<Lockfile>,
    /// The version of the toolchain that will build the workspace. Under
    /// resolver 3, versions whose `rust-version` the workspace supports are
    /// preferred; this version stands in for the members' `rust-version`
    /// when none declares one.
    pub rust_version: Option<Version>,
}

/// Resolves the versions of every package `workspace` reaches, with all
/// features and dev-dependencies of every member, as cargo resolves a lock
/// file, fetching index entries from `source` as needed.
///
/// Index files are fetched before each solve, so solving itself is
/// synchronous and deterministic. The fetch follows the dependencies the
/// solver is expected to activate; when a solve reaches further, the
/// missing crates are fetched and the solve starts over. The last solve
/// saw only fetched crates, so the result is the one a complete index
/// gives.
///
/// # Errors
///
/// [`ResolveError`] when index files cannot be loaded, no selection
/// satisfies every requirement, the selection has a dependency cycle
/// outside dev-dependencies, or the index's checksum of a version differs
/// from the previous lock file's.
pub async fn resolve(
    workspace: &Workspace,
    source: &IndexSource,
    options: &ResolveOptions,
) -> Result<Lockfile, ResolveError> {
    let mut index = Index::load(source, workspace, options.previous.as_ref()).await?;
    loop {
        match solve(workspace, &index, options)? {
            Solved::Resolved(lockfile) => return Ok(lockfile),
            Solved::Incomplete(requests) => {
                tracing::debug!(crates = requests.len(), "loading more of the index");
                index.require(source, &requests).await?;
            }
        }
    }
}

/// What one solve against the loaded index gives.
enum Solved {
    /// The lock file.
    Resolved(Lockfile),
    /// The solver reached crates the index has not loaded, so its result
    /// does not hold: load these and solve again.
    Incomplete(Vec<IndexRequest>),
}

/// One solve against the index entries loaded so far.
fn solve(
    workspace: &Workspace,
    index: &Index,
    options: &ResolveOptions,
) -> Result<Solved, ResolveError> {
    let provider = Provider::new(
        workspace,
        index,
        options.previous.as_ref(),
        options.rust_version.as_ref(),
    );
    let requirements = provider.root_requirements();
    // The previous lock file acts through candidate order alone. As soft
    // requirements its versions would also be selected when nothing needs
    // them, and their requirements could enable features of packages that
    // are needed, adding optional dependencies cargo leaves out.
    let mut solver = Solver::new(provider);
    let solved = solver.solve(Problem::new().requirements(requirements));
    let requests = solver.provider().missing();
    if !requests.is_empty() {
        return Ok(Solved::Incomplete(requests));
    }
    let solution = match solved {
        Ok(solution) => solution,
        Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
            return Err(explain(&solver, &conflict));
        }
        Err(UnsolvableOrCancelled::Cancelled(_)) => {
            return Err(ResolveError::Conflict {
                message: "the solver was cancelled".to_owned(),
            });
        }
    };
    let provider = solver.provider();

    let mut selection = Selection::default();
    for solvable in solution {
        let (name, reference) = provider.solvable(solvable);
        match name {
            SolverName::Package(key) => {
                selection.packages.insert(key.clone(), reference);
            }
            SolverName::OptionalDep(_, dep) => {
                selection.optional.insert((reference, dep.clone()));
            }
            SolverName::Proxy {
                parent,
                entry,
                bucket,
            } => selection
                .proxies
                .entry((*parent, *entry))
                .or_default()
                .push(bucket.clone()),
            SolverName::Feature(..) | SolverName::DefaultFeatures(_) | SolverName::Links(_) => {}
        }
    }

    let previous_edges: HashMap<&PackageId, &[PackageId]> = options
        .previous
        .iter()
        .flat_map(|lockfile| lockfile.packages())
        .map(|package| (&package.id, package.dependencies.as_slice()))
        .collect();

    // Walk from the members, so only packages a member reaches are locked.
    let mut edges: BTreeMap<VersionRef, BTreeMap<VersionRef, bool>> = BTreeMap::new();
    let mut queue: Vec<VersionRef> = provider.members().to_vec();
    while let Some(reference) = queue.pop() {
        if edges.contains_key(&reference) {
            continue;
        }
        let mut targets: BTreeMap<VersionRef, bool> = BTreeMap::new();
        let version = provider.version(reference);
        for (entry, dependency) in version.dependencies().iter().enumerate() {
            if !version.includes(dependency)
                || dependency.optional
                    && !selection
                        .optional
                        .contains(&(reference, dependency.name.clone()))
            {
                continue;
            }
            let target = selection.target(provider, &previous_edges, reference, entry, dependency);
            match target {
                Some(target) => {
                    *targets.entry(target).or_default() |= dependency.kind != DepKind::Dev;
                    queue.push(target);
                }
                None => tracing::error!(
                    package = %version.key.name,
                    version = %version.version(),
                    dependency = %dependency.name,
                    "no selected package satisfies an included dependency"
                ),
            }
        }
        edges.insert(reference, targets);
    }

    check_cycles(provider, &edges)?;

    let mut packages = Vec::new();
    for (&reference, targets) in &edges {
        let version = provider.version(reference);
        packages.push(LockedPackage {
            id: version.locked_id(),
            checksum: version.checksum.map(str::to_owned),
            dependencies: targets
                .keys()
                .map(|&target| provider.version(target).locked_id())
                .collect(),
        });
    }
    check_checksums(&packages, options.previous.as_ref())?;

    let lowest_rust_version = workspace
        .members()
        .filter_map(|member| member.summary().rust_version())
        .min();
    let default_version = LockfileVersion::for_rust_version(lowest_rust_version);
    let lockfile = match &options.previous {
        Some(previous) => Lockfile::new(
            previous.version().max(default_version),
            previous.header().to_vec(),
            previous.metadata().clone(),
            packages,
        ),
        None => Lockfile::new(default_version, Vec::new(), BTreeMap::new(), packages),
    };
    Ok(Solved::Resolved(lockfile))
}

/// What the solver selected, by kind of solver name.
#[derive(Default)]
struct Selection {
    packages: HashMap<PackageKey, VersionRef>,
    /// Enabled optional dependencies: the package version and the key.
    optional: HashSet<(VersionRef, DepName)>,
    /// For dependency entries spanning several ranges, the ranges they
    /// resolved to.
    proxies: HashMap<(VersionRef, usize), Vec<PackageKey>>,
}

impl Selection {
    /// The selected package dependency `entry` of `parent` points to. When
    /// several selected packages satisfy it, the previous lock file's edge
    /// wins, then cargo's candidate order (`Provider::preference`), which
    /// is the order in which cargo tries candidates and so the one it
    /// activates for the dependency.
    fn target(
        &self,
        provider: &Provider<'_>,
        previous_edges: &HashMap<&PackageId, &[PackageId]>,
        parent: VersionRef,
        entry: usize,
        dependency: &Dependency,
    ) -> Option<VersionRef> {
        let EntryTarget::Buckets(buckets) = provider.entry_target(dependency) else {
            return None;
        };
        let resolved_to = self.proxies.get(&(parent, entry));
        let candidates: Vec<VersionRef> = buckets
            .iter()
            .filter(|bucket| {
                buckets.len() == 1 || resolved_to.is_none_or(|resolved| resolved.contains(bucket))
            })
            .filter_map(|bucket| self.packages.get(bucket).copied())
            .filter(|&target| dependency.accepts(provider.version(target).version()))
            .collect();
        let previous = previous_edges.get(&provider.version(parent).locked_id());
        if let Some(previous) = previous
            && let Some(&target) = candidates
                .iter()
                .find(|&&target| previous.contains(&provider.version(target).locked_id()))
        {
            return Some(target);
        }
        candidates
            .into_iter()
            .min_by(|&a, &b| provider.preference(a, b))
    }
}

/// cargo's `check_cycles`: a cycle of edges that are not only
/// dev-dependencies is an error.
fn check_cycles(
    provider: &Provider<'_>,
    edges: &BTreeMap<VersionRef, BTreeMap<VersionRef, bool>>,
) -> Result<(), ResolveError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        OnPath,
        Done,
    }
    // Visit packages and their dependencies in lock file order, as cargo
    // iterates its resolve graph, so the reported cycle starts where
    // cargo's does.
    let order = |a: &VersionRef, b: &VersionRef| {
        provider
            .version(*a)
            .locked_id()
            .cmp(&provider.version(*b).locked_id())
    };
    let mut starts: Vec<VersionRef> = edges.keys().copied().collect();
    starts.sort_by(order);
    // The targets to visit, last first since the search pops them.
    let targets_of = |reference: VersionRef| -> Vec<VersionRef> {
        let mut targets: Vec<VersionRef> = edges
            .get(&reference)
            .into_iter()
            .flatten()
            .filter(|(_, transitive)| **transitive)
            .map(|(&target, _)| target)
            .collect();
        targets.sort_by(|a, b| order(b, a));
        targets
    };
    let mut marks: HashMap<VersionRef, Mark> = HashMap::new();
    for start in starts {
        if marks.contains_key(&start) {
            continue;
        }
        // Iterative depth-first search; `path` holds the packages being
        // visited with the transitive targets still to visit.
        let mut path: Vec<(VersionRef, Vec<VersionRef>)> = vec![(start, targets_of(start))];
        marks.insert(start, Mark::OnPath);
        while let Some((_, remaining)) = path.last_mut() {
            match remaining.pop() {
                Some(next) => match marks.get(&next) {
                    Some(Mark::OnPath) => {
                        let mut packages: Vec<(PackageName, Version)> = path
                            .iter()
                            .map(|(reference, _)| *reference)
                            .skip_while(|&reference| reference != next)
                            .map(|reference| {
                                let version = provider.version(reference);
                                (version.key.name.clone(), version.version().clone())
                            })
                            .collect();
                        let version = provider.version(next);
                        packages.push((version.key.name.clone(), version.version().clone()));
                        return Err(ResolveError::Cycle { packages });
                    }
                    Some(Mark::Done) => {}
                    None => {
                        marks.insert(next, Mark::OnPath);
                        path.push((next, targets_of(next)));
                    }
                },
                None => {
                    if let Some((done, _)) = path.pop() {
                        marks.insert(done, Mark::Done);
                    }
                }
            }
        }
    }
    Ok(())
}

/// cargo's `Resolve::merge_from`: a registry version's checksum may not
/// change between lock files.
fn check_checksums(
    packages: &[LockedPackage],
    previous: Option<&Lockfile>,
) -> Result<(), ResolveError> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let previous: HashMap<&PackageId, Option<&str>> = previous
        .packages()
        .iter()
        .map(|package| (&package.id, package.checksum.as_deref()))
        .collect();
    for package in packages {
        if package.id.source == PackageSource::Path {
            continue;
        }
        if let Some(&Some(locked)) = previous.get(&package.id)
            && package.checksum.as_deref() != Some(locked)
        {
            return Err(ResolveError::ChecksumChanged {
                package: package.id.name.clone(),
                version: package.id.version.clone(),
            });
        }
    }
    Ok(())
}

/// Turns a resolvo conflict into the error variant naming its cause, with
/// resolvo's explanation in cargo terms.
fn explain(solver: &Solver<Provider<'_>>, conflict: &Conflict) -> ResolveError {
    let provider = solver.provider();
    let message = conflict.display_user_friendly(solver).to_string();
    let graph = conflict.graph(solver);
    let name_of_set = |set| {
        provider
            .pool()
            .resolve_package_name(provider.version_set_name(set))
    };

    if let Some(unresolved) = graph.unresolved_node {
        for edge in graph.graph.edges_directed(unresolved, Direction::Incoming) {
            let ConflictEdge::Requires(requirement) = edge.weight() else {
                continue;
            };
            let sets: Vec<_> = match *requirement {
                Requirement::Single(set) => vec![set],
                Requirement::Union(union) => provider.version_sets_in_union(union).collect(),
            };
            for set in sets {
                if let SolverName::Feature(key, feature) = name_of_set(set) {
                    return ResolveError::MissingFeature {
                        package: key.name.clone(),
                        feature: feature.clone(),
                        message,
                    };
                }
            }
        }
    }

    // A feature some versions lack is a candidate of those versions with
    // unknown dependencies: the feature is missing when every candidate of
    // it the conflict involves lacks it.
    let mut features: BTreeMap<(&PackageKey, &FeatureName), bool> = BTreeMap::new();
    for node in graph.graph.node_indices() {
        let ConflictNode::Solvable(solvable) = graph.graph[node] else {
            continue;
        };
        let (SolverName::Feature(key, feature), _) = provider.solvable(solvable) else {
            continue;
        };
        let lacking = graph
            .graph
            .edges_directed(node, Direction::Outgoing)
            .any(|edge| {
                if let ConflictEdge::Conflict(ConflictCause::Excluded) = edge.weight()
                    && let ConflictNode::Excluded(reason) = graph.graph[edge.target()]
                {
                    provider.pool().resolve_string(reason) == MISSING_FEATURE
                } else {
                    false
                }
            });
        *features.entry((key, feature)).or_insert(true) &= lacking;
    }
    if let Some(((key, feature), _)) = features.into_iter().find(|(_, lacking)| *lacking) {
        return ResolveError::MissingFeature {
            package: key.name.clone(),
            feature: feature.clone(),
            message,
        };
    }

    for edge in graph.graph.edge_references() {
        let (source, target) = (graph.graph[edge.source()], graph.graph[edge.target()]);
        if let ConflictEdge::Conflict(ConflictCause::Excluded) = edge.weight()
            && let ConflictNode::Excluded(reason) = target
            && provider.pool().resolve_string(reason) == YANKED
            && let ConflictNode::Solvable(solvable) = source
        {
            let (_, reference) = provider.solvable(solvable);
            return ResolveError::OnlyYanked {
                package: provider.version(reference).key.name.clone(),
                message,
            };
        }
    }

    for edge in graph.graph.edge_references() {
        if let ConflictEdge::Conflict(ConflictCause::ForbidMultipleInstances) = edge.weight()
            && let ConflictNode::Solvable(solvable) = graph.graph[edge.target()]
            && let (SolverName::Links(links), _) = provider.solvable(solvable)
        {
            return ResolveError::LinksConflict {
                links: links.clone(),
                message,
            };
        }
    }

    ResolveError::Conflict { message }
}
