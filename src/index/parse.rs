//! Parsing a registry index file: one JSON object per line, one line per
//! published version, as cargo's `IndexSummary::parse` reads it.
//!
//! Reading a file parses only each line's head: name, version, checksum,
//! yanked, `links`, `rust_version` and format version. The dependencies
//! and features of a version are parsed when a resolution first asks for
//! them ([`parse_summary`]).
//!
//! cargo keeps lines it cannot use (a format version it does not know, a
//! dependency requirement that does not parse, a feature table it rejects)
//! as unsupported or invalid entries that never take part in resolution.
//! Lines whose head already shows this are left out of the parsed file;
//! the others yield no summary. Only a line whose `name` and `vers` cannot
//! be read is an error.

use std::{collections::BTreeMap, ops::Range, sync::Arc};

use semver::{Version, VersionReq};
use serde::Deserialize;
use thiserror::Error;

use super::IndexEntry;
use crate::{
    error::{FeatureMapError, IndexError, RustVersionError},
    names::{DepName, FeatureName, PackageName},
    summary::{DepKind, DepSource, Dependency, RustVersion, Summary},
};

/// The highest index format version (`v`) understood. Version 2 adds
/// `features2`; version 3 adds artifact dependencies, which are not
/// supported.
const INDEX_V_MAX: u32 = 2;

/// The dependencies and features of an index line, as cargo's
/// `IndexPackage` declares them. `deps` is required, as in cargo, so that
/// the same lines are invalid; [`IndexLineHead`] requires `cksum`.
#[derive(Deserialize)]
struct IndexLine {
    name: String,
    vers: Version,
    deps: Vec<IndexDependency>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    features2: Option<BTreeMap<String, Vec<String>>>,
    links: Option<String>,
    rust_version: Option<String>,
}

