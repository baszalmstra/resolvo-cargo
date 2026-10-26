//! Finding the members and path packages of a workspace and normalizing
//! each manifest into a [`Summary`], following cargo's
//! `core/workspace.rs` (`find_members`, `find_path_deps`,
//! `members_paths`, `is_excluded`, `set_resolve_behavior`) and
//! `util/toml/mod.rs` (`normalize_toml`, `inner_dependency_inherit_with`,
//! `dep_to_dependency`).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Component, Path, PathBuf},
    rc::Rc,
};

use semver::{Version, VersionReq};
use tracing::{debug, warn};

use super::{
    PathPackage, ResolverVersion, Workspace,
    manifest::{
        DependencyTable, Inheritable, TomlDependency, TomlDetailedDependency, TomlLib,
        TomlManifest, TomlWorkspace, TomlWorkspacePackage,
    },
};
use crate::{
    error::{ManifestError, UnsupportedFeature},
    names::{DepName, FeatureName, PackageName},
    summary::{DepKind, DepSource, Dependency, RustVersion, Summary},
};

const MANIFEST_NAME: &str = "Cargo.toml";

/// Loads the workspace rooted at `root`; see [`Workspace::load`].
pub(super) fn load(root: &Path) -> Result<Workspace, ManifestError> {
    let root = std::path::absolute(root)
        .map(|absolute| normalize_path(&absolute))
        .map_err(|source| ManifestError::Read {
            path: root.join(MANIFEST_NAME),
            source,
        })?;
    let mut loader = Loader {
        root: root.clone(),
        root_config: None,
        manifests: HashMap::new(),
        members: BTreeSet::new(),
        packages: BTreeMap::new(),
    };

    let root_manifest = loader.manifest(&root)?;
    reject_overrides(&root_manifest)?;
    loader.root_config = root_manifest.workspace.clone();
    match loader.root_config.clone() {
        Some(config) => {
            for member in config.members_paths()? {
                loader.find_path_deps(&normalize_path(&member), false)?;
            }
            loader.find_path_deps(&root, false)?;
        }
        None => {
            // No `[workspace]`: the root package is the only member.
            let package = loader.load_package(&root, &root_manifest, true)?;
            loader.members.insert(root.clone());
            loader.packages.insert(root.clone(), package);
        }
    }
    loader.load_non_members()?;

    let resolver = loader.resolver(&root_manifest)?;
    let packages = loader
        .packages
        .into_values()
        .map(|loaded| loaded.package)
        .collect();
    Ok(Workspace {
        root,
        resolver,
        packages,
    })
}

/// A parsed manifest with its `[workspace]` table split off.
struct Manifest {
    /// The `Cargo.toml` file.
    path: PathBuf,
    /// Everything but `[workspace]`.
    toml: TomlManifest,
    /// `[workspace]`, if this manifest is a workspace root.
    workspace: Option<Rc<WorkspaceConfig>>,
}

/// A `[workspace]` table and the directory of its manifest.
struct WorkspaceConfig {
    /// The normalized directory of the root manifest.
    root_dir: PathBuf,
    members: Option<Vec<String>>,
    exclude: Vec<String>,
    resolver: Option<String>,
    package: TomlWorkspacePackage,
    dependencies: Option<DependencyTable>,
}

impl WorkspaceConfig {
    fn new(root_dir: PathBuf, workspace: TomlWorkspace) -> WorkspaceConfig {
        WorkspaceConfig {
            root_dir,
            members: workspace.members,
            exclude: workspace.exclude.unwrap_or_default(),
            resolver: workspace.resolver,
            package: workspace.package.unwrap_or_default(),
            dependencies: workspace.dependencies,
        }
    }

