//! Resolves a Cargo workspace against crates.io and prints the `Cargo.lock`
//! cargo would write: like `cargo generate-lockfile` with `--fresh`, and
//! otherwise like cargo updating an existing lock file after a manifest
//! change, keeping its versions where it can.
//!
//! ```text
//! cargo run --example generate_lockfile -- <workspace> <cache-dir> [--fresh] [--offline] [--rust-version 1.95.0]
//! ```

use std::{path::PathBuf, time::Instant};

use miette::{IntoDiagnostic, Result, miette};
use resolvo_cargo::{IndexSource, Lockfile, ResolveOptions, Workspace, resolve};

#[tokio::main]
async fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let workspace_dir = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| miette!("missing <workspace>"))?,
    );
    let cache_dir = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| miette!("missing <cache-dir>"))?,
    );
    let mut fresh = false;
    let mut offline = false;
    let mut rust_version = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--fresh" => fresh = true,
            "--offline" => offline = true,
            "--rust-version" => {
                let text = arguments.next().ok_or_else(|| miette!("missing version"))?;
                rust_version = Some(semver::Version::parse(&text).into_diagnostic()?);
            }
            other => return Err(miette!("unknown argument '{other}'")),
        }
    }

    let previous = match std::fs::read_to_string(workspace_dir.join("Cargo.lock")) {
        Ok(text) if !fresh => Some(Lockfile::parse(&text)?),
        Ok(_) | Err(_) => None,
    };

    let started = Instant::now();
    let workspace = Workspace::load(&workspace_dir)?;
    let source = IndexSource::crates_io(cache_dir, offline);
    let options = ResolveOptions {
        previous,
        rust_version,
    };
    let lockfile = resolve(&workspace, &source, &options).await?;
    eprintln!(
        "{} packages in {:?}",
        lockfile.packages().len(),
        started.elapsed()
    );
    print!("{lockfile}");
    Ok(())
}
