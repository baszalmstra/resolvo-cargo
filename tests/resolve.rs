//! Cases of cargo's `resolver-tests/tests/resolve.rs` under the same
//! names: which versions are selected (the newest that fits, several of a
//! crate across semver-incompatible ranges, one per `links` value),
//! backtracking out of dead ends, dev-dependencies, and the failures cargo
//! reports for cycles, missing crates and names that differ from the
//! published one only in case or in `-` versus `_`. Cases about
//! `-Z minimal-versions`, git sources and resolver performance are left
//! out. The last case is not cargo's: a dependency whose requirement spans
//! two ranges and requests a feature only the older range defines.

mod common;

use common::{ToDep, dep, dep_kind, dep_req, names, pkg, resolve, resolve_names, without};
use resolvo_cargo::{DepKind, ResolveError};

/// The `[[package]]` entry of `name` in the lock file `lockfile`.
fn lock_entry<'a>(lockfile: &'a str, name: &str) -> Option<&'a str> {
    let header = format!("[[package]]\nname = \"{name}\"\n");
    lockfile
        .split("\n\n")
        .find(|entry| entry.starts_with(&header))
}

#[test]
fn test_resolving_empty_dependency_list() -> anyhow::Result<()> {
    let resolved = resolve_names(Vec::new(), Vec::<&str>::new())?;
    insta::assert_snapshot!(resolved, @"root 1.0.0");
    Ok(())
}

