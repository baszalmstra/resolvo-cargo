//! Resolves crates.io packages with resolvo-cargo and with cargo's resolver
//! and compares the solutions and the time each takes, as
//! pubgrub-crates-benchmark does for PubGrub.
//!
//! Each case is one published version taken as the root of a resolution,
//! with all its features and its dev-dependencies, as cargo resolves a
//! workspace member for `Cargo.lock`. Both resolvers read the same local
//! checkout of the crates.io git index, whose layout is the sparse index's:
//! resolvo-cargo through `IndexSource::Directory`, cargo through a
//! `Registry` that reads and parses index files on demand, as cargo's
//! registry source does. Timings cover index reads, parsing and solving.
//!
//! ```text
//! corpus --index <crates.io-index checkout> --out <dir>
//!        [--sample N] [--seed S] [--latest] [--filter SUBSTRING]
//!        [--cases FILE] [--threads N]
//! ```
//!
//! `--latest` takes the newest non-yanked version of each crate instead of
//! every non-yanked version; `--sample` then keeps a deterministic
//! pseudo-random subset; `--cases` reads `name version` lines instead.
//! Writes `results.csv`, `summary.md` and one file per differing case
//! under `diffs/`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

use anyhow::{Context as _, bail};
use cargo::{
    core::{
        Dependency as CargoDependency, PackageId, Registry, SourceId, Summary as CargoSummary,
        dependency::DepKind as CargoDepKind,
        resolver::{self, ResolveOpts, ResolveVersion, VersionPreferences},
    },
    sources::{IndexSummary, source::QueryKind},
    util::{CargoResult, interning::InternedString},
};
use rayon::prelude::*;
use resolvo_cargo::{
    DepKind, DepName, DepSource, Dependency, FeatureName, IndexSource, PackageName, PackageSource,
    PathPackage, ResolveOptions, ResolverVersion, Summary, Workspace, resolve,
};
use semver::{Version, VersionReq};
use serde::Deserialize;

struct Args {
    index: PathBuf,
    out: PathBuf,
    sample: Option<usize>,
    seed: u64,
    latest: bool,
    filter: Option<String>,
    cases: Option<PathBuf>,
    threads: usize,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        index: PathBuf::new(),
        out: PathBuf::from("corpus-out"),
        sample: None,
        seed: 0,
        latest: false,
        filter: None,
        cases: None,
        threads: 0,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--index" => args.index = PathBuf::from(value()?),
            "--out" => args.out = PathBuf::from(value()?),
            "--sample" => args.sample = Some(value()?.parse()?),
            "--seed" => args.seed = value()?.parse()?,
            "--latest" => args.latest = true,
            "--filter" => args.filter = Some(value()?),
            "--cases" => args.cases = Some(PathBuf::from(value()?)),
            "--threads" => args.threads = value()?.parse()?,
            other => bail!("unknown argument '{other}'"),
        }
    }
    if args.index.as_os_str().is_empty() {
        bail!("--index <crates.io-index checkout> is required");
    }
    Ok(args)
}

/// One line of an index file.
#[derive(Deserialize)]
struct Line {
    name: String,
    vers: String,
    #[serde(default)]
    deps: Vec<LineDependency>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    features2: Option<BTreeMap<String, Vec<String>>>,
    yanked: Option<bool>,
    links: Option<String>,
    v: Option<u32>,
}

#[derive(Deserialize)]
struct LineDependency {
    name: String,
    req: String,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    optional: bool,
    #[serde(default = "default_true")]
    default_features: bool,
    target: Option<String>,
    kind: Option<String>,
    registry: Option<String>,
    package: Option<String>,
}

fn default_true() -> bool {
    true
}

/// What enumerating the index needs of a line.
#[derive(Deserialize)]
struct LineHead {
    name: String,
    vers: Version,
    yanked: Option<bool>,
    v: Option<u32>,
}

impl Line {
    fn features(&self) -> BTreeMap<&str, Vec<&str>> {
        let mut features: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for table in std::iter::once(&self.features).chain(self.features2.as_ref()) {
            for (feature, values) in table {
                features
                    .entry(feature)
                    .or_default()
                    .extend(values.iter().map(String::as_str));
            }
        }
        features
    }