    /// The directories `members` names: each pattern's matching
    /// directories, or the pattern's path itself when nothing matches.
    fn members_paths(&self) -> Result<Vec<PathBuf>, ManifestError> {
        let mut paths = Vec::new();
        for pattern in self.members.iter().flatten() {
            let joined = self.root_dir.join(pattern);
            let mut expanded = Vec::new();
            if let Some(text) = joined.to_str() {
                let matches = glob::glob(text).map_err(|source| ManifestError::MemberPattern {
                    pattern: pattern.clone(),
                    source,
                })?;
                for path in matches {
                    expanded.push(path.map_err(|source| ManifestError::MemberGlob {
                        pattern: pattern.clone(),
                        source,
                    })?);
                }
            }
            if expanded.is_empty() {
                paths.push(joined);
            } else {
                // Filters out files such as `.DS_Store` a glob may match.
                paths.extend(expanded.into_iter().filter(|path| path.is_dir()));
            }
        }
        Ok(paths)
    }

    /// Whether `exclude` excludes the manifest at `manifest_path` and no
    /// `members` entry lists it explicitly.
    fn is_excluded(&self, manifest_path: &Path) -> bool {
        let under = |entries: &[String]| {
            entries
                .iter()
                .any(|entry| manifest_path.starts_with(self.root_dir.join(entry)))
        };
        under(&self.exclude) && !under(self.members.as_deref().unwrap_or_default())
    }
}

/// The edition of a package; only its default resolver matters here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Edition {
    Edition2015,
    Edition2018,
    Edition2021,
    Edition2024,
}

impl Edition {
    fn parse(text: &str, path: &Path, field: &str) -> Result<Edition, ManifestError> {
        match text {
            "2015" => Ok(Edition::Edition2015),
            "2018" => Ok(Edition::Edition2018),
            "2021" => Ok(Edition::Edition2021),
            "2024" => Ok(Edition::Edition2024),
            _ => Err(ManifestError::UnknownValue {
                path: path.to_owned(),
                field: field.to_owned(),
                value: text.to_owned(),
            }),
        }
    }

    fn default_resolver(self) -> ResolverVersion {
        match self {
            Edition::Edition2015 | Edition::Edition2018 => ResolverVersion::V1,
            Edition::Edition2021 => ResolverVersion::V2,
            Edition::Edition2024 => ResolverVersion::V3,
        }
    }
}

/// A normalized path package and the edition its resolver default comes
/// from.
struct LoadedPackage {
    package: PathPackage,
    edition: Edition,
}

/// The state of one [`load`]: parsed manifests and the packages found so
/// far.
struct Loader {
    /// The normalized workspace root directory.
    root: PathBuf,
    /// `[workspace]` of the root manifest.
    root_config: Option<Rc<WorkspaceConfig>>,
    /// Parsed manifests by normalized directory.
    manifests: HashMap<PathBuf, Rc<Manifest>>,
    /// Member directories, virtual manifests included.
    members: BTreeSet<PathBuf>,
    /// Loaded path packages by normalized directory.
    packages: BTreeMap<PathBuf, LoadedPackage>,
}

impl Loader {
    /// The parsed manifest in `directory`, read once.
    fn manifest(&mut self, directory: &Path) -> Result<Rc<Manifest>, ManifestError> {
        if let Some(manifest) = self.manifests.get(directory) {
            return Ok(manifest.clone());
        }
        let path = directory.join(MANIFEST_NAME);
        let text = fs::read_to_string(&path).map_err(|source| ManifestError::Read {
            path: path.clone(),
            source,
        })?;
        let mut toml: TomlManifest =
            toml::from_str(&text).map_err(|source| ManifestError::Parse {
                path: path.clone(),
                source: Box::new(source),
            })?;
        let workspace = toml
            .workspace
            .take()
            .map(|workspace| Rc::new(WorkspaceConfig::new(directory.to_owned(), workspace)));
        let manifest = Rc::new(Manifest {
            path,
            toml,
            workspace,
        });
        self.manifests
            .insert(directory.to_owned(), manifest.clone());
        Ok(manifest)
    }

