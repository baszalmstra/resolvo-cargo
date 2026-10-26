//! Resolver fixtures in the shape of cargo's `resolver-tests` helpers:
//! registry packages and their dependencies are built in code, written as
//! an index directory with the sparse layout, and resolved through the
//! public API for a workspace of one package, `root 1.0.0`, whose
//! dependencies are the case's.
//!
//! As in cargo's helpers, a package is version `1.0.0` unless given as a
//! `(name, version)` pair, a package named `*-sys` declares `links` equal to
//! its name, `dep("x")` requires `*`, and the shorthands on a bare name
//! (`"x".opt()`, `"x".with(..)`) require `1.0.0`, that is `^1.0.0`.
#![expect(
    dead_code,
    reason = "each test file uses a different part of the fixtures"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::Path,
};

use resolvo_cargo::{
    DepKind, FeatureError, FeatureRequest, FeaturesFor, IndexSource, Lockfile, ManifestError,
    PackageId, PackageName, PackageSource, ResolveError, ResolveOptions, ResolvedFeatures,
    TargetPlatform, Workspace, resolve as resolve_workspace, resolve_features,
};
use semver::Version;
use serde_json::json;
use tempfile::TempDir;

/// A crate name and version: a bare name is version `1.0.0`.
pub trait ToPkgId {
    /// The name and the version.
    fn to_pkg_id(self) -> (String, String);
}

impl ToPkgId for &str {
    fn to_pkg_id(self) -> (String, String) {
        (self.to_owned(), "1.0.0".to_owned())
    }
}

impl<N: AsRef<str>, V: AsRef<str>> ToPkgId for (N, V) {
    fn to_pkg_id(self) -> (String, String) {
        (self.0.as_ref().to_owned(), self.1.as_ref().to_owned())
    }
}

/// One version of a crate in the fixture index.
#[derive(Clone, Debug)]
pub struct Pkg {
    name: String,
    version: String,
    deps: Vec<Dep>,
    features: BTreeMap<String, Vec<String>>,
    links: Option<String>,
    yanked: bool,
    rust_version: Option<String>,
}

/// A registry package without dependencies or features. A name ending in
/// `-sys` declares `links` equal to the name.
pub fn pkg(id: impl ToPkgId) -> Pkg {
    let (name, version) = id.to_pkg_id();
    let links = name.ends_with("-sys").then(|| name.clone());
    Pkg {
        name,
        version,
        deps: Vec::new(),
        features: BTreeMap::new(),
        links,
        yanked: false,
        rust_version: None,
    }
}

impl Pkg {
    /// Adds dependencies, bare names meaning `name = "1.0.0"`.
    pub fn deps<D: ToDep>(mut self, deps: impl IntoIterator<Item = D>) -> Pkg {
        self.deps.extend(deps.into_iter().map(ToDep::to_dep));
        self
    }

    /// Adds feature `name` enabling `values` (`g`, `dep:d`, `d/f`, `d?/f`).
    pub fn feature(mut self, name: &str, values: &[&str]) -> Pkg {
        self.features.insert(
            name.to_owned(),
            values.iter().map(|&value| value.to_owned()).collect(),
        );
        self
    }

    /// Sets `links`.
    pub fn links(mut self, links: &str) -> Pkg {
        self.links = Some(links.to_owned());
        self
    }

    /// Marks the version yanked.
    pub fn yanked(mut self) -> Pkg {
        self.yanked = true;
        self
    }

    /// Sets `rust_version`.
    pub fn rust_version(mut self, rust_version: &str) -> Pkg {
        self.rust_version = Some(rust_version.to_owned());
        self
    }

    /// The fake `.crate` checksum written to the index: the hex of
    /// `name version`, padded to the 64 digits of a sha256.
    pub fn checksum(&self) -> String {
        let mut checksum = String::new();
        for byte in format!("{} {}", self.name, self.version).bytes() {
            let _ = write!(checksum, "{byte:02x}");
        }
        format!("{checksum:0<64}")
    }

    /// The package's line of its index file.
    fn index_line(&self) -> String {
        let deps: Vec<serde_json::Value> = self
            .deps
            .iter()
            .map(|dep| {
                let kind = match dep.kind {
                    DepKind::Normal => "normal",
                    DepKind::Build => "build",
                    DepKind::Dev => "dev",
                };
                json!({
                    "name": dep.key(),
                    "req": dep.req,
                    "features": dep.features,
                    "optional": dep.optional,
                    "default_features": dep.default_features,
                    "target": dep.target,
                    "kind": kind,
                    "package": dep.rename.as_ref().map(|_| &dep.package),
                })
            })
            .collect();
        json!({
            "name": self.name,
            "vers": self.version,
            "deps": deps,
            "cksum": self.checksum(),
            "features": self.features,
            "yanked": self.yanked,
            "links": self.links,
            "rust_version": self.rust_version,
        })
        .to_string()
    }
}