#[test]
fn test_resolving_one_dep() -> anyhow::Result<()> {
    let registry = vec![pkg("foo"), pkg("bar")];
    let resolved = resolve_names(registry, [dep("foo")])?;
    insta::assert_snapshot!(resolved, @"
    foo 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn test_resolving_transitive_deps() -> anyhow::Result<()> {
    let registry = vec![pkg("foo"), pkg("bar").deps(["foo"])];
    let resolved = resolve_names(registry, [dep("bar")])?;
    insta::assert_snapshot!(resolved, @"
    bar 1.0.0
    foo 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn test_resolving_common_transitive_deps() -> anyhow::Result<()> {
    let registry = vec![pkg("foo").deps(["bar"]), pkg("bar")];
    let resolved = resolve_names(registry, [dep("foo"), dep("bar")])?;
    insta::assert_snapshot!(resolved, @"
    bar 1.0.0
    foo 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn test_resolving_with_dev_deps() -> anyhow::Result<()> {
    let registry = vec![
        pkg("foo").deps([dep("bar"), dep_kind("baz", DepKind::Dev)]),
        pkg("baz").deps([dep("bat"), dep_kind("bam", DepKind::Dev)]),
        pkg("bar"),
        pkg("bat"),
    ];
    let resolved = resolve_names(registry, [dep("foo"), dep_kind("baz", DepKind::Dev)])?;
    insta::assert_snapshot!(resolved, @"
    bar 1.0.0
    bat 1.0.0
    baz 1.0.0
    foo 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn resolving_with_many_versions() -> anyhow::Result<()> {
    let registry = vec![pkg(("foo", "1.0.1")), pkg(("foo", "1.0.2"))];
    let resolved = resolve_names(registry, [dep("foo")])?;
    insta::assert_snapshot!(resolved, @"
    foo 1.0.2
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn resolving_with_specific_version() -> anyhow::Result<()> {
    let registry = vec![pkg(("foo", "1.0.1")), pkg(("foo", "1.0.2"))];
    let resolved = resolve_names(registry, [dep_req("foo", "=1.0.1")])?;
    insta::assert_snapshot!(resolved, @"
    foo 1.0.1
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn test_resolving_maximum_version_with_transitive_deps() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("util", "1.2.2")),
        pkg(("util", "1.0.0")),
        pkg(("util", "1.1.1")),
        pkg("foo").deps([dep_req("util", "1.0.0")]),
        pkg("bar").deps([dep_req("util", ">=1.0.1")]),
    ];
    let resolved = resolve_names(registry, [dep_req("foo", "1.0.0"), dep_req("bar", "1.0.0")])?;
    insta::assert_snapshot!(resolved, @"
    bar 1.0.0
    foo 1.0.0
    root 1.0.0
    util 1.2.2
    ");
    Ok(())
}

#[test]
fn resolving_incompat_versions() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("foo", "1.0.1")),
        pkg(("foo", "1.0.2")),
        pkg("bar").deps([dep_req("foo", "=1.0.2")]),
    ];
    let result = resolve(registry, [dep_req("foo", "=1.0.1"), dep("bar")])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn resolving_wrong_case_from_registry() -> anyhow::Result<()> {
    let registry = vec![pkg(("foo", "1.0.0")), pkg("bar").deps(["Foo"])];
    let result = resolve(registry, [dep("bar")])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn resolving_mis_hyphenated_from_registry() -> anyhow::Result<()> {
    let registry = vec![pkg(("fo-o", "1.0.0")), pkg("bar").deps(["fo_o"])];
    let result = resolve(registry, [dep("bar")])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn resolving_backtrack() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("foo", "1.0.2")).deps([dep("bar")]),
        pkg(("foo", "1.0.1")).deps([dep("baz")]),
        pkg("bar").deps([dep_req("foo", "=2.0.2")]),
        pkg("baz"),
    ];
    let resolved = resolve_names(registry, [dep_req("foo", "^1")])?;
    insta::assert_snapshot!(resolved, @"
    baz 1.0.0
    foo 1.0.1
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn resolving_backtrack_features() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("foo", "1.0.2")).deps([dep("bar").features(&["bad"])]),
        pkg(("foo", "1.0.1")).deps([dep("bar")]),
        pkg("bar"),
    ];
    let resolved = resolve_names(registry, [dep_req("foo", "^1")])?;
    insta::assert_snapshot!(resolved, @"
    bar 1.0.0
    foo 1.0.1
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn resolving_allows_multiple_compatible_versions() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("foo", "1.0.0")),
        pkg(("foo", "2.0.0")),
        pkg(("foo", "0.1.0")),
        pkg(("foo", "0.2.0")),
        pkg("bar").deps(["d1", "d2", "d3", "d4"]),
        pkg("d1").deps([dep_req("foo", "1")]),
        pkg("d2").deps([dep_req("foo", "2")]),
        pkg("d3").deps([dep_req("foo", "0.1")]),
        pkg("d4").deps([dep_req("foo", "0.2")]),
    ];
    let resolved = resolve_names(registry, [dep("bar")])?;
    insta::assert_snapshot!(resolved, @"
    bar 1.0.0
    d1 1.0.0
    d2 1.0.0
    d3 1.0.0
    d4 1.0.0
    foo 0.1.0
    foo 0.2.0
    foo 1.0.0
    foo 2.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn resolving_with_deep_backtracking() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("foo", "1.0.1")).deps([dep_req("bar", "1")]),
        pkg(("foo", "1.0.0")).deps([dep_req("bar", "2")]),
        pkg(("bar", "1.0.0")).deps([dep_req("baz", "=1.0.2"), dep_req("other", "1")]),
        pkg(("bar", "2.0.0")).deps([dep_req("baz", "=1.0.1")]),
        pkg(("baz", "1.0.2")).deps([dep_req("other", "2")]),
        pkg(("baz", "1.0.1")),
        pkg(("dep_req", "1.0.0")),
        pkg(("dep_req", "2.0.0")),
    ];
    let resolved = resolve_names(registry, [dep_req("foo", "1")])?;
    insta::assert_snapshot!(resolved, @"
    bar 2.0.0
    baz 1.0.1
    foo 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn resolving_with_sys_crates() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("l-sys", "0.9.1")),
        pkg(("l-sys", "0.10.0")),
        pkg(("l", "0.9.1")),
        pkg(("l", "0.10.0")),
        pkg(("d", "1.0.0")).deps([
            dep_req("l-sys", ">=0.8.0, <=0.10.0"),
            dep_req("l", ">=0.8.0, <=0.10.0"),
        ]),
        pkg(("r", "1.0.0")).deps([dep_req("l-sys", "0.9"), dep_req("l", "0.9")]),
    ];
    let resolved = resolve_names(registry, [dep_req("d", "1"), dep_req("r", "1")])?;
    insta::assert_snapshot!(resolved, @"
    d 1.0.0
    l 0.9.1
    l 0.10.0
    l-sys 0.9.1
    r 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn resolving_with_constrained_sibling_backtrack_parent() -> anyhow::Result<()> {
    let mut registry = vec![
        pkg(("foo", "1.0.0")).deps([dep_req("bar", "1.0"), dep_req("constrained", "=1.0.0")]),
        pkg(("bar", "1.0.0")).deps([
            dep_req("backtrack_trap1", "1.0.2"),
            dep_req("backtrack_trap2", "1.0.2"),
            dep_req("constrained", "1.0.0"),
        ]),
        pkg(("constrained", "1.0.0")),
        pkg(("backtrack_trap1", "1.0.0")),
        pkg(("backtrack_trap2", "1.0.0")),
    ];
    for patch in 1..50 {
        let version = format!("1.0.{patch}");
        registry.push(pkg(("bar", &version)).deps([
            dep_req("backtrack_trap1", "1.0.2"),
            dep_req("backtrack_trap2", "1.0.2"),
            dep_req("constrained", "1.0.1"),
        ]));
        registry.push(pkg(("backtrack_trap1", &version)));
        registry.push(pkg(("backtrack_trap2", &version)));
        registry.push(pkg(("constrained", &version)));
    }
    let resolved = resolve_names(registry, [dep_req("foo", "1")])?;
    insta::assert_snapshot!(resolved, @"
    backtrack_trap1 1.0.49
    backtrack_trap2 1.0.49
    bar 1.0.0
    constrained 1.0.0
    foo 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn resolving_with_constrained_sibling_backtrack_activation() -> anyhow::Result<()> {
    let mut registry = vec![
        pkg(("foo", "1.0.0")).deps([
            dep_req("bar", "=1.0.0"),
            dep_req("backtrack_trap1", "1.0"),
            dep_req("backtrack_trap2", "1.0"),
            dep_req("constrained", "<=1.0.60"),
        ]),
        pkg(("bar", "1.0.0")).deps([dep_req("constrained", ">=1.0.60")]),
    ];
    for patch in 0..45 {
        let version = format!("1.0.{patch}");
        registry.push(pkg(("backtrack_trap1", &version)));
        registry.push(pkg(("backtrack_trap2", &version)));
    }
    for patch in 0..100 {
        registry.push(pkg(("constrained", format!("1.0.{patch}"))));
    }
    let resolved = resolve_names(registry, [dep_req("foo", "1")])?;
    insta::assert_snapshot!(resolved, @"
    backtrack_trap1 1.0.44
    backtrack_trap2 1.0.44
    bar 1.0.0
    constrained 1.0.60
    foo 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn resolving_with_constrained_sibling_transitive_dep_effects() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("A", "1.0.0")).deps([
            dep_req("B", "1.0"),
            dep_req("C", "1.0"),
            dep_req("D", "1.0.100"),
        ]),
        pkg(("B", "1.0.0")).deps([dep_req("C", ">=1.0.0")]),
        pkg(("B", "1.0.1")).deps([dep_req("C", ">=1.0.1")]),
        pkg(("C", "1.0.0")).deps([dep_req("D", "1.0.0")]),
        pkg(("C", "1.0.1")).deps([dep_req("D", ">=1.0.1,<1.0.100")]),
        pkg(("C", "1.0.2")).deps([dep_req("D", ">=1.0.2,<1.0.100")]),
        pkg(("D", "1.0.0")),
        pkg(("D", "1.0.1")),
        pkg(("D", "1.0.2")),
        pkg(("D", "1.0.100")),
        pkg(("D", "1.0.101")),
        pkg(("D", "1.0.102")),
        pkg(("D", "1.0.103")),
        pkg(("D", "1.0.104")),
        pkg(("D", "1.0.105")),
    ];
    let resolved = resolve_names(registry, [dep_req("A", "1")])?;
    insta::assert_snapshot!(resolved, @"
    A 1.0.0
    B 1.0.0
    C 1.0.0
    D 1.0.105
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn incomplete_information_skipping() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("a", "1.0.0")),
        pkg(("a", "1.1.0")),
        pkg("b").deps([dep("a")]),
        pkg(("c", "1.0.0")),
        pkg(("c", "1.1.0")),
        pkg("d").deps([dep_req("c", "=1.0")]),
        pkg(("e", "1.0.0")),
        pkg(("e", "1.1.0")).deps([dep_req("c", "1.1")]),
        pkg("to_yank"),
        pkg(("f", "1.0.0")).deps([dep("to_yank"), dep("d")]),
        pkg(("f", "1.1.0")).deps([dep("d")]),
        pkg("g").deps([dep("b"), dep("e"), dep("f")]),
    ];
    let resolved = resolve_names(registry.clone(), [dep("g")])?;
    insta::assert_snapshot!(resolved, @"
    a 1.1.0
    b 1.0.0
    c 1.0.0
    d 1.0.0
    e 1.0.0
    f 1.1.0
    g 1.0.0
    root 1.0.0
    ");
    let unused_removed = resolve_names(without(&registry, "to_yank"), [dep("g")])?;
    assert_eq!(unused_removed, resolved);
    Ok(())
}

#[test]
fn incomplete_information_skipping_2() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("b", "3.8.10")),
        pkg(("b", "8.7.4")),
        pkg(("b", "9.4.6")),
        pkg(("c", "1.8.8")),
        pkg(("c", "10.2.5")),
        pkg(("d", "4.1.2")).deps([dep_req("bad", "=6.10.9")]),
        pkg(("d", "5.5.6")),
        pkg(("d", "5.6.10")),
        pkg(("to_yank", "8.0.1")),
        pkg(("to_yank", "8.8.1")),
        pkg(("e", "4.7.8")).deps([
            dep_req("d", ">=5.5.6, <=5.6.10"),
            dep_req("to_yank", "=8.0.1"),
        ]),
        pkg(("e", "7.4.9")).deps([dep_req("bad", "=4.7.5")]),
        pkg("f").deps([dep_req("d", ">=4.1.2, <=5.5.6")]),
        pkg("g").deps([dep("bad")]),
        pkg(("h", "3.8.3")).deps([dep("g")]),
        pkg(("h", "6.8.3")).deps([dep("f")]),
        pkg(("h", "8.1.9")).deps([dep_req("to_yank", "=8.8.1")]),
        pkg("i").deps([dep("b"), dep("c"), dep("e"), dep("h")]),
    ];
    let resolved = resolve_names(registry.clone(), [dep("i")])?;
    insta::assert_snapshot!(resolved, @"
    b 9.4.6
    c 10.2.5
    d 5.5.6
    e 4.7.8
    f 1.0.0
    h 6.8.3
    i 1.0.0
    root 1.0.0
    to_yank 8.0.1
    ");
    let unused_removed = resolve_names(without(&registry, ("to_yank", "8.8.1")), [dep("i")])?;
    assert_eq!(unused_removed, resolved);
    Ok(())
}

#[test]
fn incomplete_information_skipping_3() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("to_yank", "3.0.3")),
        pkg(("to_yank", "3.3.0")),
        pkg(("to_yank", "3.3.1")),
        pkg(("a", "3.3.0")).deps([dep_req("to_yank", "=3.0.3")]),
        pkg(("a", "3.3.2")).deps([dep_req("to_yank", "<=3.3.0")]),
        pkg(("b", "0.1.3")).deps([dep_req("a", "=3.3.0")]),
        pkg(("b", "2.0.2")).deps([dep_req("to_yank", "3.3.0"), dep("a")]),
        pkg(("b", "2.3.3")).deps([dep_req("to_yank", "3.3.0"), dep_req("a", "=3.3.0")]),
    ];
    let resolved = resolve_names(registry.clone(), [dep("b")])?;
    insta::assert_snapshot!(resolved, @"
    a 3.3.2
    b 2.0.2
    root 1.0.0
    to_yank 3.3.0
    ");
    let unused_removed = resolve_names(without(&registry, ("to_yank", "3.0.3")), [dep("b")])?;
    assert_eq!(unused_removed, resolved);
    Ok(())
}

#[test]
fn resolving_but_no_exists() -> anyhow::Result<()> {
    let result = resolve(Vec::new(), [dep_req("foo", "1")])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn hard_equality() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("foo", "1.0.1")),
        pkg(("foo", "1.0.0")),
        pkg(("bar", "1.0.0")).deps([dep_req("foo", "1.0.0")]),
    ];
    let resolved = resolve_names(registry, [dep_req("bar", "1"), dep_req("foo", "=1.0.0")])?;
    insta::assert_snapshot!(resolved, @"
    bar 1.0.0
    foo 1.0.0
    root 1.0.0
    ");
    Ok(())
}

#[test]
fn cyclic_good_error_message() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("A", "0.0.0")).deps([dep("C")]),
        pkg(("B", "0.0.0")).deps([dep("C")]),
        pkg(("C", "0.0.0")).deps([dep("A")]),
    ];
    let result = resolve(registry, [dep("A"), dep("B")])?;
    let Err(error @ ResolveError::Cycle { .. }) = result else {
        anyhow::bail!("expected a cycle, got {result:?}");
    };
    insta::assert_snapshot!(error.to_string(), @"cyclic package dependency: `A v0.0.0` -> `C v0.0.0` -> `A v0.0.0`");
    Ok(())
}