    /// cargo's `find_path_deps`: adds the package in `directory` as a
    /// member unless it is excluded, or is a path dependency outside the
    /// root, then visits its path dependencies.
    fn find_path_deps(&mut self, directory: &Path, is_path_dep: bool) -> Result<(), ManifestError> {
        if self.members.contains(directory) {
            return Ok(());
        }
        if is_path_dep && !directory.starts_with(&self.root) {
            return Ok(());
        }
        if let Some(config) = &self.root_config
            && config.is_excluded(&directory.join(MANIFEST_NAME))
        {
            return Ok(());
        }
        self.members.insert(directory.to_owned());
        let manifest = self.manifest(directory)?;
        reject_overrides(&manifest)?;
        if manifest.toml.package.is_none() {
            debug!(directory = %directory.display(), "virtual manifest among the workspace members");
            return Ok(());
        }
        let loaded = self.load_package(directory, &manifest, true)?;
        let dependencies = path_dependency_dirs(&loaded.package.summary);
        self.packages.insert(directory.to_owned(), loaded);
        for dependency in dependencies {
            self.find_path_deps(&dependency, true)?;
        }
        Ok(())
    }

    /// Loads every path package the members reach that is not a member
    /// itself, transitively.
    fn load_non_members(&mut self) -> Result<(), ManifestError> {
        let mut pending: Vec<PathBuf> = self
            .packages
            .values()
            .flat_map(|loaded| path_dependency_dirs(&loaded.package.summary))
            .collect();
        while let Some(directory) = pending.pop() {
            if self.packages.contains_key(&directory) {
                continue;
            }
            let manifest = self.manifest(&directory)?;
            let loaded = self.load_package(&directory, &manifest, false)?;
            pending.extend(path_dependency_dirs(&loaded.package.summary));
            self.packages.insert(directory, loaded);
        }
        Ok(())
    }

    /// Normalizes the package in `directory`, resolving `workspace = true`
    /// against its workspace root.
    fn load_package(
        &mut self,
        directory: &Path,
        manifest: &Manifest,
        member: bool,
    ) -> Result<LoadedPackage, ManifestError> {
        let inherit = if manifest.toml.inherits() {
            self.inheritance_root(directory, manifest, member)?
        } else {
            None
        };
        let (summary, edition) = normalize_package(directory, manifest, inherit.as_deref())?;
        Ok(LoadedPackage {
            package: PathPackage {
                directory: directory.to_owned(),
                summary,
                member,
                proc_macro: manifest
                    .toml
                    .lib
                    .as_ref()
                    .is_some_and(TomlLib::is_proc_macro),
            },
            edition,
        })
    }

    /// The workspace root a package inherits from: the loaded root for
    /// members; for other path packages, cargo's `find_root`: the package
    /// itself if it is a root, the root `package.workspace` points to, or
    /// the nearest ancestor root that does not exclude it.
    fn inheritance_root(
        &mut self,
        directory: &Path,
        manifest: &Manifest,
        member: bool,
    ) -> Result<Option<Rc<WorkspaceConfig>>, ManifestError> {
        if member {
            return Ok(self.root_config.clone());
        }
        if let Some(config) = &manifest.workspace {
            return Ok(Some(config.clone()));
        }
        if let Some(package) = &manifest.toml.package
            && let Some(pointer) = &package.workspace
        {
            let root_dir = normalize_path(&directory.join(pointer));
            return Ok(self.manifest(&root_dir)?.workspace.clone());
        }
        for ancestor in directory.ancestors().skip(1) {
            if !ancestor.join(MANIFEST_NAME).is_file() {
                continue;
            }
            if let Some(config) = &self.manifest(ancestor)?.workspace
                && !config.is_excluded(&manifest.path)
            {
                return Ok(Some(config.clone()));
            }
        }
        Ok(None)
    }