/// `registry` without the package version `id`.
pub fn without(registry: &[Pkg], id: impl ToPkgId) -> Vec<Pkg> {
    let (name, version) = id.to_pkg_id();
    registry
        .iter()
        .filter(|package| package.name != name || package.version != version)
        .cloned()
        .collect()
}

/// A dependency entry, of a registry package or of `root`.
#[derive(Clone, Debug)]
pub struct Dep {
    package: String,
    rename: Option<String>,
    req: String,
    optional: bool,
    default_features: bool,
    features: Vec<String>,
    kind: DepKind,
    target: Option<String>,
}

impl Dep {
    /// The key the dependent knows the dependency by.
    fn key(&self) -> &str {
        self.rename.as_deref().unwrap_or(&self.package)
    }

    /// The dependency as an entry of a `Cargo.toml` dependency table.
    fn manifest_entry(&self) -> String {
        let mut fields = vec![format!("version = {}", quote(&self.req))];
        if self.rename.is_some() {
            fields.push(format!("package = {}", quote(&self.package)));
        }
        if self.optional {
            fields.push("optional = true".to_owned());
        }
        if !self.default_features {
            fields.push("default-features = false".to_owned());
        }
        if !self.features.is_empty() {
            let features: Vec<String> =
                self.features.iter().map(|feature| quote(feature)).collect();
            fields.push(format!("features = [{}]", features.join(", ")));
        }
        format!("{} = {{ {} }}", quote(self.key()), fields.join(", "))
    }
}

/// cargo's `dep`: a normal dependency on `name = "*"`.
pub fn dep(name: &str) -> Dep {
    dep_req(name, "*")
}

/// cargo's `dep_req`: a normal dependency on `name = req`.
pub fn dep_req(name: &str, req: &str) -> Dep {
    Dep {
        package: name.to_owned(),
        rename: None,
        req: req.to_owned(),
        optional: false,
        default_features: true,
        features: Vec::new(),
        kind: DepKind::Normal,
        target: None,
    }
}

/// cargo's `dep_kind`: a dependency of `kind` on `name = "*"`.
pub fn dep_kind(name: &str, kind: DepKind) -> Dep {
    dep_req_kind(name, "*", kind)
}

/// cargo's `dep_req_kind`: a dependency of `kind` on `name = req`.
pub fn dep_req_kind(name: &str, req: &str, kind: DepKind) -> Dep {
    Dep {
        kind,
        ..dep_req(name, req)
    }
}

/// cargo's `ToDep`: modifiers on a dependency, or on a bare name meaning
/// `name = "1.0.0"`.
pub trait ToDep: Sized {
    /// The dependency itself.
    fn to_dep(self) -> Dep;

    /// Makes it optional.
    fn opt(self) -> Dep {
        Dep {
            optional: true,
            ..self.to_dep()
        }
    }

    /// Enables `features` and, as in cargo's helper, turns default
    /// features off.
    fn with(self, features: &[&str]) -> Dep {
        Dep {
            default_features: false,
            features: features.iter().map(|&feature| feature.to_owned()).collect(),
            ..self.to_dep()
        }
    }

    /// Enables `features`, keeping default features as they are.
    fn features(self, features: &[&str]) -> Dep {
        Dep {
            features: features.iter().map(|&feature| feature.to_owned()).collect(),
            ..self.to_dep()
        }
    }

    /// Turns default features back on.
    fn with_default(self) -> Dep {
        Dep {
            default_features: true,
            ..self.to_dep()
        }
    }

    /// Makes the dependent know the crate by `key` (`key = { package = .. }`).
    fn rename(self, key: &str) -> Dep {
        Dep {
            rename: Some(key.to_owned()),
            ..self.to_dep()
        }
    }

    /// Limits it to `platform`: a `cfg(..)` expression or a target triple.
    fn on(self, platform: &str) -> Dep {
        Dep {
            target: Some(platform.to_owned()),
            ..self.to_dep()
        }
    }
}

impl ToDep for Dep {
    fn to_dep(self) -> Dep {
        self
    }
}

impl ToDep for &str {
    fn to_dep(self) -> Dep {
        dep_req(self, "1.0.0")
    }
}

/// A resolution of `root` against a fixture index, with the manifest
/// lines and options the default case does not need.
pub struct Case {
    registry: Vec<Pkg>,
    deps: Vec<Dep>,
    package_fields: Vec<String>,
    manifest_tail: String,
    files: Vec<(String, String)>,
    previous: Option<String>,
    rust_version: Option<Version>,
}