#[test]
fn shortest_path_in_error_message() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("F", "0.1.2")),
        pkg(("F", "0.1.1")).deps([dep("bad")]),
        pkg(("F", "0.1.0")).deps([dep("bad")]),
        pkg("E").deps([dep_req("F", "^0.1.2")]),
        pkg("D").deps([dep_req("F", "^0.1.2")]),
        pkg("C").deps([dep("D")]),
        pkg("A").deps([dep("C"), dep("E"), dep_req("F", "<=0.1.1")]),
    ];
    let result = resolve(registry, [dep("A")])?;
    assert!(
        matches!(&result, Err(ResolveError::Conflict { .. })),
        "{result:?}"
    );
    Ok(())
}

/// A requirement spanning the ranges `0.1` and `0.2` that asks for a
/// feature only `0.1` defines, while another dependency selects `0.2`:
/// cargo rejects `x 0.2.0` for the feature and takes `x 0.1.0` for it, so
/// both are selected and `root`'s edge goes to `0.1.0`.
#[test]
fn feature_missing_in_range_selected_for_another_dependency() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("x", "0.1.0")).feature("f", &[]),
        pkg(("x", "0.2.0")),
        pkg("b").deps([dep_req("x", "0.2")]),
    ];
    let resolved = resolve(
        registry,
        [dep_req("x", ">=0.1, <0.3").with(&["f"]), dep("b")],
    )??;
    insta::assert_snapshot!(names(&resolved), @"
    b 1.0.0
    root 1.0.0
    x 0.1.0
    x 0.2.0
    ");
    let lockfile = resolved.to_string();
    let root = lock_entry(&lockfile, "root").ok_or_else(|| anyhow::anyhow!("no `root` entry"))?;
    insta::assert_snapshot!(root, @r#"
    [[package]]
    name = "root"
    version = "1.0.0"
    dependencies = [
     "b",
     "x 0.1.0",
    ]
    "#);
    Ok(())
}

/// A feature enabling `x/f` on a dependency whose requirement spans the
/// ranges `0.1` and `0.2`, where only `0.1` defines `f`: cargo rejects
/// `x 0.2.0` for lacking the feature, so the dependency resolves to
/// `x 0.1.0`.
#[test]
fn dependency_feature_on_requirement_spanning_ranges() -> anyhow::Result<()> {
    let registry = vec![
        pkg(("x", "0.1.0")).feature("f", &[]),
        pkg(("x", "0.2.0")),
        pkg("a")
            .deps([dep_req("x", ">=0.1, <0.3")])
            .feature("default", &["x/f"]),
    ];
    let resolved = resolve(registry, [dep("a")])??;
    insta::assert_snapshot!(names(&resolved), @"
    a 1.0.0
    root 1.0.0
    x 0.1.0
    ");
    Ok(())
}