    /// cargo's `set_resolve_behavior`: `resolver` of the root manifest's
    /// `[workspace]` or `[package]`, else the root package's edition
    /// default, else `V1` for a virtual workspace.
    fn resolver(&self, root_manifest: &Manifest) -> Result<ResolverVersion, ManifestError> {
        let path = &root_manifest.path;
        let workspace_resolver = self
            .root_config
            .as_ref()
            .and_then(|config| config.resolver.as_deref());
        let package_resolver = root_manifest
            .toml
            .package
            .as_ref()
            .and_then(|package| package.resolver.as_deref());
        let (field, text) = match (workspace_resolver, package_resolver) {
            (Some(_), Some(_)) => {
                return Err(ManifestError::ConflictingResolver { path: path.clone() });
            }
            (Some(text), None) => ("workspace.resolver", text),
            (None, Some(text)) => ("package.resolver", text),
            (None, None) => {
                return Ok(self
                    .packages
                    .get(&self.root)
                    .map_or(ResolverVersion::V1, |root_package| {
                        root_package.edition.default_resolver()
                    }));
            }
        };
        match text {
            "1" => Ok(ResolverVersion::V1),
            "2" => Ok(ResolverVersion::V2),
            "3" => Ok(ResolverVersion::V3),
            _ => Err(ManifestError::UnknownValue {
                path: path.clone(),
                field: field.to_owned(),
                value: text.to_owned(),
            }),
        }
    }
}

/// The directories of the path dependencies of `summary`.
fn path_dependency_dirs(summary: &Summary) -> Vec<PathBuf> {
    summary
        .dependencies()
        .iter()
        .filter_map(|dependency| match &dependency.source {
            DepSource::Path(directory) => Some(directory.clone()),
            DepSource::Registry => None,
        })
        .collect()
}

/// Rejects `[patch]` and `[replace]`, which would change what the
/// registry provides.
fn reject_overrides(manifest: &Manifest) -> Result<(), ManifestError> {
    let unsupported = |feature| ManifestError::Unsupported {
        path: manifest.path.clone(),
        feature,
    };
    if manifest.toml.patch.is_some() {
        return Err(unsupported(UnsupportedFeature::Patch));
    }
    if manifest.toml.replace.is_some() {
        return Err(unsupported(UnsupportedFeature::Replace));
    }
    Ok(())
}