    fn usable(&self) -> bool {
        !self.yanked.unwrap_or(false) && self.v.unwrap_or(1) <= 2
    }
}

/// The index file path of a crate, as the sparse protocol lays it out.
fn index_path(index: &Path, name: &str) -> PathBuf {
    let name = name.to_lowercase();
    match name.len() {
        1 => index.join("1").join(&name),
        2 => index.join("2").join(&name),
        3 => index.join("3").join(&name[..1]).join(&name),
        _ => index.join(&name[..2]).join(&name[2..4]).join(&name),
    }
}

/// The root of a case: one published version.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Case {
    name: String,
    version: Version,
}

/// Every crate file of the index checkout.
fn crate_files(index: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![index.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') || name == "config.json" {
                continue;
            }
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn enumerate_cases(args: &Args) -> anyhow::Result<Vec<Case>> {
    if let Some(cases) = &args.cases {
        let text = fs::read_to_string(cases)?;
        return text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let (name, version) = line
                    .trim()
                    .split_once(' ')
                    .with_context(|| format!("'{line}' is not `name version`"))?;
                Ok(Case {
                    name: name.to_owned(),
                    version: Version::parse(version)?,
                })
            })
            .collect();
    }

    let files = crate_files(&args.index)?;
    eprintln!("{} crates in the index", files.len());
    let mut cases: Vec<Case> = files
        .par_iter()
        .flat_map_iter(|file| {
            let text = fs::read(file).unwrap_or_default();
            let mut versions: Vec<Case> = text
                .split(|byte| *byte == b'\n')
                .filter_map(|line| serde_json::from_slice::<LineHead>(line).ok())
                .filter(|head| !head.yanked.unwrap_or(false) && head.v.unwrap_or(1) <= 2)
                .filter(|head| {
                    args.filter
                        .as_ref()
                        .is_none_or(|filter| head.name.contains(filter.as_str()))
                })
                .map(|head| Case {
                    name: head.name,
                    version: head.vers,
                })
                .collect();
            if args.latest {
                let newest = versions
                    .iter()
                    .filter(|case| case.version.pre.is_empty())
                    .max_by(|a, b| a.version.cmp(&b.version))
                    .or_else(|| versions.iter().max_by(|a, b| a.version.cmp(&b.version)))
                    .cloned();
                versions = newest.into_iter().collect();
            }
            versions
        })
        .collect();
    cases.sort();
    if let Some(sample) = args.sample {
        cases.sort_by_key(|case| stable_hash(args.seed, &case.name, &case.version));
        cases.truncate(sample);
        cases.sort();
    }
    Ok(cases)
}

