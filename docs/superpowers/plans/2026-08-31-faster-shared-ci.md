# Faster Shared CI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build Linux x86_64 release artifacts once in normal CI, share their Cargo cache with the release workflow, and remove redundant validation builds without serializing test suites.

**Architecture:** One `neomacs-test-artifacts` producer compiles the exact Linux x86_64 release shape, bootstraps the runtime, creates both existing artifacts, and writes the main-scoped release cache. A local composite action gives all consumers one artifact download/unpack contract. The release workflow restores that same cache for Linux x86_64, while other platforms keep their existing cache families.

**Tech Stack:** GitHub Actions YAML, local composite actions, Cargo release profiles, cargo-nextest archives, Rust workflow contract tests, GitHub CLI cache API.

**Spec:** `docs/superpowers/specs/2026-08-31-faster-shared-ci-design.md`

## Global Constraints

- Preserve exactly four release assets and early release creation.
- Keep Linux ARM, Windows x86_64/ARM64, macOS check, daemon, GStreamer, no-GStreamer, and native-font coverage.
- Keep independent test suites and shards parallel.
- CI and Linux x86_64 release must use identical release profile, features, `NEOMACS_BUILD_PROFILE`, and compiler flags.
- Save shared workspace caches only from `main`; pull requests may restore but must not write default-branch caches.
- Do not add sccache or another third-party caching layer.
- Preserve original line endings in every modified file.
- Use fixup commits and autosquash so the fork remains three commits.

---

### Task 1: Preserve the Upstream Windows Font Adapter

**Files:**
- Modify: `neomacs-layout-engine/src/font_backend/windows/mod.rs:290-301`

**Interfaces:**
- Consumes: `FontVariationSet::as_slice() -> &[FontVariationCoord]`
- Produces: a Windows-compilable exact variation-coordinate comparison

- [ ] **Step 1: Reproduce the existing compiler failure**

Run:

```powershell
cargo check -p neomacs-layout-engine
```

Expected: FAIL with E0277 because `Vec<FontVariationCoord>` cannot be compared
to `FontVariationSet`.

- [ ] **Step 2: Apply the minimal comparison fix**

Use:

```rust
if variations.as_slice() != matched.identity.variation_coords.as_slice() {
    return false;
}
```

- [ ] **Step 3: Verify the Windows adapter**

Run:

```powershell
cargo check -p neomacs-layout-engine
cargo fmt --all -- --check
git diff --check
```

Expected: all commands exit zero.

### Task 2: Write the CI Contract Tests

**Files:**
- Modify: `xtask/src/main_test.rs:202-218`
- Modify: `xtask/src/main_test.rs:486-721`

**Interfaces:**
- Consumes: `github_workflow_job(workflow: &str, job_name: &str) -> &str`
- Produces: source-level contracts for the shared producer, cache alignment,
  consumer dependencies, shared action, and removed redundant jobs

- [ ] **Step 1: Replace split-producer assertions**

Add or update tests that assert:

```rust
let producer = github_workflow_job(workflow, "neomacs-test-artifacts");
assert!(!workflow.contains("\n  neomacs-workspace-test-archive:"));
assert!(!workflow.contains("\n  neomacs-test-runtime:"));
assert!(producer.contains("cache-shared-key: linux-x86_64-release"));
assert!(producer.contains("cache-workspace-crates: true"));
assert!(producer.contains("NEOMACS_BUILD_PROFILE: release"));
assert!(producer.contains(
    "--features video,neomacs-layout-engine/freetype-bundled"
));
assert!(producer.contains("cargo nextest archive"));
assert!(producer.contains("--release"));
```

- [ ] **Step 2: Assert release cache consumption**

Extend the release cache test with:

```rust
let linux = github_workflow_job(release, "build-linux");
assert!(linux.contains(
    "cache-shared-key: ${{ matrix.arch == 'x86_64' && 'linux-x86_64-release' || '' }}"
));
assert!(linux.contains(
    "cache-save-if: ${{ matrix.arch != 'x86_64' }}"
));
```

- [ ] **Step 3: Assert shared consumer setup**

Add the new action to the pinned-action test list and assert each consumer uses:

```yaml
uses: ./.github/actions/download-test-assets
with:
  runtime: "true"
  archive: "true"
```

Use the minimum required input combination for archive-only and runtime-only
jobs.

- [ ] **Step 4: Assert redundant builds are gone**

Assert:

```rust
let check = github_workflow_job(workflow, "check");
assert!(!check.contains("label: linux-x86_64"));
let no_gstreamer = github_workflow_job(workflow, "linux-no-gstreamer-startup");
assert!(no_gstreamer.contains("cargo check -p neomacs --no-default-features"));
assert!(no_gstreamer.contains("disabled_build_exposes_only_gnu_capability_probes"));
let fmt = github_workflow_job(workflow, "fmt");
assert!(fmt.contains("runs-on: ubuntu-24.04"));
assert!(!fmt.contains("matrix."));
```

