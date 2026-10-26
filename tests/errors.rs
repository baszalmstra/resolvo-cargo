//! Failures a caller tells apart without reading messages: why no
//! resolution exists, which manifest feature is not supported, and why a
//! `Cargo.lock` cannot be read. Unsolvable resolutions explain themselves
//! in cargo's terms, packages and their features, not solver internals.

mod common;

use common::{ToDep, dep, dep_req, dep_req_kind, load_manifest, pkg, resolve};
use resolvo_cargo::{
    DepKind, Lockfile, LockfileError, ManifestError, ResolveError, UnsupportedFeature,
};

/// The manifest of package `root 1.0.0` followed by `tables`.
fn manifest(tables: &str) -> String {
    format!("[package]\nname = \"root\"\nversion = \"1.0.0\"\n\n{tables}")
}

/// The unsupported feature loading a workspace with `tables` in its root
/// manifest fails with.
fn unsupported(tables: &str) -> anyhow::Result<UnsupportedFeature> {
    match load_manifest(&manifest(tables))? {
        Err(ManifestError::Unsupported { feature, .. }) => Ok(feature),
        other => anyhow::bail!("expected an unsupported feature, got {other:?}"),
    }
}

#[test]
fn only_yanked_versions_match() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("x", "1.0.0")).yanked(),
        pkg(("x", "1.1.0")).yanked(),
        pkg(("x", "2.0.0")),
    ];
    let result = resolve(registry, [dep_req("x", "1")])?;
    assert!(
        matches!(&result, Err(ResolveError::OnlyYanked { package, .. }) if package.as_str() == "x"),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn a_feature_requested_by_the_manifest_is_missing() -> anyhow::Result<()> {
    let registry = vec![pkg(("x", "1.0.0")).feature("g", &[])];
    let result = resolve(registry, ["x".with(&["f"])])?;
    assert!(
        matches!(&result, Err(ResolveError::MissingFeature { package, feature, .. })
            if package.as_str() == "x" && feature.as_str() == "f"),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn two_ranges_of_a_sys_crate_link_the_same_library() -> anyhow::Result<()> {
    let registry = vec![pkg(("z-sys", "0.1.0")), pkg(("z-sys", "0.2.0"))];
    let result = resolve(
        registry,
        [
            dep_req("z-sys", "0.1"),
            dep_req("z-sys", "0.2").rename("z-sys-2"),
        ],
    )?;
    assert!(
        matches!(&result, Err(ResolveError::LinksConflict { links, .. }) if links == "z-sys"),
        "{result:?}"
    );
    Ok(())
}

/// Only dev-dependencies may close a cycle; a build-dependency may not.
#[test]
fn a_cycle_through_a_build_dependency() -> anyhow::Result<()> {
    let registry = vec![
        pkg("a").deps([dep_req_kind("b", "*", DepKind::Build)]),
        pkg("b").deps([dep("a")]),
    ];
    let result = resolve(registry, [dep("a")])?;
    let Err(error @ ResolveError::Cycle { .. }) = result else {
        anyhow::bail!("expected a cycle, got {result:?}");
    };
    insta::assert_snapshot!(error.to_string(), @"cyclic package dependency: `a v1.0.0` -> `b v1.0.0` -> `a v1.0.0`");
    Ok(())
}

/// `a` needs `x` with feature `f`, which only `x 1.0.0` has, while `root`
/// pins `x 1.1.0` of the same semver range.
#[test]
fn a_conflict_is_explained_in_cargo_terms() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("x", "1.0.0")).feature("f", &[]),
        pkg(("x", "1.1.0")),
        pkg("a").deps([dep_req("x", "1").features(&["f"])]),
    ];
    let result = resolve(registry, [dep("a"), dep_req("x", "=1.1.0")])?;
    let Err(error @ ResolveError::Conflict { .. }) = result else {
        anyhow::bail!("expected a conflict, got {result:?}");
    };
    insta::assert_snapshot!(error.to_string(), @"
    failed to select a version for the requirements
    The following packages are incompatible
    └─ package `root` =1.0.0 cannot be installed because there are no viable options:
       └─ package `root` 1.0.0 would require
          ├─ package `x` =1.1.0, which can be installed with any of the following options:
          │  └─ package `x` 1.1.0
          └─ package `a` *, which cannot be installed because there are no viable options:
             └─ package `a` 1.0.0 would require
                └─ package `x` with feature `f` ^1, which cannot be installed because there are no viable options:
                   ├─ package `x` with feature `f` 1.1.0 is excluded because it does not have the feature
                   └─ package `x` with feature `f` 1.0.0 would require
                      └─ package `x` =1.0.0, which cannot be installed because there are no viable options:
                         └─ package `x` 1.0.0, which conflicts with the versions reported above.
    ");
    Ok(())
}

#[test]
fn a_git_dependency_is_unsupported() -> anyhow::Result<()> {
    let feature = unsupported("[dependencies]\nx = { git = \"https://github.com/example/x\" }\n")?;
    assert!(
        matches!(&feature, UnsupportedFeature::GitDependency(dep) if dep.as_str() == "x"),
        "{feature:?}"
    );
    Ok(())
}

#[test]
fn an_alternative_registry_is_unsupported() -> anyhow::Result<()> {
    let feature =
        unsupported("[dependencies]\nx = { version = \"1\", registry = \"internal\" }\n")?;
    assert!(
        matches!(&feature, UnsupportedFeature::AlternativeRegistry(dep) if dep.as_str() == "x"),
        "{feature:?}"
    );
    Ok(())
}

#[test]
fn a_public_dependency_is_unsupported() -> anyhow::Result<()> {
    let feature = unsupported("[dependencies]\nx = { version = \"1\", public = true }\n")?;
    assert!(
        matches!(&feature, UnsupportedFeature::PublicDependency(dep) if dep.as_str() == "x"),
        "{feature:?}"
    );
    Ok(())
}

#[test]
fn an_artifact_dependency_is_unsupported() -> anyhow::Result<()> {
    let feature = unsupported("[dependencies]\nx = { version = \"1\", artifact = \"bin\" }\n")?;
    assert!(
        matches!(&feature, UnsupportedFeature::ArtifactDependency(dep) if dep.as_str() == "x"),
        "{feature:?}"
    );
    Ok(())
}

#[test]
fn a_path_base_is_unsupported() -> anyhow::Result<()> {
    let feature = unsupported("[dependencies]\nx = { path = \"x\", base = \"vendored\" }\n")?;
    assert!(
        matches!(&feature, UnsupportedFeature::PathBase(dep) if dep.as_str() == "x"),
        "{feature:?}"
    );
    Ok(())
}

#[test]
fn a_patch_table_is_unsupported() -> anyhow::Result<()> {
    let feature = unsupported("[patch.crates-io]\nx = { path = \"x\" }\n")?;
    assert!(matches!(&feature, UnsupportedFeature::Patch), "{feature:?}");
    Ok(())
}

#[test]
fn a_replace_table_is_unsupported() -> anyhow::Result<()> {
    let feature = unsupported("[replace]\n\"x:1.0.0\" = { path = \"x\" }\n")?;
    assert!(
        matches!(&feature, UnsupportedFeature::Replace),
        "{feature:?}"
    );
    Ok(())
}

#[test]
fn a_lock_file_without_a_format_version_is_unsupported() {
    let result = Lockfile::parse(
        "[[package]]\nname = \"root\"\nversion = \"1.0.0\"\ndependencies = [\n \"x 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)\",\n]\n\n[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[metadata]\n",
    );
    assert!(
        matches!(
            &result,
            Err(LockfileError::UnsupportedVersion { version: None })
        ),
        "{result:?}"
    );
}

#[test]
fn a_locked_git_package_is_unsupported() {
    let result = Lockfile::parse(
        "version = 4\n\n[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"git+https://github.com/example/x#0123456789abcdef0123456789abcdef01234567\"\n",
    );
    assert!(
        matches!(&result, Err(LockfileError::UnsupportedSource { name, .. }) if name.as_str() == "x"),
        "{result:?}"
    );
}
