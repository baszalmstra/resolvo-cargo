//! The parts of a `Cargo.toml` that resolution reads, as written: nothing
//! is inherited or normalized here. Keys this model does not name are
//! ignored, as cargo ignores them with an "unused manifest key" warning.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, de};

/// A dependency table: dependency key to its specification.
pub(super) type DependencyTable = BTreeMap<String, TomlDependency>;

/// A whole `Cargo.toml`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct TomlManifest {
    pub(super) package: Option<TomlPackage>,
    pub(super) workspace: Option<TomlWorkspace>,
    pub(super) dependencies: Option<DependencyTable>,
    pub(super) dev_dependencies: Option<DependencyTable>,
    #[serde(rename = "dev_dependencies")]
    pub(super) dev_dependencies_underscore: Option<DependencyTable>,
    pub(super) build_dependencies: Option<DependencyTable>,
    #[serde(rename = "build_dependencies")]
    pub(super) build_dependencies_underscore: Option<DependencyTable>,
    pub(super) target: Option<BTreeMap<String, TomlPlatform>>,
    pub(super) features: Option<BTreeMap<String, Vec<String>>>,
    pub(super) patch: Option<toml::Table>,
    pub(super) replace: Option<toml::Table>,
    pub(super) lib: Option<TomlLib>,
}

/// The `[lib]` table, for whether the library is a procedural macro.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct TomlLib {
    pub(super) proc_macro: Option<bool>,
    #[serde(rename = "proc_macro")]
    pub(super) proc_macro_underscore: Option<bool>,
}

impl TomlLib {
    /// `proc-macro`, else its older spelling `proc_macro`.
    pub(super) fn is_proc_macro(&self) -> bool {
        self.proc_macro
            .or(self.proc_macro_underscore)
            .unwrap_or(false)
    }
}

impl TomlManifest {
    /// Whether some field says `workspace = true`, so normalizing the
    /// package needs the inheritable fields of its workspace root.
    pub(super) fn inherits(&self) -> bool {
        let package_inherits = self.package.as_ref().is_some_and(|package| {
            [&package.version, &package.edition, &package.rust_version]
                .into_iter()
                .flatten()
                .any(Inheritable::is_inherit)
        });
        let table_inherits = |table: &Option<DependencyTable>| {
            table
                .iter()
                .flat_map(BTreeMap::values)
                .any(TomlDependency::is_inherited)
        };
        let platform_tables = self
            .target
            .iter()
            .flat_map(BTreeMap::values)
            .flat_map(|platform| {
                [
                    &platform.dependencies,
                    &platform.dev_dependencies,
                    &platform.dev_dependencies_underscore,
                    &platform.build_dependencies,
                    &platform.build_dependencies_underscore,
                ]
            });
        package_inherits
            || [
                &self.dependencies,
                &self.dev_dependencies,
                &self.dev_dependencies_underscore,
                &self.build_dependencies,
                &self.build_dependencies_underscore,
            ]
            .into_iter()
            .chain(platform_tables)
            .any(table_inherits)
    }
}

/// `[package]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct TomlPackage {
    pub(super) name: Option<String>,
    pub(super) version: Option<Inheritable<String>>,
    pub(super) edition: Option<Inheritable<String>>,
    pub(super) rust_version: Option<Inheritable<String>>,
    pub(super) links: Option<String>,
    pub(super) resolver: Option<String>,
    /// `package.workspace`: the directory of the workspace root, relative
    /// to the package.
    pub(super) workspace: Option<String>,
}

/// `[workspace]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct TomlWorkspace {
    pub(super) members: Option<Vec<String>>,
    pub(super) exclude: Option<Vec<String>>,
    pub(super) resolver: Option<String>,
    pub(super) package: Option<TomlWorkspacePackage>,
    pub(super) dependencies: Option<DependencyTable>,
}

/// `[workspace.package]`: the package fields members may inherit.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct TomlWorkspacePackage {
    pub(super) version: Option<String>,
    pub(super) edition: Option<String>,
    pub(super) rust_version: Option<String>,
}

/// `[target.'<cfg>']`: the dependency tables limited to a platform.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct TomlPlatform {
    pub(super) dependencies: Option<DependencyTable>,
    pub(super) dev_dependencies: Option<DependencyTable>,
    #[serde(rename = "dev_dependencies")]
    pub(super) dev_dependencies_underscore: Option<DependencyTable>,
    pub(super) build_dependencies: Option<DependencyTable>,
    #[serde(rename = "build_dependencies")]
    pub(super) build_dependencies_underscore: Option<DependencyTable>,
}