- [ ] **Step 5: Run the tests and observe RED**

Run:

```powershell
cargo test -p xtask ci_
cargo test -p xtask release_workflow_caches_workspace_crates_per_flavor
```

Expected: FAIL because the workflows and action still have the old structure.

### Task 3: Implement the Shared Test-Artifact Action

**Files:**
- Create: `.github/actions/download-test-assets/action.yml`

**Interfaces:**
- Consumes:
  - `runtime`: string boolean, default `"false"`
  - `archive`: string boolean, default `"false"`
- Produces:
  - extracted `target/release/neomacs`, `neomacsclient`, and `neomacs.pdump`
    when runtime is requested
  - `neomacs-workspace-tests.tar.zst` when archive is requested

- [ ] **Step 1: Define the composite inputs**

```yaml
name: Download Neomacs test assets
description: Download and validate the shared runtime and nextest archive.

inputs:
  runtime:
    description: Download and unpack the shared runtime.
    required: false
    default: "false"
  archive:
    description: Download the workspace nextest archive.
    required: false
    default: "false"
```

- [ ] **Step 2: Add conditional runtime download and validation**

Use the already-pinned `actions/download-artifact` revision. Extract the
runtime, make both binaries executable, and require the pdump and generated
Lisp assets:

```yaml
- name: Download shared test runtime
  if: inputs.runtime == 'true'
  uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
  with:
    name: neomacs-test-runtime-linux-x86_64
    path: .

- name: Unpack shared test runtime
  if: inputs.runtime == 'true'
  shell: bash
  run: |
    tar xzf neomacs-test-runtime-linux-x86_64.tar.gz
    chmod +x target/release/neomacs target/release/neomacsclient
    test -f target/release/neomacs.pdump
    test -f lisp/international/charscript.el
    test -f lisp/international/charprop.el
```

- [ ] **Step 3: Add conditional archive download**

```yaml
- name: Download workspace test archive
  if: inputs.archive == 'true'
  uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
  with:
    name: neomacs-workspace-tests-nextest-archive-linux-x86_64
    path: .
```

### Task 4: Consolidate CI and Share the Release Cache

**Files:**
- Modify: `.github/workflows/ci.yml:57-410`
- Modify: `.github/workflows/ci.yml:410-685`
- Modify: `.github/workflows/nextest-shards.yml:70-100`
- Modify: `.github/workflows/release.yml:111-120`

**Interfaces:**
- Consumes: `.github/actions/download-test-assets/action.yml`
- Produces: the existing runtime and nextest artifact names from one producer

- [ ] **Step 1: Collapse rustfmt to one runner**

Replace the fmt matrix with:

```yaml
fmt:
  name: cargo fmt
  if: github.event_name != 'schedule'
  runs-on: ubuntu-24.04
```

Keep its cache disabled and preserve both formatting commands.

- [ ] **Step 2: Remove only Linux x86_64 from the check matrix**

Keep Linux ARM, macOS ARM, Windows x86_64, and Windows ARM64. Remove the
Linux-only minimal capability steps from this matrix.

- [ ] **Step 3: Move unique Linux capability coverage**

In `linux-no-gstreamer-startup`, install nextest and add:

```yaml
- name: Check minimal capability build
  run: cargo check -p neomacs --no-default-features

- name: Test SQLite-disabled capability surface
  run: >-
    cargo nextest run -p neovm-core --no-default-features
    -E 'test(disabled_build_exposes_only_gnu_capability_probes)'
```

- [ ] **Step 4: Create the single producer**

Replace both split jobs with `neomacs-test-artifacts`. Use one environment and
one Rust setup:

```yaml
neomacs-test-artifacts:
  name: shared test artifacts (linux x86_64)
  runs-on: ubuntu-24.04
  timeout-minutes: 180
  env:
    CARGO_TERM_COLOR: always
    RUST_BACKTRACE: 1
    TMPDIR: ${{ github.workspace }}/tmp
```

Rust setup:

```yaml
with:
  cache-key: ci-test-artifacts-linux-x86_64
  cache-shared-key: linux-x86_64-release
  cache-workspace-crates: true
  cache-save-if: ${{ github.ref == 'refs/heads/main' }}
  install-nextest: true
```

Install the same dependency profile as release:

```yaml
- name: Install Linux release dependencies
  run: scripts/ci/setup-linux.sh release
```

Include `target: x86_64-unknown-linux-gnu` in the Rust setup to match the
release job's installed host target. Compile exactly like release:

```yaml
- name: Compile release binaries
  env:
    NEOMACS_BUILD_PROFILE: release
  run: >-
    cargo build -p neomacs
    --features video,neomacs-layout-engine/freetype-bundled
    --profile release

- name: Compile optional GStreamer video backend
  env:
    NEOMACS_BUILD_PROFILE: release
  run: cargo build -p neomacs-video-gstreamer --profile release

- name: Bootstrap and dump
  run: >-
    cargo xtask fresh-build --release
    --features neomacs-layout-engine/freetype-bundled
    --skip-build
```

