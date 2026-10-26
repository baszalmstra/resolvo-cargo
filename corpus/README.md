# corpus

Compares resolvo-cargo with cargo's resolver on published crates.io versions.

Each case resolves one crate version as the root, with all features and
dev-dependencies. Both resolvers run as libraries over the same local index
checkout; cargo's comes from the `cargo` crate, version 0.96. When the lock
files differ, cargo resolves again with only resolvo-cargo's versions on
offer. That tells us whether cargo accepts the other lock file.

## Running it

It needs a checkout of the crates.io git index, which has the same layout as
the sparse index. From this directory:

```console
$ git clone --depth 1 https://github.com/rust-lang/crates.io-index ../../crates.io-index
$ cargo run --release -- --index ../../crates.io-index --out out --sample 5000
```

`--latest` takes the newest version of each crate, `--cases <file>` reruns
`name version` lines, and `--threads` sets the parallelism. A run writes
`results.csv`, `summary.md`, and the differences per case under `diffs/`.

## Results

On 20,000 randomly sampled versions:

| Outcome | Cases |
|---|---:|
| Same lock file as cargo | 18,422 (92.1%) |
| Different lock file, which cargo accepts | 1,120 (5.6%) |
| Both fail | 458 (2.3%) |
| Cargo rejects the lock file, or only one fails | 0 |

The different lock files come from the order in which the two solvers make
decisions. When two versions exclude each other, for example
`crypto-common 0.1.7` pinning `generic-array = "=0.14.7"`, cargo and resolvo
can each keep a different one at its highest version.

Time per case, including reading and parsing the index:

| ms | cargo | resolvo-cargo |
|---|---:|---:|
| total | 4,401,471 | 2,384,284 |
| median | 79 | 59 |
| p90 | 375 | 219 |
| p99 | 686 | 923 |
| max | 263,018 | 29,320 |

The slow tail is mostly Solana 1.x crates, which are full of exact version
pins that make resolvo backtrack through many versions.
`solana-stake-o-matic 1.5.x` takes 6 to 15 s, against 0.3 to 1.6 s for
cargo. Cargo's slowest case, `solana-core 1.4.1`, takes 263 s, against 29 s
for resolvo-cargo.

## Whole workspaces

`generate_lockfile --fresh` against `cargo generate-lockfile` from cargo
1.95, mean of 10 runs with warm caches:

| Workspace | Packages | cargo | resolvo-cargo | cargo `--offline` | resolvo-cargo `--offline` |
|---|---:|---:|---:|---:|---:|
| rattler | 792 | 1.76 s | 0.94 s | 0.90 s | 0.21 s |
| cargo | 541 | 1.88 s | 0.47 s | 0.24 s | 0.16 s |

Online, most of the time goes to revalidating the index, so those numbers
move by a few hundred milliseconds between runs. Cargo's offline times also
varied; the table has the fastest stable series.