/// cargo's `paths::normalize_path`: removes `.` and resolves `..`
/// lexically, without touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    let mut normalized = match components.peek() {
        Some(prefix @ Component::Prefix(_)) => {
            let prefix = PathBuf::from(prefix.as_os_str());
            components.next();
            prefix
        }
        Some(
            Component::RootDir | Component::CurDir | Component::ParentDir | Component::Normal(_),
        )
        | None => PathBuf::new(),
    };
    for component in components {
        match component {
            // Only the first component can be a prefix.
            Component::Prefix(_) | Component::CurDir => {}
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::ParentDir => {
                // A leading `..` stays when there is nothing to pop.
                if normalized.ends_with(Component::ParentDir)
                    || (!normalized.pop() && !normalized.has_root())
                {
                    normalized.push(Component::ParentDir);
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

/// Turns the manifest of a package into its [`Summary`] and edition.
fn normalize_package(
    directory: &Path,
    manifest: &Manifest,
    inherit: Option<&WorkspaceConfig>,
) -> Result<(Summary, Edition), ManifestError> {
    let path = &manifest.path;
    let toml = &manifest.toml;
    let Some(package) = &toml.package else {
        return Err(ManifestError::NotAPackage { path: path.clone() });
    };
    reject_overrides(manifest)?;
    let name = package
        .name
        .as_deref()
        .map(PackageName::new)
        .ok_or_else(|| ManifestError::MissingField {
            path: path.clone(),
            field: "package.name".to_owned(),
        })?;
    let inherited = |field: &str, value: Option<&String>| {
        value.cloned().ok_or_else(|| ManifestError::NotInherited {
            path: path.clone(),
            field: format!("package.{field}"),
        })
    };
    let workspace_package = inherit.map(|config| &config.package);
    let field_value = |field: &str,
                       value: &Option<Inheritable<String>>,
                       from_workspace: fn(&TomlWorkspacePackage) -> Option<&String>|
     -> Result<Option<String>, ManifestError> {
        match value {
            None => Ok(None),
            Some(Inheritable::Value(text)) => Ok(Some(text.clone())),
            Some(Inheritable::Inherit) => {
                inherited(field, workspace_package.and_then(from_workspace)).map(Some)
            }
        }
    };

    let edition = match field_value("edition", &package.edition, |workspace| {
        workspace.edition.as_ref()
    })? {
        Some(text) => Edition::parse(&text, path, "package.edition")?,
        None => Edition::Edition2015,
    };
    let version = match field_value("version", &package.version, |workspace| {
        workspace.version.as_ref()
    })? {
        Some(text) => Version::parse(&text).map_err(|source| ManifestError::InvalidVersion {
            path: path.clone(),
            field: "package.version".to_owned(),
            text,
            source,
        })?,
        None => Version::new(0, 0, 0),
    };
    let rust_version = field_value("rust-version", &package.rust_version, |workspace| {
        workspace.rust_version.as_ref()
    })?
    .map(|text| text.parse::<RustVersion>())
    .transpose()
    .map_err(|source| ManifestError::InvalidRustVersion {
        path: path.clone(),
        source,
    })?;

    let context = DependencyContext {
        path,
        directory,
        edition,
        inherit,
    };
    let mut dependencies = Vec::new();
    let tables = [
        (None, toml.dependencies.as_ref(), DepKind::Normal),
        (
            None,
            context.pick_table(
                "dev-dependencies",
                &toml.dev_dependencies,
                &toml.dev_dependencies_underscore,
            )?,
            DepKind::Dev,
        ),
        (
            None,
            context.pick_table(
                "build-dependencies",
                &toml.build_dependencies,
                &toml.build_dependencies_underscore,
            )?,
            DepKind::Build,
        ),
    ];
    for (target, table, kind) in tables {
        context.push_table(&mut dependencies, table, kind, target)?;
    }
    for (platform_name, platform) in toml.target.iter().flatten() {
        let platform_tables = [
            (platform.dependencies.as_ref(), DepKind::Normal),
            (
                context.pick_table(
                    "dev-dependencies",
                    &platform.dev_dependencies,
                    &platform.dev_dependencies_underscore,
                )?,
                DepKind::Dev,
            ),
            (
                context.pick_table(
                    "build-dependencies",
                    &platform.build_dependencies,
                    &platform.build_dependencies_underscore,
                )?,
                DepKind::Build,
            ),
        ];
        for (table, kind) in platform_tables {
            context.push_table(&mut dependencies, table, kind, Some(platform_name))?;
        }
    }

    let features: BTreeMap<FeatureName, Vec<String>> = toml
        .features
        .iter()
        .flatten()
        .map(|(feature, values)| (FeatureName::new(feature.as_str()), values.clone()))
        .collect();
    let summary = Summary::new(
        name,
        version,
        dependencies,
        &features,
        package.links.clone(),
        rust_version,
    )
    .map_err(|source| ManifestError::Features {
        path: path.clone(),
        source,
    })?;
    Ok((summary, edition))
}

/// What normalizing the dependencies of one package needs.
struct DependencyContext<'a> {
    /// The manifest file, for errors.
    path: &'a Path,
    /// The normalized package directory, which `path` keys are relative to.
    directory: &'a Path,
    edition: Edition,
    /// The workspace root `workspace = true` dependencies inherit from.
    inherit: Option<&'a WorkspaceConfig>,
}

impl<'a> DependencyContext<'a> {
    /// cargo's `deprecated_underscore` for a table: the dashed spelling
    /// wins; the underscore spelling is an error from edition 2024.
    fn pick_table<'t>(
        &self,
        key: &str,
        dashed: &'t Option<DependencyTable>,
        underscore: &'t Option<DependencyTable>,
    ) -> Result<Option<&'t DependencyTable>, ManifestError> {
        self.check_underscore(key, "package", dashed.is_some(), underscore.is_some())?;
        Ok(dashed.as_ref().or(underscore.as_ref()))
    }

    fn check_underscore(
        &self,
        key: &str,
        context: &str,
        has_dashed: bool,
        has_underscore: bool,
    ) -> Result<(), ManifestError> {
        if !has_underscore {
            return Ok(());
        }
        let underscore_key = key.replace('-', "_");
        if self.edition >= Edition::Edition2024 {
            return Err(ManifestError::UnderscoreKey {
                path: self.path.to_owned(),
                key: underscore_key,
            });
        }
        if has_dashed {
            warn!(manifest = %self.path.display(), key = %underscore_key, context, "redundant with the dashed spelling, which is preferred");
        } else {
            warn!(manifest = %self.path.display(), key = %underscore_key, context, "deprecated in favor of the dashed spelling");
        }
        Ok(())
    }

    fn push_table(
        &self,
        dependencies: &mut Vec<Dependency>,
        table: Option<&DependencyTable>,
        kind: DepKind,
        target: Option<&String>,
    ) -> Result<(), ManifestError> {
        for (key, specification) in table.into_iter().flatten() {
            dependencies.push(self.dependency(key, specification, kind, target)?);
        }
        Ok(())
    }

    /// cargo's `dependency_inherit_with` followed by `dep_to_dependency`.
    fn dependency(
        &self,
        key: &str,
        specification: &TomlDependency,
        kind: DepKind,
        target: Option<&String>,
    ) -> Result<Dependency, ManifestError> {
        let dep_name = DepName::new(key);
        let (detailed, base_dir) = match specification {
            TomlDependency::Detailed(member) if member.workspace.is_some() => {
                self.inherit_dependency(&dep_name, member)?
            }
            TomlDependency::Simple(_) | TomlDependency::Detailed(_) => {
                (specification.clone().into_detailed(), self.directory)
            }
        };
        self.check_underscore(
            "default-features",
            key,
            detailed.default_features.is_some(),
            detailed.default_features_underscore.is_some(),
        )?;
        self.reject_unsupported(&dep_name, &detailed)?;

        let req = match &detailed.version {
            Some(text) => {
                VersionReq::parse(text).map_err(|source| ManifestError::InvalidVersion {
                    path: self.path.to_owned(),
                    field: format!("dependencies.{key}.version"),
                    text: text.clone(),
                    source,
                })?
            }
            None if detailed.path.is_some() => VersionReq::STAR,
            None => {
                return Err(ManifestError::DependencyWithoutSource {
                    path: self.path.to_owned(),
                    dep: dep_name,
                });
            }
        };
        let source = match &detailed.path {
            Some(relative) => DepSource::Path(normalize_path(&base_dir.join(relative))),
            None => DepSource::Registry,
        };
        let package = PackageName::new(detailed.package.as_deref().unwrap_or(key));
        let features = detailed
            .features
            .iter()
            .flatten()
            .map(|feature| FeatureName::new(feature.as_str()))
            .collect();
        Ok(Dependency {
            name: dep_name,
            package,
            req,
            source,
            features,
            default_features: detailed.default_features().unwrap_or(true),
            optional: detailed.optional.unwrap_or(false),
            kind,
            target: target.cloned(),
        })
    }

    /// cargo's `inner_dependency_inherit_with`: the workspace dependency
    /// with the member's `features` appended and its `optional` and
    /// `public`, and the directory its `path` is relative to.
    fn inherit_dependency(
        &self,
        dep_name: &DepName,
        member: &TomlDetailedDependency,
    ) -> Result<(TomlDetailedDependency, &'a Path), ManifestError> {
        let not_inherited = || ManifestError::NotInherited {
            path: self.path.to_owned(),
            field: format!("workspace.dependencies.{dep_name}"),
        };
        let config = self.inherit.ok_or_else(not_inherited)?;
        let workspace_dependency = config
            .dependencies
            .as_ref()
            .and_then(|dependencies| dependencies.get(dep_name.as_str()))
            .ok_or_else(not_inherited)?;
        self.warn_ignored_inherited_keys(dep_name, member);

        let mut merged = workspace_dependency.clone().into_detailed();
        match (member.default_features(), merged.default_features()) {
            (Some(true), Some(false)) => merged.default_features = Some(true),
            (Some(false), workspace @ (Some(true) | None)) => {
                if self.edition >= Edition::Edition2024 {
                    return Err(ManifestError::InheritedDefaultFeatures {
                        path: self.path.to_owned(),
                        dep: dep_name.clone(),
                    });
                }
                warn!(
                    manifest = %self.path.display(),
                    dependency = %dep_name,
                    workspace_default_features = ?workspace,
                    "`default-features = false` is ignored because the workspace dependency enables default features"
                );
            }
            (Some(true), Some(true) | None) | (Some(false), Some(false)) | (None, _) => {}
        }
        merged.features = match (merged.features.take(), member.features.clone()) {
            (Some(mut workspace), Some(own)) => {
                workspace.extend(own);
                Some(workspace)
            }
            (workspace, own) => workspace.or(own),
        };
        merged.optional = member.optional;
        merged.public = member.public.clone();
        Ok((merged, &config.root_dir))
    }

    /// cargo accepts only `features`, `optional`, `default-features` and
    /// `public` next to `workspace = true` and ignores other keys with a
    /// warning.
    fn warn_ignored_inherited_keys(&self, dep_name: &DepName, member: &TomlDetailedDependency) {
        let keys = [
            ("version", member.version.is_some()),
            ("path", member.path.is_some()),
            ("git", member.git.is_some()),
            ("branch", member.branch.is_some()),
            ("tag", member.tag.is_some()),
            ("rev", member.rev.is_some()),
            ("registry", member.registry.is_some()),
            ("registry-index", member.registry_index.is_some()),
            ("package", member.package.is_some()),
            ("artifact", member.artifact.is_some()),
            ("lib", member.lib.is_some()),
            ("target", member.target.is_some()),
            ("base", member.base.is_some()),
        ];
        for (key, present) in keys {
            if present {
                warn!(manifest = %self.path.display(), dependency = %dep_name, key, "unused manifest key next to `workspace = true`");
            }
        }
    }

    /// Rejects the dependency keys this resolver does not implement.
    fn reject_unsupported(
        &self,
        dep_name: &DepName,
        detailed: &TomlDetailedDependency,
    ) -> Result<(), ManifestError> {
        let feature = if detailed.git.is_some()
            || detailed.branch.is_some()
            || detailed.tag.is_some()
            || detailed.rev.is_some()
        {
            UnsupportedFeature::GitDependency(dep_name.clone())
        } else if detailed.registry.is_some() || detailed.registry_index.is_some() {
            UnsupportedFeature::AlternativeRegistry(dep_name.clone())
        } else if detailed.public.is_some() {
            UnsupportedFeature::PublicDependency(dep_name.clone())
        } else if detailed.artifact.is_some() || detailed.lib.is_some() || detailed.target.is_some()
        {
            UnsupportedFeature::ArtifactDependency(dep_name.clone())
        } else if detailed.base.is_some() {
            UnsupportedFeature::PathBase(dep_name.clone())
        } else {
            return Ok(());
        };
        Err(ManifestError::Unsupported {
            path: self.path.to_owned(),
            feature,
        })
    }
}