/// The case of `root` depending on `deps`, resolved against `registry`.
pub fn case<D: ToDep>(registry: Vec<Pkg>, deps: impl IntoIterator<Item = D>) -> Case {
    Case {
        registry,
        deps: deps.into_iter().map(ToDep::to_dep).collect(),
        package_fields: Vec::new(),
        manifest_tail: String::new(),
        files: Vec::new(),
        previous: None,
        rust_version: None,
    }
}

impl Case {
    /// Adds a line to `root`'s `[package]` table, such as
    /// `rust-version = "1.70"`.
    pub fn package_field(mut self, line: &str) -> Case {
        self.package_fields.push(line.to_owned());
        self
    }

    /// Appends tables to `root`'s manifest, such as `[workspace]`.
    pub fn manifest_tail(mut self, tables: &str) -> Case {
        self.manifest_tail.push_str(tables);
        self
    }

    /// Adds a file to the workspace directory at the relative `path`, such
    /// as the manifest of another member.
    pub fn file(mut self, path: &str, contents: &str) -> Case {
        self.files.push((path.to_owned(), contents.to_owned()));
        self
    }

    /// Resolves with `lock` as the previous `Cargo.lock`.
    pub fn previous(mut self, lock: &str) -> Case {
        self.previous = Some(lock.to_owned());
        self
    }

    /// Resolves for this toolchain version ([`ResolveOptions::rust_version`]).
    pub fn rust_version(mut self, rust_version: Version) -> Case {
        self.rust_version = Some(rust_version);
        self
    }

    /// `root`'s `Cargo.toml`.
    fn manifest(&self) -> String {
        let mut manifest = String::from("[package]\nname = \"root\"\nversion = \"1.0.0\"\n");
        for line in &self.package_fields {
            manifest.push_str(line);
            manifest.push('\n');
        }
        let mut tables: BTreeMap<(Option<&str>, &str), Vec<String>> = BTreeMap::new();
        for dep in &self.deps {
            let table = match dep.kind {
                DepKind::Normal => "dependencies",
                DepKind::Build => "build-dependencies",
                DepKind::Dev => "dev-dependencies",
            };
            tables
                .entry((dep.target.as_deref(), table))
                .or_default()
                .push(dep.manifest_entry());
        }
        for ((target, table), entries) in tables {
            let header = match target {
                Some(target) => format!("target.'{target}'.{table}"),
                None => table.to_owned(),
            };
            let _ = write!(manifest, "\n[{header}]\n{}\n", entries.join("\n"));
        }
        manifest.push_str(&self.manifest_tail);
        manifest
    }

    /// Writes the index and the workspace to fresh temporary directories
    /// and resolves. The outer error is a broken fixture; the inner result
    /// is the resolution's.
    pub fn resolve(&self) -> anyhow::Result<Result<Lockfile, ResolveError>> {
        let index_dir = TempDir::new()?;
        write_index(index_dir.path(), &self.registry)?;
        let workspace_dir = write_workspace(&self.manifest(), &self.files)?;
        let workspace = Workspace::load(workspace_dir.path())?;
        let options = ResolveOptions {
            previous: self.previous.as_deref().map(Lockfile::parse).transpose()?,
            rust_version: self.rust_version.clone(),
        };
        let source = IndexSource::Directory(index_dir.path().to_owned());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(runtime.block_on(resolve_workspace(&workspace, &source, &options)))
    }

    /// Resolves the lock file, expected to succeed, then the features of
    /// the build `configure` makes of [`FeatureRequest::new`] for
    /// [`linux`]. The outer error is a broken fixture or a failed lock
    /// resolution; the inner result is the feature resolution's.
    pub fn features(
        &self,
        configure: impl FnOnce(&mut FeatureRequest) -> anyhow::Result<()>,
    ) -> anyhow::Result<Result<ResolvedFeatures, FeatureError>> {
        let index_dir = TempDir::new()?;
        write_index(index_dir.path(), &self.registry)?;
        let workspace_dir = write_workspace(&self.manifest(), &self.files)?;
        let workspace = Workspace::load(workspace_dir.path())?;
        let source = IndexSource::Directory(index_dir.path().to_owned());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let lockfile = runtime.block_on(resolve_workspace(
            &workspace,
            &source,
            &ResolveOptions::default(),
        ))?;
        let mut request = FeatureRequest::new(linux()?);
        configure(&mut request)?;
        Ok(runtime.block_on(resolve_features(&workspace, &source, &lockfile, &request)))
    }
}

