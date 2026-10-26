//! The cases of cargo's `resolver-tests/tests/pubgrub.rs`, under the same
//! names: small registries reduced from real crates that exercise renamed
//! dependencies, optional dependencies, feature shadowing, weak dependency
//! features, `links` and cycles. Each case gives the verdict of cargo's own
//! resolver. Where cargo fails although a solution exists (dependency
//! cycles, and weak `d?/f` features, which cargo's resolver treats as
//! enabling `d`), the case fails here too, because a lock file cargo would
//! refuse to produce is one cargo rewrites.

mod common;

use common::{ToDep, dep, dep_kind, dep_req, dep_req_kind, pkg, resolve, resolve_names};
use resolvo_cargo::{DepKind, ResolveError};

#[test]
fn test_01_renamed_package() -> anyhow::Result<()> {
    let registry = vec![
        pkg("a")
            .deps(["b".opt().rename("b_package")])
            .feature("default", &["b_package"]),
        pkg("b"),
    ];
    let resolved = resolve_names(registry, ["a".with(&["default"])])?;
    insta::assert_snapshot!(resolved, @"
    a 1.0.0
    b 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn test_02_renamed_package_no_shadowing() -> anyhow::Result<()> {
    let registry = vec![
        pkg("url"),
        pkg("wasmi").deps(["wasmparser-nostd".rename("wasmparser")]),
        pkg("wasmparser").deps(["url"]),
    ];
    let result = resolve(registry, [dep("wasmi")])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_03_prerelease_semver() -> anyhow::Result<()> {
    let registry = vec![
        pkg("parking_lot_core").deps([dep_req("smallvec", "^1.6.1")]),
        pkg(("smallvec", "2.0.0-alpha.3")),
        pkg(("tokio", "1.35.1"))
            .deps(["parking_lot_core".opt()])
            .feature("default", &["parking_lot_core"]),
    ];
    let result = resolve(registry, ["tokio".with(&["default"])])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_04_cyclic_features() -> anyhow::Result<()> {
    let registry = vec![
        pkg("windows")
            .feature("Win32", &["Win32_Foundation"])
            .feature("Win32_Foundation", &["Win32"]),
    ];
    let resolved = resolve_names(registry, ["windows".with(&["Win32_Foundation"])])?;
    insta::assert_snapshot!(resolved, @"
    root 1.0.0
    windows 1.0.0
    ");
    Ok(())
}

#[test]
fn test_05_cyclic_optional_dependencies() -> anyhow::Result<()> {
    let registry = vec![
        pkg("async-global-executor").deps(["io-lifetimes".opt()]),
        pkg("io-lifetimes").deps(["test".with(&["async-global-executor"])]),
        pkg("test").deps(["async-global-executor".opt().with(&["io-lifetimes"])]),
    ];
    let resolved = resolve_names(registry, [dep("test")])?;
    insta::assert_snapshot!(resolved, @"
    root 1.0.0
    test 1.0.0
    ");
    Ok(())
}

#[test]
fn test_06_cyclic_dependencies() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("a", "1.0.0")),
        pkg(("a", "1.0.1")).deps([dep("dep")]),
        pkg("dep").deps([dep("a")]),
    ];
    let result = resolve(registry, [dep("dep")])?;
    assert!(
        matches!(&result, Err(ResolveError::Cycle { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_07_self_dependency() -> anyhow::Result<()> {
    let registry = vec![pkg("dep").deps([dep("dep")])];
    let result = resolve(registry, [dep("dep")])?;
    assert!(
        matches!(&result, Err(ResolveError::Cycle { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_08_activated_optional_self_dependency() -> anyhow::Result<()> {
    let registry = vec![pkg("a").deps(["a".opt()])];
    let result = resolve(registry, ["a".with(&["a"])])?;
    assert!(
        matches!(&result, Err(ResolveError::Cycle { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_09_build_dependency_with_same_name() -> anyhow::Result<()> {
    let registry = vec![
        pkg("memchr"),
        pkg(("regex", "1.4.6"))
            .deps(["memchr".opt()])
            .feature("default", &["memchr"]),
        pkg("sv-parser").deps(["regex".with(&["default"])]),
        pkg("svlint").deps([
            dep_req("regex", "^1.5"),
            dep_req_kind("regex", "^1", DepKind::Build),
            dep("sv-parser"),
        ]),
    ];
    let result = resolve(registry, [dep("svlint")])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_10_root_dev_dependency_with_same_name() -> anyhow::Result<()> {
    let registry = vec![pkg(("root", "1.0.1"))];
    let resolved = resolve_names(
        registry,
        [dep_req_kind("root", "=1.0.1", DepKind::Dev).rename("root101")],
    )?;
    insta::assert_snapshot!(resolved, @"
    root 1.0.0
    root 1.0.1
    ");
    Ok(())
}

#[test]
fn test_11_dev_dependency() -> anyhow::Result<()> {
    let registry = vec![
        pkg("burn-core")
            .deps([dep_kind("burn-ndarray", DepKind::Dev)])
            .feature("default", &["burn-ndarray/std"]),
    ];
    let resolved = resolve_names(registry, ["burn-core".with(&["default"])])?;
    insta::assert_snapshot!(resolved, @"
    burn-core 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn test_12_weak_dependencies() -> anyhow::Result<()> {
    let registry = vec![
        pkg("borsh")
            .deps(["borsh-derive".opt()])
            .feature("std", &[]),
        pkg("rust_decimal")
            .deps(["borsh".opt().with(&["borsh-derive"])])
            .feature("default", &["borsh?/std"]),
    ];
    let result = resolve(registry, ["rust_decimal".with(&["default"])])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_13_weak_dependencies() -> anyhow::Result<()> {
    let registry = vec![
        pkg("memchr")
            .deps(["std".opt()])
            .feature("std", &["dep:std"]),
        pkg("winnow")
            .deps(["memchr".opt()])
            .feature("default", &["memchr?/std"])
            .feature("simd", &["dep:memchr"]),
    ];
    let result = resolve(registry, ["winnow".with(&["default"])])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_14_weak_dependencies() -> anyhow::Result<()> {
    let registry = vec![
        pkg("a").deps([dep("bad")]),
        pkg("b")
            .deps(["a".opt()])
            .feature("perf-literal", &["dep:a"]),
        pkg("c")
            .deps(["b".opt()])
            .feature("perf-literal", &["b?/perf-literal"])
            .feature("perf-literal-multisubstring", &["dep:b"]),
        pkg("dep")
            .deps([dep("c")])
            .feature("default", &["c/perf-literal"]),
    ];
    let result = resolve(registry, ["dep".with(&["default"])])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_15_duplicate_sys_crate() -> anyhow::Result<()> {
    let registry = vec![
        pkg("js").links("js"),
        pkg("dep").links("js").deps([dep("js")]),
    ];
    let result = resolve(registry, [dep("dep")])?;
    assert!(
        matches!(&result, Err(ResolveError::LinksConflict { links, .. }) if links == "js"),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_16_missing_optional_dependency() -> anyhow::Result<()> {
    let registry = vec![
        pkg("b").deps(["c".opt()]),
        pkg("dep").deps([dep("b")]).feature("d", &["b/c"]),
    ];
    let result = resolve(registry, ["dep".with(&["d"])])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_17_feature_shadowing_missing_optional_dependency() -> anyhow::Result<()> {
    let registry = vec![
        pkg("rustix")
            .deps(["alloc".opt()])
            .feature("alloc", &[])
            .feature("default", &["alloc"])
            .feature("rustc-dep-of-std", &["dep:alloc"]),
    ];
    let resolved = resolve_names(registry, ["rustix".with(&["default"])])?;
    insta::assert_snapshot!(resolved, @"
    root 1.0.0
    rustix 1.0.0
    ");
    Ok(())
}

#[test]
fn test_18_feature_shadowing_activated_optional_dependency() -> anyhow::Result<()> {
    let registry = vec![
        pkg("alloc").deps([dep("bad")]),
        pkg("rustix")
            .deps(["alloc".opt()])
            .feature("alloc", &[])
            .feature("default", &["dep:alloc"])
            .feature("rustc-dep-of-std", &["alloc"]),
    ];
    let result = resolve(registry, ["rustix".with(&["default"])])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_19_same_dep_twice_feature_unification() -> anyhow::Result<()> {
    let registry = vec![
        pkg("iced")
            .deps(["iced_wgpu".opt(), "iced_wgpu".opt().with(&["webgl"])])
            .feature("default", &["iced_wgpu"]),
        pkg("iced_wgpu").feature("webgl", &[]),
    ];
    let resolved = resolve_names(registry, ["iced".with(&["default"])])?;
    insta::assert_snapshot!(resolved, @"
    iced 1.0.0
    iced_wgpu 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn test_20_no_implicit_feature() -> anyhow::Result<()> {
    let registry = vec![
        pkg("c"),
        pkg("ureq").deps(["c".opt()]).feature("cookies", &["dep:c"]),
        pkg("dep")
            .deps([dep("ureq")])
            .feature("cookies", &["ureq/c"]),
    ];
    let result = resolve(registry, ["dep".with(&["cookies"])])?;
    assert!(
        matches!(&result, Err(ResolveError::MissingFeature { package, feature, .. })
            if package.as_str() == "ureq" && feature.as_str() == "c"),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_21_implicit_feature() -> anyhow::Result<()> {
    let registry = vec![
        pkg("c"),
        pkg("ureq").deps(["c".opt()]),
        pkg("dep")
            .deps([dep("ureq")])
            .feature("cookies", &["ureq/c"]),
    ];
    let resolved = resolve_names(registry, ["dep".with(&["cookies"])])?;
    insta::assert_snapshot!(resolved, @"
    c 1.0.0
    dep 1.0.0
    root 1.0.0
    ureq 1.0.0
    ");
    Ok(())
}

#[test]
fn test_22_missing_explicit_default_feature() -> anyhow::Result<()> {
    let registry = vec![
        pkg("fuel-tx")
            .deps([dep("serde"), "serde_json".opt()])
            .feature("default", &["serde/default"])
            .feature("serde", &["serde_json"]),
        pkg("serde"),
    ];
    let result = resolve(registry, ["fuel-tx".with(&["default"])])?;
    assert!(
        matches!(&result, Err(ResolveError::MissingFeature { package, feature, .. })
            if package.as_str() == "serde" && feature.as_str() == "default"),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_23_no_need_for_explicit_default_feature() -> anyhow::Result<()> {
    let registry = vec![
        pkg("a"),
        pkg("b")
            .deps(["a".with_default()])
            .feature("default", &["std"])
            .feature("std", &[]),
    ];
    let resolved = resolve_names(registry, ["b".with(&["default"])])?;
    insta::assert_snapshot!(resolved, @"
    a 1.0.0
    b 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn test_24_dep_feature() -> anyhow::Result<()> {
    let registry = vec![
        pkg("proc-macro2").feature("proc-macro", &[]),
        pkg("syn")
            .deps([dep("proc-macro2")])
            .feature("proc-macro", &["proc-macro2/proc-macro"]),
        pkg("serde_derive").deps(["syn".with(&["proc-macro"])]),
    ];
    let resolved = resolve_names(registry, [dep("serde_derive")])?;
    insta::assert_snapshot!(resolved, @"
    proc-macro2 1.0.0
    root 1.0.0
    serde_derive 1.0.0
    syn 1.0.0
    ");
    Ok(())
}

#[test]
fn test_25_dep_feature() -> anyhow::Result<()> {
    let registry = vec![
        pkg("proc-macro2").feature("proc-macro", &[]),
        pkg("syn")
            .deps([dep("proc-macro2")])
            .feature("proc-macro", &["proc-macro2/proc-macro"]),
    ];
    let resolved = resolve_names(registry, ["syn".with(&["proc-macro"])])?;
    insta::assert_snapshot!(resolved, @"
    proc-macro2 1.0.0
    root 1.0.0
    syn 1.0.0
    ");
    Ok(())
}

#[test]
fn test_26_implicit_feature_with_dep_feature() -> anyhow::Result<()> {
    let registry = vec![
        pkg("quote").feature("proc-macro", &[]),
        pkg("syn")
            .deps(["quote".opt()])
            .feature("default", &["quote", "quote/proc-macro"]),
    ];
    let resolved = resolve_names(registry, ["syn".with(&["default"])])?;
    insta::assert_snapshot!(resolved, @"
    quote 1.0.0
    root 1.0.0
    syn 1.0.0
    ");
    Ok(())
}

#[test]
fn test_27_dep_feature_activating_shadowing_feature() -> anyhow::Result<()> {
    let registry = vec![
        pkg("a")
            .deps(["b".opt(), "x".opt()])
            .feature("b", &["x"])
            .feature("default", &["b/native"]),
        pkg("b").feature("native", &[]),
    ];
    let result = resolve(registry, ["a".with(&["default"])])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn test_28_dep_feature_not_activating_shadowing_feature() -> anyhow::Result<()> {
    let registry = vec![
        pkg("fuel-tx")
            .deps([dep("serde"), "serde_json".opt()])
            .feature("default", &["serde/default"])
            .feature("serde", &["serde_json"]),
        pkg("serde").feature("default", &[]),
    ];
    let resolved = resolve_names(registry, ["fuel-tx".with(&["default"])])?;
    insta::assert_snapshot!(resolved, @"
    fuel-tx 1.0.0
    root 1.0.0
    serde 1.0.0
    ");
    Ok(())
}