/// FNV-1a, stable across runs and toolchains so samples are reproducible.
fn stable_hash(seed: u64, name: &str, version: &Version) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed;
    for byte in name
        .bytes()
        .chain([b' '])
        .chain(version.to_string().bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The index line of a case's root.
fn root_line(index: &Path, case: &Case) -> anyhow::Result<Line> {
    let text = fs::read(index_path(index, &case.name))?;
    for line in text.split(|byte| *byte == b'\n') {
        if let Ok(parsed) = serde_json::from_slice::<Line>(line)
            && parsed.name == case.name
            && Version::parse(&parsed.vers).is_ok_and(|version| version == case.version)
        {
            return Ok(parsed);
        }
    }
    bail!("{} {} is not in the index", case.name, case.version)
}

// ---------------------------------------------------------------------------
// resolvo-cargo

fn ours_summary(line: &Line) -> anyhow::Result<Summary> {
    let mut dependencies = Vec::new();
    for dependency in &line.deps {
        if dependency.registry.is_some() {
            bail!("dependency from another registry");
        }
        dependencies.push(Dependency {
            name: DepName::new(dependency.name.as_str()),
            package: PackageName::new(dependency.package.as_deref().unwrap_or(&dependency.name)),
            req: VersionReq::parse(&dependency.req)?,
            source: DepSource::Registry,
            features: dependency
                .features
                .iter()
                .filter(|feature| !feature.is_empty())
                .map(|feature| FeatureName::new(feature.as_str()))
                .collect(),
            default_features: dependency.default_features,
            optional: dependency.optional,
            kind: match dependency.kind.as_deref() {
                Some("dev") => DepKind::Dev,
                Some("build") => DepKind::Build,
                Some(_) | None => DepKind::Normal,
            },
            target: dependency.target.clone(),
        });
    }
    let features: BTreeMap<FeatureName, Vec<String>> = line
        .features()
        .into_iter()
        .map(|(feature, values)| {
            (
                FeatureName::new(feature),
                values.into_iter().map(str::to_owned).collect(),
            )
        })
        .collect();
    Ok(Summary::new(
        PackageName::new(line.name.as_str()),
        Version::parse(&line.vers)?,
        dependencies,
        &features,
        line.links.clone(),
        None,
    )?)
}

/// Resolves with resolvo-cargo, as a workspace whose only member is the
/// root.
fn resolve_ours(
    runtime: &tokio::runtime::Runtime,
    index: &Path,
    root: Summary,
) -> anyhow::Result<Solution> {
    let directory = PathBuf::from("/corpus/root");
    let workspace = Workspace::from_packages(
        directory.clone(),
        ResolverVersion::V2,
        vec![PathPackage::new(directory, root, true)],
    );
    let source = IndexSource::Directory(index.to_path_buf());
    let lockfile = runtime.block_on(resolve(&workspace, &source, &ResolveOptions::default()))?;
    let node = |id: &resolvo_cargo::PackageId| Node {
        name: id.name.to_string(),
        version: id.version.clone(),
        root: id.source == PackageSource::Path,
    };
    let mut solution = Solution::default();
    for package in lockfile.packages() {
        let from = node(&package.id);
        for dependency in &package.dependencies {
            solution.edges.insert((from.clone(), node(dependency)));
        }
        solution.packages.insert(from);
    }
    Ok(solution)
}

// ---------------------------------------------------------------------------
// cargo

fn registry_source() -> CargoResult<SourceId> {
    SourceId::for_registry(&"https://example.com".parse()?)
}

fn root_source() -> CargoResult<SourceId> {
    SourceId::for_registry(&"https://root.example.com".parse()?)
}

fn cargo_summary(line: &Line, source: SourceId) -> anyhow::Result<CargoSummary> {
    let registry = registry_source()?;
    let mut dependencies = Vec::new();
    for dependency in &line.deps {
        if dependency.registry.is_some() {
            bail!("dependency from another registry");
        }
        let package = dependency.package.as_deref().unwrap_or(&dependency.name);
        let mut parsed = CargoDependency::parse(package, Some(&dependency.req), registry)?;
        if dependency.package.is_some() {
            parsed.set_explicit_name_in_toml(dependency.name.as_str());
        }
        parsed
            .set_kind(match dependency.kind.as_deref() {
                Some("dev") => CargoDepKind::Development,
                Some("build") => CargoDepKind::Build,
                Some(_) | None => CargoDepKind::Normal,
            })
            .set_optional(dependency.optional)
            .set_default_features(dependency.default_features)
            .set_features(
                dependency
                    .features
                    .iter()
                    .filter(|feature| !feature.is_empty())
                    .map(String::as_str),
            );
        dependencies.push(parsed);
    }
    let features: BTreeMap<InternedString, Vec<InternedString>> = line
        .features()
        .into_iter()
        .map(|(feature, values)| {
            (
                InternedString::new(feature),
                values.into_iter().map(InternedString::new).collect(),
            )
        })
        .collect();
    let id = PackageId::try_new(line.name.as_str(), line.vers.as_str(), source)?;
    CargoSummary::new(id, dependencies, &features, line.links.as_deref(), None)
}

/// A cargo `Registry` over the index checkout. Like cargo's registry
/// source it reads a crate's index file on first query and parses only
/// the versions a dependency matches, leaving yanked and unsupported
/// versions out.
struct FileRegistry<'a> {
    index: &'a Path,
    source: SourceId,
    crates: HashMap<String, Vec<CargoEntry>>,
    /// When set, the only registry versions offered.
    allowed: Option<&'a BTreeSet<(String, Version)>>,
}

struct CargoEntry {
    name: String,
    version: Version,
    line: Vec<u8>,
    summary: Option<Option<CargoSummary>>,
}

