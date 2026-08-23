# Faster Shared CI Design

## Goal

Reduce normal CI runner time and duplicated compilation without weakening
platform or behavioral coverage. Release builds remain optimized and retain
their existing four-asset publication contract.

## Current Problem

The Linux workspace test archive and shared runtime jobs each:

- check out the repository;
- install the same Linux build dependencies;
- install the same Rust toolchain;
- restore closely related release-profile caches;
- compile the same workspace with release optimization.

Downstream test jobs also repeat the same artifact download, unpack, executable
permission, and runtime-layout checks.

## Design

### One Linux Test-Artifact Producer

Replace `neomacs-workspace-test-archive` and `neomacs-test-runtime` with one
`neomacs-test-artifacts` job.

The job:

1. Checks out the repository and prepares the workspace temporary directory.
2. Installs the Linux build dependencies once.
3. Installs Rust and cargo-nextest once.
4. Compiles the Linux x86_64 release runtime with the exact profile, features,
   and environment used by the release workflow.
5. Bootstraps the runtime and runs the SQLite and GC-stress checks against
   `target/release/neomacs`.
6. Packages the runtime while the bootstrapped binary still matches its pdump.
7. Creates the workspace nextest archive in the same release profile, then
   uploads both artifacts under their current names.

Keeping the artifact names stable avoids changing the external contract of
consumer jobs. Packaging must precede `cargo nextest archive`, because the
workspace build can relink `target/release/neomacs` with a different feature
set and invalidate the binary/pdump fingerprint pair.

### Shared Release-Profile Cache

The shared producer and Linux x86_64 release job use:

- the same Cargo release profile;
- the same `video,neomacs-layout-engine/freetype-bundled` feature set for the
  `neomacs` release binary;
- the same `NEOMACS_BUILD_PROFILE=release` build environment;
- the same `linux-x86_64-release` cache shared key;
- workspace-crate caching enabled.

The producer saves the cache only from `main`. A tag release restores that
main-scoped cache and does not create a duplicate Linux x86_64 tag cache.
Linux aarch64 and Windows release caches remain target-specific.

The producer must not set release-profile overrides or job-level `CARGO_*` or
`RUST*` environment variables that differ from the release workflow.
`rust-cache` hashes those values into its setup-time key, and different
compiler settings also make Cargo reject restored artifacts. The workspace
archive step may set `CARGO_BUILD_JOBS=1` after cache setup; that scheduler
bound does not change Cargo unit fingerprints and prevents hosted-runner
memory spikes.

Runtime consumers keep their existing `target/release` paths. Release
packaging and the four published assets are unchanged.

### Shared Test-Artifact Setup Action

Add a local composite action at
`.github/actions/download-test-assets/action.yml`.

Inputs select whether a consumer needs:

- the runtime artifact;
- the nextest archive;
- both.

The action owns artifact download, runtime extraction, executable permissions,
and basic runtime-layout validation. It is reused by `ci.yml` and
`nextest-shards.yml`.

### Preserved Parallelism and Coverage

The producer is shared, but all consumer suites remain separate and parallel:

- core and oracle shards;
- MELPA batch, TUI, and GUI suites;
- display stack tests;
- prefix-face parity;
- real GUI and daemon lifecycle tests;
- Doom compatibility;
- scheduled ecosystem canaries.

Windows, ARM, GStreamer, native-font boundary, daemon, and no-GStreamer checks
remain in place.

### Remove Redundant Validation Builds

The shared nextest archive fully compiles and links the Linux x86_64 workspace,
which is stronger than the existing `cargo check --workspace` matrix entry for
that target. Remove only the Linux x86_64 row from the check matrix.

Move its two unique checks into the independent no-GStreamer job:

- `cargo check -p neomacs --no-default-features`;
- the SQLite-disabled `neovm-core` capability test.

Run rustfmt on one Linux runner rather than a three-platform matrix. Rustfmt
formats cfg-gated platform modules without compiling them.

## Caching

The shared producer and Linux x86_64 release use one release-profile cache
family. Consumer jobs that only execute downloaded artifacts keep Cargo build
caching disabled.

GitHub artifacts remain the authoritative cross-job binary transport. Cargo
caches are not used as a substitute because they are mutable and may be
missing.

Do not add sccache. It does not cache final binaries or proc-macro crates, and
its per-rustc GitHub cache uploads are a poor fit for this workspace.

## Validation

The implementation must pass:

- the existing release workflow contract;
- Windows GStreamer setup contract;
- actionlint and YAML parsing;
- native-font dependency boundary checks;
- focused xtask CI/release workflow tests;
- explicit assertions that CI has exactly one Linux x86_64 release artifact
  producer;
- explicit assertions that CI and Linux x86_64 release use the same cache key,
  profile, feature set, and build environment;
- explicit assertions that the release job does not save a duplicate x86_64
  tag cache;
- dependency checks showing all consumers depend on the shared producer;
- checks that the Linux x86_64 workspace-check row is removed while its
  no-default-feature coverage remains;
- focused daemon, renderer, and native-cache tests affected by the rebase.

## Non-Goals

- Changing release build optimization or release assets.
- Combining independent test suites into one serial job.
- Sharing apt state across hosted runners.
- Using Cargo caches as the authoritative artifact transport.
- Refactoring Windows GStreamer release setup in this change.

## Operational Cache Cleanup

After the rewritten workflow has produced the shared Linux x86_64 release
cache, use `gh` directly to remove cache entries owned only by:

- the retired split runtime producer;
- the retired split workspace-archive producer;
- removed macOS release jobs;
- older duplicate Linux x86_64 release cache families.

Keep the current Windows GStreamer SDK cache, active architecture-specific
release caches, active platform-check caches, and the newest shared Linux
x86_64 release cache. Inspect cache IDs and sizes before deleting them; do not
use an unqualified delete-all operation.