/// `x86_64-unknown-linux-gnu`, with the cfg values rustc prints for it.
pub fn linux() -> anyhow::Result<TargetPlatform> {
    Ok(TargetPlatform::new(
        "x86_64-unknown-linux-gnu",
        "target_arch=\"x86_64\"\ntarget_env=\"gnu\"\ntarget_family=\"unix\"\ntarget_os=\"linux\"\ntarget_pointer_width=\"64\"\nunix\n",
    )?)
}

/// `x86_64-pc-windows-msvc`, with the cfg values rustc prints for it.
pub fn windows() -> anyhow::Result<TargetPlatform> {
    Ok(TargetPlatform::new(
        "x86_64-pc-windows-msvc",
        "target_arch=\"x86_64\"\ntarget_env=\"msvc\"\ntarget_family=\"windows\"\ntarget_os=\"windows\"\ntarget_pointer_width=\"64\"\nwindows\n",
    )?)
}

/// The id of registry package `id`.
pub fn registry_id(id: impl ToPkgId) -> anyhow::Result<PackageId> {
    let (name, version) = id.to_pkg_id();
    Ok(PackageId {
        name: PackageName::new(&name),
        version: Version::parse(&version)?,
        source: PackageSource::Registry,
    })
}

/// Every package a build compiles as sorted `name version: features`
/// lines, `(host)` after the version for the host feature set and no
/// colon without features.
pub fn units(resolved: &ResolvedFeatures) -> String {
    let mut lines = String::new();
    for (package, features_for) in resolved.packages() {
        let side = match features_for {
            FeaturesFor::Target => "",
            FeaturesFor::Host => " (host)",
        };
        let features: Vec<&str> = resolved
            .features(package, features_for)
            .into_iter()
            .flatten()
            .map(|feature| feature.as_str())
            .collect();
        let _ = write!(lines, "{} {}{side}", package.name, package.version);
        if !features.is_empty() {
            let _ = write!(lines, ": {}", features.join(", "));
        }
        lines.push('\n');
    }
    lines
}

/// Resolves `root` depending on `deps` against `registry`.
pub fn resolve<D: ToDep>(
    registry: Vec<Pkg>,
    deps: impl IntoIterator<Item = D>,
) -> anyhow::Result<Result<Lockfile, ResolveError>> {
    case(registry, deps).resolve()
}

/// [`resolve`], expected to succeed, rendered by [`names`].
pub fn resolve_names<D: ToDep>(
    registry: Vec<Pkg>,
    deps: impl IntoIterator<Item = D>,
) -> anyhow::Result<String> {
    Ok(names(&resolve(registry, deps)??))
}

/// The selected packages as sorted `name version` lines, `root 1.0.0`
/// included.
pub fn names(lockfile: &Lockfile) -> String {
    let packages: BTreeSet<(&str, &Version)> = lockfile
        .packages()
        .iter()
        .map(|package| (package.id.name.as_str(), &package.id.version))
        .collect();
    packages
        .into_iter()
        .map(|(name, version)| format!("{name} {version}\n"))
        .collect()
}

/// Loads a workspace whose root manifest is `manifest`.
pub fn load_manifest(manifest: &str) -> anyhow::Result<Result<Workspace, ManifestError>> {
    let workspace_dir = write_workspace(manifest, &[])?;
    Ok(Workspace::load(workspace_dir.path()))
}

/// Writes `registry` as index files with the sparse layout (`1/a`, `2/ab`,
/// `3/a/abc`, `ab/cd/abcd`), one file per lowercased name.
fn write_index(root: &Path, registry: &[Pkg]) -> anyhow::Result<()> {
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for package in registry {
        let file = files
            .entry(index_path(&package.name.to_lowercase()))
            .or_default();
        file.push_str(&package.index_line());
        file.push('\n');
    }
    for (path, contents) in files {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(())
}

/// The sparse-layout path of the index file of the lowercased `name`.
fn index_path(name: &str) -> String {
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

/// A fresh directory holding a package with `manifest` as `Cargo.toml`,
/// and `files` at their relative paths.
fn write_workspace(manifest: &str, files: &[(String, String)]) -> anyhow::Result<TempDir> {
    let directory = TempDir::new()?;
    std::fs::write(directory.path().join("Cargo.toml"), manifest)?;
    std::fs::create_dir_all(directory.path().join("src"))?;
    std::fs::write(directory.path().join("src").join("lib.rs"), "")?;
    for (path, contents) in files {
        let path = directory.path().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(directory)
}

/// A TOML basic string of an identifier or version requirement, which
/// need no escapes.
fn quote(text: &str) -> String {
    format!("\"{text}\"")
}