impl FileRegistry<'_> {
    fn load(&self, name: &str) -> Vec<CargoEntry> {
        let Ok(text) = fs::read(index_path(self.index, name)) else {
            return Vec::new();
        };
        text.split(|byte| *byte == b'\n')
            .filter_map(|line| {
                let head = serde_json::from_slice::<LineHead>(line).ok()?;
                (!head.yanked.unwrap_or(false) && head.v.unwrap_or(1) <= 2).then(|| CargoEntry {
                    name: head.name,
                    version: head.vers,
                    line: line.to_vec(),
                    summary: None,
                })
            })
            .collect()
    }
}

impl Registry for FileRegistry<'_> {
    fn query(
        &mut self,
        dependency: &CargoDependency,
        kind: QueryKind,
        f: &mut dyn FnMut(IndexSummary),
    ) -> Poll<CargoResult<()>> {
        match kind {
            QueryKind::Exact => {}
            // Only used for suggestions in error messages.
            QueryKind::AlternativeNames | QueryKind::RejectedVersions | QueryKind::Normalized => {
                return Poll::Ready(Ok(()));
            }
        }
        let name = dependency.package_name().as_str().to_lowercase();
        if !self.crates.contains_key(&name) {
            let entries = self.load(&name);
            self.crates.insert(name.clone(), entries);
        }
        let source = self.source;
        for entry in self.crates.get_mut(&name).into_iter().flatten() {
            if entry.name != dependency.package_name().as_str()
                || !dependency.version_req().matches(&entry.version)
                || self.allowed.is_some_and(|allowed| {
                    !allowed.contains(&(entry.name.clone(), entry.version.clone()))
                })
            {
                continue;
            }
            let summary = entry.summary.get_or_insert_with(|| {
                serde_json::from_slice::<Line>(&entry.line)
                    .ok()
                    .filter(Line::usable)
                    .and_then(|line| cargo_summary(&line, source).ok())
            });
            if let Some(summary) = summary
                && dependency.matches(summary)
            {
                f(IndexSummary::Candidate(summary.clone()));
            }
        }
        Poll::Ready(Ok(()))
    }

    fn describe_source(&self, _source: SourceId) -> String {
        String::new()
    }

    fn is_replaced(&self, _source: SourceId) -> bool {
        false
    }

    fn block_until_ready(&mut self) -> CargoResult<()> {
        Ok(())
    }
}

/// Resolves with cargo; with `allowed`, offering only those registry
/// versions, which checks whether cargo accepts a given solution.
fn resolve_cargo(
    index: &Path,
    root: CargoSummary,
    allowed: Option<&BTreeSet<(String, Version)>>,
) -> anyhow::Result<Solution> {
    let mut registry = FileRegistry {
        index,
        source: registry_source()?,
        crates: HashMap::new(),
        allowed,
    };
    let resolve = resolver::resolve(
        &[(root, ResolveOpts::everything())],
        &[],
        &mut registry,
        &VersionPreferences::default(),
        ResolveVersion::with_rust_version(None),
        None,
    )?;
    let root_source = root_source()?;
    let node = |id: PackageId| Node {
        name: id.name().to_string(),
        version: id.version().clone(),
        root: id.source_id() == root_source,
    };
    let mut solution = Solution::default();
    for id in resolve.iter() {
        for (dependency, _) in resolve.deps(id) {
            solution.edges.insert((node(id), node(dependency)));
        }
        solution.packages.insert(node(id));
    }
    Ok(solution)
}

// ---------------------------------------------------------------------------
// comparison

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
struct Node {
    name: String,
    version: Version,
    root: bool,
}

impl std::fmt::Display for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let root = if self.root { " (root)" } else { "" };
        write!(f, "{} {}{root}", self.name, self.version)
    }
}

