//! Feature resolution for one build of a resolved workspace, as cargo's
//! second resolution pass: which packages are compiled with which
//! features, per resolver version, platform and dev units.

mod common;

use common::{ToDep, case, dep_kind, pkg, registry_id, units, windows};
use resolvo_cargo::{DepKind, FeatureError, FeatureValue, PackageName, RequestedPackage};

/// `root` (edition 2015: resolver 1, or `edition` if given).
fn edition(edition: Option<&str>, case: common::Case) -> common::Case {
    match edition {
        Some(edition) => case.package_field(&format!("edition = \"{edition}\"")),
        None => case,
    }
}

#[test]
fn build_dependencies_keep_features_of_their_own_under_resolver_2() -> anyhow::Result<()> {
    let build = |resolver_2: bool| -> anyhow::Result<String> {
        let registry = vec![
            pkg("a")
                .feature("f", &[])
                .feature("g", &[])
                .deps(["b".opt()])
                .feature("b", &["dep:b"]),
            pkg("b"),
        ];
        let deps = [
            "a".with(&["f"]),
            dep_kind("a", DepKind::Build).with(&["g", "b"]),
        ];
        let case = edition(resolver_2.then_some("2021"), case(registry, deps));
        Ok(units(&case.features(|_| Ok(()))??))
    };
    insta::assert_snapshot!(build(true)?, @"
    a 1.0.0: f
    a 1.0.0 (host): b, g
    b 1.0.0 (host)
    root 1.0.0
    ");
    insta::assert_snapshot!(build(false)?, @"
    a 1.0.0: b, f, g
    b 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn proc_macros_and_their_dependencies_build_for_the_host() -> anyhow::Result<()> {
    let registry = vec![
        pkg("shared").feature("std", &[]),
        pkg("derive").deps(["shared".with(&["std"])]),
    ];
    let case = edition(Some("2021"), case(registry, ["derive", "shared"]));
    let resolved = case.features(|request| {
        request.proc_macros.insert(registry_id("derive")?);
        Ok(())
    })??;
    insta::assert_snapshot!(units(&resolved), @"
    derive 1.0.0 (host)
    root 1.0.0
    shared 1.0.0
    shared 1.0.0 (host): std
    ");
    Ok(())
}

#[test]
fn a_proc_macro_member_builds_for_both_host_and_target() -> anyhow::Result<()> {
    let registry = vec![pkg("shared").feature("std", &[])];
    let case = edition(Some("2021"), case(registry, ["shared".with(&["std"])]))
        .manifest_tail("\n[lib]\nproc-macro = true\n");
    insta::assert_snapshot!(units(&case.features(|_| Ok(()))??), @"
    root 1.0.0
    root 1.0.0 (host)
    shared 1.0.0: std
    shared 1.0.0 (host): std
    ");
    Ok(())
}

#[test]
fn dependencies_of_platforms_not_built_for_are_left_out() -> anyhow::Result<()> {
    let registry = vec![
        pkg("shared").feature("windows", &[]),
        pkg("winapi").deps(["shared".with(&["windows"])]),
    ];
    let build = |resolver: Option<&str>, for_windows: bool| -> anyhow::Result<String> {
        let deps = ["shared".to_dep(), "winapi".on("cfg(windows)")];
        let case = edition(resolver, case(registry.clone(), deps));
        let resolved = case.features(|request| {
            if for_windows {
                request.targets.push(windows()?);
            }
            Ok(())
        })??;
        Ok(units(&resolved))
    };
    insta::assert_snapshot!(build(Some("2021"), false)?, @"
    root 1.0.0
    shared 1.0.0
    ");
    insta::assert_snapshot!(build(Some("2021"), true)?, @"
    root 1.0.0
    shared 1.0.0: windows
    winapi 1.0.0
    ");
    // Resolver 1 unifies features over every platform, but compiles only
    // the dependencies of the platforms built for.
    insta::assert_snapshot!(build(None, false)?, @"
    root 1.0.0
    shared 1.0.0: windows
    ");
    Ok(())
}

#[test]
fn build_dependencies_follow_the_host_platform() -> anyhow::Result<()> {
    let registry = vec![pkg("unix-only"), pkg("windows-only")];
    let deps = [
        dep_kind("unix-only", DepKind::Build).on("cfg(unix)"),
        dep_kind("windows-only", DepKind::Build).on("cfg(windows)"),
    ];
    let case = edition(Some("2021"), case(registry, deps));
    // Cross compiling from the linux host to windows.
    let resolved = case.features(|request| {
        request.targets.push(windows()?);
        Ok(())
    })??;
    insta::assert_snapshot!(units(&resolved), @"
    root 1.0.0
    unix-only 1.0.0 (host)
    ");
    Ok(())
}

#[test]
fn dev_dependencies_only_count_for_dev_units_under_resolver_2() -> anyhow::Result<()> {
    let registry = vec![pkg("a").feature("testing", &[]), pkg("only-dev")];
    let build = |resolver: Option<&str>, dev_units: bool| -> anyhow::Result<String> {
        let deps = [
            "a".to_dep(),
            dep_kind("a", DepKind::Dev).with(&["testing"]),
            dep_kind("only-dev", DepKind::Dev),
        ];
        let case = edition(resolver, case(registry.clone(), deps));
        let resolved = case.features(|request| {
            request.dev_units = dev_units;
            Ok(())
        })??;
        Ok(units(&resolved))
    };
    insta::assert_snapshot!(build(Some("2021"), false)?, @"
    a 1.0.0
    root 1.0.0
    ");
    insta::assert_snapshot!(build(Some("2021"), true)?, @"
    a 1.0.0: testing
    only-dev 1.0.0
    root 1.0.0
    ");
    // Resolver 1 enables the features of dev-dependencies in every build.
    insta::assert_snapshot!(build(None, false)?, @"
    a 1.0.0: testing
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn a_weak_dependency_feature_waits_for_the_dependency() -> anyhow::Result<()> {
    let registry = vec![
        pkg("a")
            .deps(["b".opt()])
            .feature("std", &["b?/std"])
            .feature("with-b", &["dep:b"]),
        pkg("b").feature("std", &[]),
    ];
    let build = |features: &[&str]| -> anyhow::Result<String> {
        let case = case(registry.clone(), ["a".with(features)]);
        Ok(units(&case.features(|_| Ok(()))??))
    };
    insta::assert_snapshot!(build(&["std"])?, @"
    a 1.0.0: std
    root 1.0.0
    ");
    insta::assert_snapshot!(build(&["std", "with-b"])?, @"
    a 1.0.0: std, with-b
    b 1.0.0: std
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn requested_features_replace_the_defaults() -> anyhow::Result<()> {
    let registry = vec![
        pkg("a")
            .feature("default", &["fast"])
            .feature("fast", &[])
            .feature("small", &[]),
    ];
    let case = case(registry, ["a".opt().with_default()])
        .manifest_tail("\n[features]\ndefault = [\"a\"]\nsmall = []\n");
    let build = |requested: RequestedPackage| -> anyhow::Result<String> {
        Ok(units(&case.features(|request| {
            request.packages.push(requested);
            Ok(())
        })??))
    };
    let root = || RequestedPackage::new(PackageName::new("root"));
    insta::assert_snapshot!(build(root())?, @"
    a 1.0.0: default, fast
    root 1.0.0: a, default
    ");
    insta::assert_snapshot!(build(RequestedPackage {
        default_features: false,
        features: vec![FeatureValue::parse("small"), FeatureValue::parse("a/small")],
        ..root()
    })?, @"
    a 1.0.0: default, fast, small
    root 1.0.0: a, small
    ");
    insta::assert_snapshot!(build(RequestedPackage {
        default_features: false,
        ..root()
    })?, @"root 1.0.0");
    Ok(())
}

#[test]
fn requests_naming_what_does_not_exist_are_rejected() -> anyhow::Result<()> {
    let registry = vec![pkg("a").feature("fast", &[])];
    let case = case(registry, ["a"]);
    let request = |features: &[&str]| {
        let mut requested = RequestedPackage::new(PackageName::new("root"));
        requested.features = features
            .iter()
            .map(|feature| FeatureValue::parse(feature))
            .collect();
        requested
    };
    let resolve = |requested: RequestedPackage| {
        case.features(|request| {
            request.packages.push(requested);
            Ok(())
        })
    };

    let missing = resolve(request(&["slow"]))?;
    assert!(
        matches!(&missing, Err(FeatureError::MissingFeature { package, feature }) if package.as_str() == "root" && feature.as_str() == "slow"),
        "{missing:?}"
    );
    let missing = resolve(request(&["a/slow"]))?;
    assert!(
        matches!(&missing, Err(FeatureError::MissingFeature { package, feature }) if package.as_str() == "a" && feature.as_str() == "slow"),
        "{missing:?}"
    );
    let missing = resolve(request(&["b/fast"]))?;
    assert!(
        matches!(&missing, Err(FeatureError::MissingDependency { package, dependency }) if package.as_str() == "root" && dependency.as_str() == "b"),
        "{missing:?}"
    );
    let unknown = resolve(RequestedPackage::new(PackageName::new("a")))?;
    assert!(
        matches!(&unknown, Err(FeatureError::UnknownMember { package }) if package.as_str() == "a"),
        "{unknown:?}"
    );
    Ok(())
}

#[test]
fn a_path_dependency_without_a_version_accepts_a_pre_release() -> anyhow::Result<()> {
    let case = case(Vec::new(), Vec::<common::Dep>::new())
        .manifest_tail("\n[dependencies]\nlocal = { path = \"local\" }\n")
        .file(
            "local/Cargo.toml",
            "[package]\nname = \"local\"\nversion = \"0.1.0-dev\"\n",
        )
        .file("local/src/lib.rs", "");
    insta::assert_snapshot!(units(&case.features(|_| Ok(()))??), @"
    local 0.1.0-dev
    root 1.0.0
    ");
    Ok(())
}