/// A package field that is either written out or `field.workspace = true`.
#[derive(Debug)]
pub(super) enum Inheritable<T> {
    /// The value as written.
    Value(T),
    /// `workspace = true`: the value of `[workspace.package]`.
    Inherit,
}

impl<T> Inheritable<T> {
    /// Whether it is `workspace = true`.
    pub(super) fn is_inherit(&self) -> bool {
        match self {
            Inheritable::Value(_) => false,
            Inheritable::Inherit => true,
        }
    }
}

impl<'de, T: de::DeserializeOwned> Deserialize<'de> for Inheritable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = toml::Value::deserialize(deserializer)?;
        if let toml::Value::Table(table) = &value
            && let Some(workspace) = table.get("workspace")
        {
            return parse_workspace_flag(workspace).map(|()| Inheritable::Inherit);
        }
        value
            .try_into()
            .map(Inheritable::Value)
            .map_err(de::Error::custom)
    }
}

/// A dependency specification: `dep = "1.0"` or a table.
#[derive(Debug, Clone)]
pub(super) enum TomlDependency {
    /// `dep = "1.0"`: a registry dependency with a version requirement.
    Simple(String),
    /// `dep = { ... }`, including `dep.workspace = true`.
    Detailed(Box<TomlDetailedDependency>),
}

impl TomlDependency {
    /// Whether it is `dep.workspace = true`.
    pub(super) fn is_inherited(&self) -> bool {
        match self {
            TomlDependency::Simple(_) => false,
            TomlDependency::Detailed(detailed) => detailed.workspace.is_some(),
        }
    }

    /// The table form; `"1.0"` becomes `{ version = "1.0" }`.
    pub(super) fn into_detailed(self) -> TomlDetailedDependency {
        match self {
            TomlDependency::Simple(version) => TomlDetailedDependency {
                version: Some(version),
                ..TomlDetailedDependency::default()
            },
            TomlDependency::Detailed(detailed) => *detailed,
        }
    }
}

impl<'de> Deserialize<'de> for TomlDependency {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match toml::Value::deserialize(deserializer)? {
            toml::Value::String(version) => Ok(TomlDependency::Simple(version)),
            toml::Value::Table(table) => {
                if let Some(workspace) = table.get("workspace") {
                    parse_workspace_flag(workspace)?;
                }
                toml::Value::Table(table)
                    .try_into()
                    .map(|detailed| TomlDependency::Detailed(Box::new(detailed)))
                    .map_err(de::Error::custom)
            }
            other => Err(de::Error::invalid_type(
                de::Unexpected::Other(other.type_str()),
                &"a version string like \"0.9.8\" or a detailed dependency table",
            )),
        }
    }
}

/// Accepts `workspace = true` only, as cargo does.
fn parse_workspace_flag<E: de::Error>(value: &toml::Value) -> Result<(), E> {
    match value {
        toml::Value::Boolean(true) => Ok(()),
        toml::Value::Boolean(false) => Err(E::custom("`workspace` cannot be false")),
        other => Err(E::invalid_type(
            de::Unexpected::Other(other.type_str()),
            &"`workspace = true`",
        )),
    }
}

/// The table form of a dependency, every key resolution or rejection
/// needs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct TomlDetailedDependency {
    pub(super) version: Option<String>,
    pub(super) path: Option<String>,
    pub(super) git: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) tag: Option<String>,
    pub(super) rev: Option<String>,
    pub(super) registry: Option<String>,
    pub(super) registry_index: Option<String>,
    pub(super) package: Option<String>,
    pub(super) features: Option<Vec<String>>,
    pub(super) optional: Option<bool>,
    pub(super) default_features: Option<bool>,
    #[serde(rename = "default_features")]
    pub(super) default_features_underscore: Option<bool>,
    pub(super) public: Option<toml::Value>,
    pub(super) artifact: Option<toml::Value>,
    pub(super) lib: Option<toml::Value>,
    pub(super) target: Option<toml::Value>,
    pub(super) base: Option<String>,
    /// `workspace = true`; `false` is rejected while deserializing.
    pub(super) workspace: Option<bool>,
}

impl TomlDetailedDependency {
    /// `default-features`, else its deprecated spelling `default_features`.
    pub(super) fn default_features(&self) -> Option<bool> {
        self.default_features.or(self.default_features_underscore)
    }
}