#[derive(Default, PartialEq, Eq)]
struct Solution {
    packages: BTreeSet<Node>,
    edges: BTreeSet<(Node, Node)>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Verdict {
    Same,
    /// Another solution, which cargo also arrives at when offered only its
    /// versions: cargo accepts it as a lock file.
    DifferentCargoAccepts,
    /// Another solution cargo does not arrive at even when offered only its
    /// versions.
    DifferentCargoRejects,
    BothFail,
    OnlyCargoFails,
    OnlyOursFails,
}

struct Outcome {
    case: Case,
    verdict: Verdict,
    cargo: Duration,
    ours: Duration,
    packages: usize,
    detail: String,
}

fn run_case(index: &Path, case: &Case) -> anyhow::Result<Option<Outcome>> {
    let line = root_line(index, case)?;
    // Roots either resolver rejects are not cases.
    let (Ok(cargo_root), Ok(our_root)) =
        (cargo_summary(&line, root_source()?), ours_summary(&line))
    else {
        return Ok(None);
    };

    let started = Instant::now();
    let cargo = resolve_cargo(index, cargo_root.clone(), None);
    let cargo_time = started.elapsed();
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let started = Instant::now();
    let ours = resolve_ours(&runtime, index, our_root);
    let ours_time = started.elapsed();

    let first_line = |error: &anyhow::Error| {
        format!("{error:#}")
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let (verdict, packages, detail) = match (&cargo, &ours) {
        (Ok(cargo), Ok(ours)) if cargo == ours => {
            (Verdict::Same, ours.packages.len(), String::new())
        }
        (Ok(cargo), Ok(ours)) => {
            let allowed: BTreeSet<(String, Version)> = ours
                .packages
                .iter()
                .filter(|node| !node.root)
                .map(|node| (node.name.clone(), node.version.clone()))
                .collect();
            let verdict = match resolve_cargo(index, cargo_root, Some(&allowed)) {
                Ok(restricted) if restricted == *ours => Verdict::DifferentCargoAccepts,
                Ok(_) | Err(_) => Verdict::DifferentCargoRejects,
            };
            (verdict, ours.packages.len(), diff(cargo, ours))
        }
        (Err(cargo), Err(ours)) => (
            Verdict::BothFail,
            0,
            format!("cargo: {}\nours: {}\n", first_line(cargo), first_line(ours)),
        ),
        (Err(cargo), Ok(ours)) => (
            Verdict::OnlyCargoFails,
            ours.packages.len(),
            format!("cargo: {cargo:#}\n"),
        ),
        (Ok(cargo), Err(ours)) => (
            Verdict::OnlyOursFails,
            cargo.packages.len(),
            format!("ours: {ours:#}\n"),
        ),
    };
    Ok(Some(Outcome {
        case: case.clone(),
        verdict,
        cargo: cargo_time,
        ours: ours_time,
        packages,
        detail,
    }))
}

fn diff(cargo: &Solution, ours: &Solution) -> String {
    let mut out = String::new();
    for node in cargo.packages.difference(&ours.packages) {
        let _ = writeln!(out, "- {node}");
    }
    for node in ours.packages.difference(&cargo.packages) {
        let _ = writeln!(out, "+ {node}");
    }
    for (from, to) in cargo.edges.difference(&ours.edges).take(50) {
        let _ = writeln!(out, "- edge {from} -> {to}");
    }
    for (from, to) in ours.edges.difference(&cargo.edges).take(50) {
        let _ = writeln!(out, "+ edge {from} -> {to}");
    }
    out
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let position = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[position.min(sorted.len() - 1)]
}

fn milliseconds(duration: Duration) -> String {
    format!("{:.2}", duration.as_secs_f64() * 1000.0)
}

fn summary(outcomes: &[Outcome], elapsed: Duration, threads: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# resolvo-cargo vs cargo\n\n{} cases, {threads} threads, {:.1}s wall time\n",
        outcomes.len(),
        elapsed.as_secs_f64()
    );
    let mut counts: BTreeMap<Verdict, usize> = BTreeMap::new();
    for outcome in outcomes {
        *counts.entry(outcome.verdict).or_default() += 1;
    }
    let _ = writeln!(out, "| Verdict | Cases |\n|---|---:|");
    for (verdict, count) in &counts {
        let _ = writeln!(out, "| {verdict:?} | {count} |");
    }

    let mut cargo: Vec<Duration> = outcomes.iter().map(|outcome| outcome.cargo).collect();
    let mut ours: Vec<Duration> = outcomes.iter().map(|outcome| outcome.ours).collect();
    cargo.sort();
    ours.sort();
    let _ = writeln!(
        out,
        "\n| Time (ms) | cargo | resolvo-cargo |\n|---|---:|---:|"
    );
    let total = |times: &[Duration]| times.iter().sum::<Duration>();
    let _ = writeln!(
        out,
        "| total | {} | {} |",
        milliseconds(total(&cargo)),
        milliseconds(total(&ours))
    );
    for (label, fraction) in [("median", 0.5), ("p90", 0.9), ("p99", 0.99), ("max", 1.0)] {
        let _ = writeln!(
            out,
            "| {label} | {} | {} |",
            milliseconds(percentile(&cargo, fraction)),
            milliseconds(percentile(&ours, fraction))
        );
    }
    let slower = outcomes
        .iter()
        .filter(|outcome| outcome.ours > outcome.cargo)
        .count();
    let _ = writeln!(out, "\nresolvo-cargo slower than cargo in {slower} cases.");

    let mut worst: Vec<&Outcome> = outcomes.iter().collect();
    worst.sort_by(|a, b| {
        (b.ours.as_secs_f64() - b.cargo.as_secs_f64())
            .total_cmp(&(a.ours.as_secs_f64() - a.cargo.as_secs_f64()))
    });
    let _ = writeln!(
        out,
        "\n## Largest slowdowns\n\n| Case | Packages | cargo (ms) | resolvo-cargo (ms) |\n|---|---:|---:|---:|"
    );
    for outcome in worst.iter().take(15) {
        let _ = writeln!(
            out,
            "| {} {} | {} | {} | {} |",
            outcome.case.name,
            outcome.case.version,
            outcome.packages,
            milliseconds(outcome.cargo),
            milliseconds(outcome.ours)
        );
    }
    let _ = writeln!(
        out,
        "\n## Slowest for cargo\n\n| Case | Packages | cargo (ms) | resolvo-cargo (ms) |\n|---|---:|---:|---:|"
    );
    let mut slowest: Vec<&Outcome> = outcomes.iter().collect();
    slowest.sort_by_key(|outcome| std::cmp::Reverse(outcome.cargo));
    for outcome in slowest.iter().take(15) {
        let _ = writeln!(
            out,
            "| {} {} | {} | {} | {} |",
            outcome.case.name,
            outcome.case.version,
            outcome.packages,
            milliseconds(outcome.cargo),
            milliseconds(outcome.ours)
        );
    }
    out
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build_global()?;
    let threads = rayon::current_num_threads();
    let cases = enumerate_cases(&args)?;
    eprintln!("{} cases on {threads} threads", cases.len());

    fs::create_dir_all(args.out.join("diffs"))?;
    let results = Mutex::new(fs::File::create(args.out.join("results.csv"))?);
    writeln!(
        results.lock().map_err(|_| anyhow::anyhow!("poisoned"))?,
        "name,version,verdict,packages,cargo_ms,ours_ms"
    )?;

    let done = AtomicUsize::new(0);
    let started = Instant::now();
    let outcomes: Vec<Outcome> = cases
        .par_iter()
        .filter_map(|case| {
            let outcome = match run_case(&args.index, case) {
                Ok(outcome) => outcome,
                Err(error) => {
                    eprintln!("{} {}: {error:#}", case.name, case.version);
                    None
                }
            };
            let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
            if finished.is_multiple_of(500) {
                eprintln!(
                    "{finished}/{} cases, {:.0}s",
                    cases.len(),
                    started.elapsed().as_secs_f64()
                );
            }
            let outcome = outcome?;
            if let Ok(mut file) = results.lock() {
                let _ = writeln!(
                    file,
                    "{},{},{:?},{},{},{}",
                    outcome.case.name,
                    outcome.case.version,
                    outcome.verdict,
                    outcome.packages,
                    milliseconds(outcome.cargo),
                    milliseconds(outcome.ours)
                );
            }
            if outcome.verdict != Verdict::Same {
                let file = args.out.join("diffs").join(format!(
                    "{:?}-{}-{}.txt",
                    outcome.verdict, outcome.case.name, outcome.case.version
                ));
                let _ = fs::write(file, &outcome.detail);
            }
            Some(outcome)
        })
        .collect();

    let report = summary(&outcomes, started.elapsed(), threads);
    fs::write(args.out.join("summary.md"), &report)?;
    println!("{report}");
    Ok(())
}