Then run SQLite and GC stress, package the runtime tarball, archive workspace
tests with `--release --workspace`, and upload both artifacts with retention
one day. Packaging must precede the workspace archive command so a later
test-binary relink cannot invalidate the packaged binary/pdump fingerprint
pair. Set `CARGO_BUILD_JOBS: "1"` only on the archive step, after Rust cache
setup, to bound hosted-runner memory use without changing the shared cache
key or Cargo unit fingerprints.

- [ ] **Step 5: Point every consumer at the producer**

Replace every `needs` reference to either old producer with
`neomacs-test-artifacts`. Replace YAML anchors for downloads/unpacking with the
local action and the required input combination.

- [ ] **Step 6: Update reusable shards**

Replace the three manual runtime/archive steps in `nextest-shards.yml` with:

```yaml
- name: Download shared test assets
  uses: ./.github/actions/download-test-assets
  with:
    runtime: "true"
    archive: "true"
```

- [ ] **Step 7: Share the Linux x86_64 release cache**

Add to `build-linux` Rust setup:

```yaml
cache-shared-key: ${{ matrix.arch == 'x86_64' && 'linux-x86_64-release' || '' }}
cache-save-if: ${{ matrix.arch != 'x86_64' }}
```

Keep workspace-crate caching enabled and leave all build/package/upload steps
unchanged.

- [ ] **Step 8: Run the workflow contract tests**

Run in parallel:

```powershell
cargo test -p xtask ci_
cargo test -p xtask release_workflow_caches_workspace_crates_per_flavor
python scripts/test-release-workflow.py
pwsh -NoProfile -File scripts/test-windows-gstreamer-setup.ps1
```

Expected: all pass.

### Task 5: Verify and Fold the Rebased Changes

**Files:**
- Modify through autosquash: the first fork commit only

**Interfaces:**
- Consumes: all prior task outputs
- Produces: exactly three fork commits on `upstream/main`

- [ ] **Step 1: Run independent focused checks in parallel**

Run:

```powershell
cargo test -p neomacs-display-runtime --lib render_thread
cargo test -p neomacs --test neomacs_daemon_cli
cargo test -p neomacs --test neomacsclient_cli
cargo test -p neovm-core --features jit --lib native_cache
cargo test -p neovm-core --lib bytecode_obj_is_only_named_by_its_chokepoints
cargo test -p neomacs-gui-tests --test harness_contract test_plan_can_drive_an_init_directory_startup_surface
```

- [ ] **Step 2: Run workflow and tree integrity checks**

Run:

```powershell
cargo fmt --all -- --check
cargo metadata --locked --no-deps --format-version 1
git diff upstream/main...HEAD --check
```

Run actionlint over every active `.yml` workflow and verify no conflict markers.

- [ ] **Step 3: Fixup and autosquash**

Create a fixup targeting the first fork commit, including the Windows font
adapter, CI workflow/action changes, tests, spec, and plan. Autosquash onto
`upstream/main`.

- [ ] **Step 4: Review the final range**

Verify:

```powershell
git log --reverse --oneline upstream/main..HEAD
git range-diff 72600c2fad10070a809c05b0d43107aa6a27ccee..89416baf04c3788e84ad0246bdc0fb5cc6d0157f upstream/main..HEAD
git status --short
```

Expected: exactly three accounted fork commits and a clean worktree.

### Task 6: Publish and Remove Obsolete GitHub Caches

**Files:**
- No repository files

**Interfaces:**
- Consumes: recorded pre-rebase `origin/main` SHA
  `89416baf04c3788e84ad0246bdc0fb5cc6d0157f`
- Produces: updated `origin/main` and a reduced GitHub cache inventory

- [ ] **Step 1: Force-push with the exact lease**

Fetch origin, require the recorded SHA, then run:

```powershell
git push --force-with-lease=main:89416baf04c3788e84ad0246bdc0fb5cc6d0157f origin main
```

- [ ] **Step 2: Inspect caches by ID, key, ref, size, and last access**

Use:

```powershell
gh api repos/kiennq/neomacs/actions/caches --paginate
```

- [ ] **Step 3: Delete only obsolete families**

Delete cache IDs belonging exclusively to:

- `ci-shared-runtime-linux-x86_64`;
- `ci-workspace-archive-linux-x86_64`;
- removed macOS release jobs;
- older duplicate `linux-x86_64-release` entries after retaining the newest
  main-scoped shared entry.

Use one explicit command per resolved cache ID:

```powershell
gh cache delete <cache-id> --repo kiennq/neomacs
```

Do not use `gh cache delete --all`.

- [ ] **Step 4: Verify the remote and cache inventory**

Confirm `origin/main` equals local `HEAD`, the working tree is clean, active
Windows/ARM/GStreamer/check cache families remain, and total cache usage is
lower than the pre-cleanup 41,702.6 MiB.