/// One element of `deps` in an index line, as cargo's
/// `RegistryDependency` declares it.
#[derive(Deserialize)]
struct IndexDependency {
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

/// The head of an index line: what identifies and ranks the version.
#[derive(Deserialize)]
struct IndexLineHead {
    name: String,
    vers: Version,
    cksum: String,
    yanked: Option<bool>,
    links: Option<String>,
    rust_version: Option<String>,
    v: Option<u32>,
}

/// The part of an index line that identifies the version, read when the
/// head cannot be.
#[derive(Deserialize)]
struct MinimalIndexLine {
    name: String,
    vers: Version,
}

/// Why an index line identifying a version is left out of the index. The
/// messages include their causes because they are only logged.
#[derive(Debug, Error)]
enum SkippedLine {
    /// The line does not have the shape of an index entry beyond `name`
    /// and `vers`.
    #[error("the entry is malformed: {0}")]
    Malformed(serde_json::Error),
    /// The format version is newer than this parser understands.
    #[error("index format version {0} is not supported")]
    UnsupportedFormat(u32),
    /// A dependency comes from another registry.
    #[error("dependency `{0}` comes from another registry")]
    AlternativeRegistry(String),
    /// A dependency requirement is not a semver requirement.
    #[error("dependency `{dep}` has an invalid version requirement: {error}")]
    InvalidRequirement {
        /// The dependency key.
        dep: String,
        /// What the semver parser reported.
        error: semver::Error,
    },
    /// The `rust_version` field does not parse.
    #[error("{0}")]
    InvalidRustVersion(RustVersionError),
    /// The feature table is one cargo rejects.
    #[error("{0}")]
    InvalidFeatures(FeatureMapError),
}

impl SkippedLine {
    /// The JSON error of a line that is not shaped like an index entry.
    fn into_json_error(self) -> Option<serde_json::Error> {
        match self {
            SkippedLine::Malformed(error) => Some(error),
            SkippedLine::UnsupportedFormat(_)
            | SkippedLine::AlternativeRegistry(_)
            | SkippedLine::InvalidRequirement { .. }
            | SkippedLine::InvalidRustVersion(_)
            | SkippedLine::InvalidFeatures(_) => None,
        }
    }
}

/// Parses the head of every line of the index file of the crate `name`,
/// leaving out the lines cargo never resolves against. The entries are
/// ascending by version; when two lines carry the same version the later
/// one wins, as in cargo.
///
/// # Errors
///
/// [`IndexError::Parse`] for a line that does not even have a `name` and
/// a `vers`.
pub(super) fn parse_index_file(
    name: &PackageName,
    contents: Vec<u8>,
) -> Result<Vec<IndexEntry>, IndexError> {
    let file: Arc<[u8]> = Arc::from(contents);
    let mut entries = BTreeMap::new();
    let mut start = 0;
    for (index, line) in file.split(|byte| *byte == b'\n').enumerate() {
        let range = start..start + line.len();
        start = range.end + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match parse_head(line, &file, range) {
            Ok(entry) => {
                entries.insert(entry.version.clone(), entry);
            }
            Err(skipped) => match serde_json::from_slice::<MinimalIndexLine>(line) {
                Ok(minimal) => tracing::debug!(
                    name = %minimal.name,
                    version = %minimal.vers,
                    reason = %skipped,
                    "skipping index entry",
                ),
                Err(minimal_error) => {
                    // Like cargo, report the head's error when there is
                    // one.
                    return Err(IndexError::Parse {
                        name: name.clone(),
                        line: index + 1,
                        source: skipped.into_json_error().unwrap_or(minimal_error),
                    });
                }
            },
        }
    }
    Ok(entries.into_values().collect())
}

/// Parses the head of one line into an entry: a supported format version
/// with a valid `rust_version`.
fn parse_head(
    line: &[u8],
    file: &Arc<[u8]>,
    range: Range<usize>,
) -> Result<IndexEntry, SkippedLine> {
    let head: IndexLineHead = serde_json::from_slice(line).map_err(SkippedLine::Malformed)?;
    let format = head.v.unwrap_or(1);
    if format > INDEX_V_MAX {
        return Err(SkippedLine::UnsupportedFormat(format));
    }
    let rust_version = head
        .rust_version
        .map(|text| text.parse::<RustVersion>())
        .transpose()
        .map_err(SkippedLine::InvalidRustVersion)?;
    Ok(IndexEntry {
        name: PackageName::new(head.name),
        version: head.vers,
        checksum: head.cksum,
        yanked: head.yanked.unwrap_or(false),
        links: head.links,
        rust_version,
        file: Arc::clone(file),
        line: range,
        summary: std::sync::OnceLock::new(),
    })
}

/// Parses the dependencies and features of one line, `None` when cargo
/// rejects them.
pub(super) fn parse_summary(line: &[u8]) -> Option<Summary> {
    let result = serde_json::from_slice::<IndexLine>(line)
        .map_err(SkippedLine::Malformed)
        .and_then(to_summary);
    match result {
        Ok(summary) => Some(summary),
        Err(skipped) => {
            tracing::debug!(reason = %skipped, "index entry takes no part in resolution");
            None
        }
    }
}

/// cargo's `index_package_to_summary`.
fn to_summary(line: IndexLine) -> Result<Summary, SkippedLine> {
    let dependencies = line
        .deps
        .into_iter()
        .map(to_dependency)
        .collect::<Result<Vec<_>, _>>()?;
    let mut features = line.features;
    for (feature, values) in line.features2.into_iter().flatten() {
        features.entry(feature).or_default().extend(values);
    }
    let features: BTreeMap<FeatureName, Vec<String>> = features
        .into_iter()
        .map(|(feature, values)| (FeatureName::new(feature), values))
        .collect();
    let rust_version = line
        .rust_version
        .map(|text| text.parse::<RustVersion>())
        .transpose()
        .map_err(SkippedLine::InvalidRustVersion)?;
    Summary::new(
        PackageName::new(line.name),
        line.vers,
        dependencies,
        &features,
        line.links,
        rust_version,
    )
    .map_err(SkippedLine::InvalidFeatures)
}

/// cargo's `registry_dependency_into_dep`.
fn to_dependency(dependency: IndexDependency) -> Result<Dependency, SkippedLine> {
    if dependency.registry.is_some() {
        return Err(SkippedLine::AlternativeRegistry(dependency.name));
    }
    let req =
        VersionReq::parse(&dependency.req).map_err(|error| SkippedLine::InvalidRequirement {
            dep: dependency.name.clone(),
            error,
        })?;
    let package = PackageName::new(dependency.package.as_deref().unwrap_or(&dependency.name));
    let kind = match dependency.kind.as_deref() {
        Some("dev") => DepKind::Dev,
        Some("build") => DepKind::Build,
        Some(_) | None => DepKind::Normal,
    };
    // Old cargo and registry versions published empty feature names, which
    // cargo drops.
    let features = dependency
        .features
        .into_iter()
        .filter(|feature| !feature.is_empty())
        .map(FeatureName::new)
        .collect();
    Ok(Dependency {
        name: DepName::new(dependency.name),
        package,
        req,
        source: DepSource::Registry,
        features,
        default_features: dependency.default_features,
        optional: dependency.optional,
        kind,
        target: dependency.target,
    })
}
