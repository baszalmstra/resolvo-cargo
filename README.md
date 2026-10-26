# resolvo-cargo

Generate and update `Cargo.lock` without cargo.

`resolvo-cargo` reads a Cargo workspace, fetches what it needs from the
crates.io index, and writes the `Cargo.lock` cargo would write, byte for
byte. Cargo accepts it unchanged under `--locked`. For a build of a locked
workspace, it also resolves which features each package is compiled with,
like cargo's feature resolver. Versions are selected by
[resolvo](https://github.com/prefix-dev/resolvo).

## Usage

```rust
use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use resolvo_cargo::{IndexSource, Lockfile, ResolveOptions, Workspace};

async fn update_lockfile(root: &Path, cache_dir: PathBuf) -> miette::Result<()> {
    let workspace = Workspace::load(root)?;

    // Keeps the locked versions where possible, like cargo does after a
    // manifest change. Without a previous lock file this is a fresh resolve.
    let previous = match std::fs::read_to_string(root.join("Cargo.lock")) {
        Ok(text) => Some(Lockfile::parse(&text)?),
        Err(_) => None,
    };
    let options = ResolveOptions { previous, ..ResolveOptions::default() };

    let source = IndexSource::crates_io(cache_dir, /* offline */ false);
    let lockfile = resolvo_cargo::resolve(&workspace, &source, &options).await?;
    std::fs::write(root.join("Cargo.lock"), lockfile.to_string()).into_diagnostic()?;
    Ok(())
}
```

The example binary does the same from the command line:

```console
$ cargo run --release --example generate_lockfile -- <workspace> <cache-dir> [--fresh] [--offline]
```

To get the features of a build, describe the build in a `FeatureRequest`:

```rust
use resolvo_cargo::{FeatureName, FeatureRequest, IndexSource, Lockfile, TargetPlatform, Workspace};

async fn print_features(
    workspace: &Workspace,
    source: &IndexSource,
    lockfile: &Lockfile,
    rustc_cfg: &str, // `rustc --print cfg --target x86_64-unknown-linux-gnu`
) -> miette::Result<()> {
    let host = TargetPlatform::new("x86_64-unknown-linux-gnu", rustc_cfg)?;
    let request = FeatureRequest::new(host);
    let resolved = resolvo_cargo::resolve_features(workspace, source, lockfile, &request).await?;
    for (package, features_for) in resolved.packages() {
        let features = resolved.features(package, features_for).into_iter().flatten();
        let features: Vec<&str> = features.map(FeatureName::as_str).collect();
        println!("{} {} {features_for:?}: {}", package.name, package.version, features.join(","));
    }
    Ok(())
}
```

`FeatureRequest::new` builds every member with its default features for the
host. Set `packages`, `targets` and `dev_units` for other builds. The index
doesn't record which crates are proc macros, so pass the registry ones in
`proc_macros`.

Errors are enums you can match on: `ManifestError`, `IndexError`,
`LockfileError`, `ResolveError` and `FeatureError`. When no lock file can be
resolved, `ResolveError` explains the conflict in cargo terms.

## Not supported

These fail with `ManifestError::Unsupported` instead of being ignored: git
dependencies, `[patch]`, `[replace]`, alternative registries, public and
private dependencies, artifact dependencies and path bases.

## Compared with cargo

On 20,000 randomly sampled crates.io versions, 92.1% resolve to the same lock
file as cargo's. Cargo accepts every lock file that differs; the differences
come from the order in which the two solvers make decisions. Feature
resolution matches `cargo tree --target <triple>` for every package on the
rattler and cargo workspaces. [`corpus/`](corpus/README.md) has the method,
the timings and how to run it.

## Development

The toolchain comes from [pixi](https://pixi.sh); `rust-toolchain.toml` pins
the same version for rustup:

```console
$ pixi install
$ pixi run cargo test
$ pixi run cargo clippy --all-targets
```

The tests port cargo's resolver test cases (`crates/resolver-tests`) and
check that each gives cargo's verdict.

## License

BSD-3-Clause, see [LICENSE](LICENSE).
