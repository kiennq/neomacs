# Persistent Native Elisp Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make hot, eligible Elisp bytecode persist as validated native code across Neomacs sessions by default on packaged Linux and Windows builds.

**Architecture:** Add a host-initialized `NativeCache` coordinator around the existing Cranelift AOT producer and JIT cache. Immutable generation directories contain one batched dynamic library plus a manifest; cheap prekeys mark named functions for call-one lookup, while full content/variant validation preserves ordinary JIT heat semantics on every miss. Packaged builds stage the pinned Rust 1.96.1 toolchain's lld and a verified import-free builtins object, so runtime linking never depends on `PATH`.

**Tech Stack:** Rust 2024, Cranelift 0.134.3, `object` 0.37, `serde`/`serde_json`, `sha2`, `libloading`, Windows APIs through `windows-sys`, PowerShell, Bash, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-08-28-persistent-native-cache-design.md`

## Global Constraints

- The Tier-0 bytecode interpreter remains the semantic oracle; every cache-specific failure falls through to ordinary JIT/interpreter behavior.
- A supporting host initializes caching as enabled by default. `neovm-core` alone remains disabled until initialized.
- Final interactive and daemon runs read and emit; final `--batch` runs read only; bootstrap, temacs, pdump-production, worker, mock-display, and test hosts remain disabled unless tests force them.
- `--no-native-cache` overrides everything. `--native-cache-dir PATH` overrides `NEOMACS_NATIVE_CACHE_DIR`; `NEOMACS_NATIVE_CACHE=0` disables the cache. `-Q` does not disable it.
- Active manifest/prewarm work has a 50 ms budget. Optional maintenance has a separate 50 ms budget and runs at most once per 24 hours.
- Shutdown emission is limited to 128 leaves, two seconds, 4,096 cached leaves, and 512 MiB across namespaces.
- Temporary generation directories expire after 24 hours. Inactive build namespaces expire after 30 days.
- Three consecutive policy-level load failures disable reads for the session. Two consecutive probe/link timeouts start a 24-hour write backoff, doubling to at most seven days.
- Cached libraries have no undefined host or platform symbols. Persistent runtime calls use the versioned `LeafSidecar` shim table.
- Build metadata is emitted for every target. Support is enabled only when
  `CARGO_FEATURE_JIT`, `TARGET`, `HOST`, the Cargo target family, and linker
  availability agree; Linux and Windows MSVC are the release-supported
  families. macOS, Windows GNU, non-JIT, and ordinary cross builds without a
  usable target driver degrade to `SUPPORTED=0` metadata and never fail the
  build.
- The Rust `gcc-ld/ld.lld` and `gcc-ld/lld-link` files are wrappers. Resolve
  and hash the sibling host `rust-lld[.exe]` bytes, and stage those actual
  bytes under `ld.lld`/`lld-link.exe`. Runtime invokes the staged executable
  as `<driver> -flavor gnu|link ...` through an absolute package-relative
  path; never search `PATH` or invoke through a shell.
- `NEOMACS_NATIVE_CACHE_LLD` is an optional absolute override for deliberate
  cross builds or nonstandard toolchains and is a Cargo rebuild input.
- Before the first write, run an isolated tiny linker probe using the staged
  actual rust-lld, the leading flavor argument, and a private temporary output.
  The probe is skipped for unsupported builds.
- Generation publication is one atomic, same-filesystem, no-replace directory rename.
- Cache roots and generation directories are private to the current user. Reject unsafe ownership, permissions, symlinks, and reparse points.
- Preserve dump-time preload behavior and existing internal `NEOVM_AOT*` test seams until their tests are migrated.
- Do not add background compilation or allow another thread to borrow VM/obarray/GC state.
- Preserve each touched file's existing line endings.

---

## File Structure

### New runtime files

- `neovm-core/src/private_directory.rs` — reusable secure private-directory creation/validation for daemon sockets and executable cache data.
- `neovm-core/src/private_directory_test.rs` — Unix permission and Windows ACL/reparse-point tests.
- `neovm-core/src/emacs_core/jit/native_cache.rs` — coordinator, public configuration/status API, process-global index state, startup/shutdown entry points.
- `neovm-core/src/emacs_core/jit/native_cache/format.rs` — generation IDs, prekeys, content/variant keys, bounded JSON manifest codec.
- `neovm-core/src/emacs_core/jit/native_cache/storage.rs` — namespace layout, atomic publication, journals, quarantine, trash, pruning, clear markers, backoff metadata.
- `neovm-core/src/emacs_core/jit/native_cache/platform.rs` — packaged linker resolution, target-specific link commands, dynamic-load flags, subprocess timeout/process-tree control.
- `neovm-core/src/emacs_core/jit/native_cache/emitter.rs` — candidate-to-object emission, import audit, batched generation linking.
- `neovm-core/src/emacs_core/jit/native_cache_test.rs` — coordinator/config/prewarm/status tests.
- `neovm-core/src/emacs_core/jit/native_cache/format_test.rs` — manifest/key parser tests.
- `neovm-core/src/emacs_core/jit/native_cache/storage_test.rs` — publication/maintenance/concurrency tests.
- `neovm-core/src/emacs_core/jit/native_cache/platform_test.rs` — linker command/load-policy tests.
- `neovm-core/native-cache/builtins.rs` — `no_std` local implementations of approved Cranelift memory libcalls.
- `neovm-core/build_support/native_cache.rs` — builtins compilation, deterministic build identity, lld discovery/digests, generated metadata.
- `neovm-core/tests/native_cache_build_support.rs` — direct tests for the build-support hashing/audit helper.

### New packaging/integration files

- `scripts/test-packaged-native-cache.sh` — Linux two-process archive test.
- `scripts/test-packaged-native-cache.ps1` — Windows two-process ZIP test.
- `test/native-cache/packaged-runtime-scenario.el` — shared hot-function, signal, memory, large-frame, and stale-prekey workload.
- `etc/licenses/llvm/LICENSE.TXT` and `etc/licenses/llvm/NOTICE.TXT` — notices shipped with the bundled rust-lld binary.

### Existing files modified

- `neovm-core/src/lib.rs`, `neovm-core/src/local_socket.rs` — expose/reuse private-directory security.
- `neovm-core/Cargo.toml`, `Cargo.lock`, `neovm-core/build.rs` — manifest codec/build dependencies, builtins object, build identity.
- `neovm-core/src/emacs_core/jit.rs` — native-cache module and clearable AOT-prewarm runtime state.
- `neovm-core/src/emacs_core/jit/cache.rs` — cache-first lookup without early JIT/negative caching after stale prewarm.
- `neovm-core/src/emacs_core/jit/aot.rs` — reusable descriptor/object/load seams; retain dump-time preload.
- `neovm-core/src/emacs_core/jit/compile.rs`, `neovm-core/src/emacs_core/jit/shim_names.rs` — sidecar shim table and persistent indirect calls.
- `neovm-core/src/emacs_core/jit/stats.rs` — native-cache counters.
- `neovm-core/src/emacs_core/symbol.rs` — cheap prekey notification after named function publication.
- `neovm-core/src/emacs_core/builtins/mod.rs`, `neovm-core/src/emacs_core/builtins/tests.rs` — status/clear Lisp surfaces.
- `lisp/native-cache.el` — interactive status report and deferred-clear commands.
- `neovm-core/tests/aot_pgo.rs`, `aot_spec.rs`, `aot_call_bearing.rs` — generation-based compatibility tests.
- `neomacs-bin/src/args.rs`, `args_test.rs`, `main.rs`, `main_test.rs`, `build_info.rs`, `build_info_test.rs` — public options, mode policy, lifecycle, build/status display.
- `xtask/src/main.rs`, `xtask/src/main_test.rs` — stage and verify rust-lld, builtins, and metadata.
- `scripts/package-release.sh`, `package-deb.sh`, `package-rpm.sh`, `package-appimage.sh`, `test-linux-release-artifacts.sh` — Linux payload and archive checks.
- `scripts/emacs-build.ps1`, `scripts/test-windows-gstreamer-setup.ps1` — Windows payload, size report, and contract checks.
- `.github/workflows/release.yml` — packaged Linux/Windows two-process gates.

---

### Task 1: Extract reusable private-directory security

**Files:**
- Create: `neovm-core/src/private_directory.rs`
- Create: `neovm-core/src/private_directory_test.rs`
- Modify: `neovm-core/src/lib.rs`
- Modify: `neovm-core/src/local_socket.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum PrivateDirectoryPurpose {
      LocalSocket,
      ExecutableCache,
  }

  pub fn prepare_private_directory(
      path: &Path,
      purpose: PrivateDirectoryPurpose,
  ) -> io::Result<()>;

  pub fn validate_private_directory(
      path: &Path,
      purpose: PrivateDirectoryPurpose,
  ) -> io::Result<()>;
  ```
- Consumes: existing Windows ACL/owner/reparse-point logic from `local_socket.rs`; existing Unix owner/mode checks.

- [ ] **Step 1: Write failing private-directory tests**

Create `private_directory_test.rs` with platform-gated tests:

```rust
use super::*;

#[test]
fn executable_cache_directory_is_created_private() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("native-cache");
    prepare_private_directory(&path, PrivateDirectoryPurpose::ExecutableCache).unwrap();
    validate_private_directory(&path, PrivateDirectoryPurpose::ExecutableCache).unwrap();
}

#[cfg(unix)]
#[test]
fn executable_cache_rejects_group_writable_directory() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(validate_private_directory(
        tmp.path(),
        PrivateDirectoryPurpose::ExecutableCache,
    )
    .is_err());
}
```

Move the existing Windows socket-directory ACL/reparse-point assertions into equivalent purpose-parameterized tests.

- [ ] **Step 2: Run the tests and confirm the new module is missing**

Run:

```powershell
cargo test -p neovm-core --lib private_directory
```

Expected: compilation fails because `private_directory` is not defined.

- [ ] **Step 3: Move the security implementation without changing socket behavior**

Move the generic create/open/owner/DACL/reparse-point code from `local_socket.rs` into `private_directory.rs`. Keep socket path selection in `local_socket.rs`, replacing its directory preparation body with:

```rust
crate::private_directory::prepare_private_directory(
    path,
    crate::private_directory::PrivateDirectoryPurpose::LocalSocket,
)
```

For Unix executable-cache directories, require owner UID and mode `0o700`. For Windows, require a protected DACL granting the current user and `SYSTEM` full control and reject reparse points.

- [ ] **Step 4: Run socket and private-directory tests**

Run:

```powershell
cargo test -p neovm-core --lib private_directory
cargo test -p neovm-core --lib local_socket
```

Expected: both pass; daemon socket-directory behavior is unchanged.

- [ ] **Step 5: Commit**

```powershell
git add neovm-core/src/private_directory.rs neovm-core/src/private_directory_test.rs neovm-core/src/local_socket.rs neovm-core/src/lib.rs
git commit -m "refactor(runtime): share private directory validation"
```

---

### Task 2: Generate the native-cache toolchain identity and builtins object

**Files:**
- Create: `neovm-core/native-cache/builtins.rs`
- Create: `neovm-core/build_support/native_cache.rs`
- Create: `neovm-core/tests/native_cache_build_support.rs`
- Modify: `neovm-core/build.rs`
- Modify: `neovm-core/Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces build-time environment constants:
  ```text
  NEOMACS_NATIVE_CACHE_SUPPORTED
  NEOMACS_NATIVE_CACHE_BUILD_ID
  NEOMACS_NATIVE_CACHE_LLD_VERSION
  NEOMACS_NATIVE_CACHE_LLD_SHA256
  NEOMACS_NATIVE_CACHE_BUILTINS_SHA256
  ```
- Produces an import-free `native-cache-builtins.o` or `.obj` plus
  `build-metadata.json` in `OUT_DIR/native-cache/`.
- Consumes: pinned `rust-toolchain.toml` (`1.96.1`), `CARGO_FEATURE_JIT`,
  `TARGET`, `HOST`, the Cargo target family, and its target-specific
  `gcc-ld/ld.lld` or `gcc-ld/lld-link` wrapper plus sibling host
  `rust-lld[.exe]`. `NEOMACS_NATIVE_CACHE_LLD`, when set, must be an absolute
  path to the deliberate cross-build override.

The build script must make the support decision before compiling builtins or
running any linker command. Linux targets and Windows MSVC targets can be
supported; macOS, Windows GNU, non-JIT targets, unsupported target families,
and ordinary cross builds without a usable driver emit unsupported metadata
and continue successfully. Unsupported output emits `SUPPORTED=0`, all four
identity values empty, `supported: false`, and a non-empty
`unsupported_reason`; it does not compile builtins, inspect symbols, probe a
linker, or panic. Release packaging later rejects unsupported Linux/Windows
artifacts.

The build-only metadata fields are exactly:
`format_version`, `supported`, `unsupported_reason`, `target`, `host`,
`build_id`, `linker_flavor`, `linker_source_file`, `staged_linker_file`,
`lld_version`, `lld_sha256`, `builtins_file`, and `builtins_sha256`.
Supported metadata records `linker_flavor` as `gnu` for Linux or `link` for
Windows and records absolute build-machine paths. Unsupported records use
empty identity/path fields. Identity hashing includes support state and all
relevant profile/codegen inputs; supported records include linker and builtins
bytes, while unsupported records omit those contents. Emit no identity
warnings on ordinary supported builds; warn once only when degrading support.

- [ ] **Step 1: Add failing pure support and identity tests**

In `build_support/native_cache.rs`, expose pure support-decision,
path/flavor, absolute-override, symbol-audit, and identity helpers to a test
crate. Create `tests/native_cache_build_support.rs` with tests covering
non-JIT, macOS, Windows GNU, and ordinary cross-build degradation; Linux
`gnu` and Windows `link` flavor/staged-name selection; host-based rust-lld
paths; absolute override validation; rejection of unknown undefined symbols;
and:

```rust
#[path = "../build_support/native_cache.rs"]
mod native_cache;

use native_cache::{hash_identity_records, IdentityRecord};

#[test]
fn identity_is_order_independent_after_name_sort() {
    let a = IdentityRecord { name: "a".into(), bytes: vec![1] };
    let b = IdentityRecord { name: "b".into(), bytes: vec![2] };
    assert_eq!(
        hash_identity_records(&[a.clone(), b.clone()]),
        hash_identity_records(&[b, a]),
    );
}

#[test]
fn identity_changes_when_codegen_input_changes() {
    let before = IdentityRecord { name: "jit.rs".into(), bytes: vec![1] };
    let after = IdentityRecord { name: "jit.rs".into(), bytes: vec![2] };
    assert_ne!(hash_identity_records(&[before]), hash_identity_records(&[after]));
}
```

- [ ] **Step 2: Run the build-support tests and confirm failure**

Run:

```powershell
cargo test -p neovm-core --test native_cache_build_support
```

Expected: fails because the support/path/flavor/override/audit helpers and
generated support behavior do not exist.

- [ ] **Step 3: Add the import-free builtins source**

Create `native-cache/builtins.rs` as `#![no_std]` with exactly these exports:

```rust
#![no_std]

#[unsafe(no_mangle)]
pub unsafe extern "C" fn neomacs_cache_memcpy(
    dst: *mut u8,
    src: *const u8,
    len: usize,
) -> *mut u8 {
    let mut i = 0;
    while i < len {
        unsafe { dst.add(i).write_volatile(src.add(i).read_volatile()) };
        i += 1;
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn neomacs_cache_memmove(
    dst: *mut u8,
    src: *const u8,
    len: usize,
) -> *mut u8 {
    if (dst as usize) <= (src as usize) {
        return unsafe { neomacs_cache_memcpy(dst, src, len) };
    }
    let mut i = len;
    while i != 0 {
        i -= 1;
        unsafe { dst.add(i).write_volatile(src.add(i).read_volatile()) };
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn neomacs_cache_memset(
    dst: *mut u8,
    value: i32,
    len: usize,
) -> *mut u8 {
    let mut i = 0;
    while i < len {
        unsafe { dst.add(i).write_volatile(value as u8) };
        i += 1;
    }
    dst
}
```

- [ ] **Step 4: Decide support before compiling or probing**

Read `CARGO_FEATURE_JIT`, `TARGET`, `HOST`, `CARGO_CFG_TARGET_FAMILY`, and
`NEOMACS_NATIVE_CACHE_LLD` with `cargo:rerun-if-env-changed` directives.
Reject non-JIT, macOS, Windows GNU, unknown target families, and ordinary
cross configurations without an available target wrapper/host rust-lld.
Resolve the optional absolute override for deliberate cross builds. Any
unsupported or unavailable configuration writes the complete unsupported
metadata record and empty constants, with one degradation warning at most.

- [ ] **Step 5: Compile and audit the builtins object in `build.rs`**

Add `build_support::native_cache::emit_native_cache_build_metadata()`. It must:

1. invoke `$RUSTC` with `--crate-type=lib --emit=obj -C panic=abort -C opt-level=2 -C relocation-model=pic --target $TARGET`;
2. parse the object with `object` 0.37;
3. require exactly the three exports above and zero undefined symbols;
4. locate the target wrapper beneath `rustc --print sysroot`, then resolve the
   actual sibling host executable at
   `lib/rustlib/<host>/bin/rust-lld[.exe]`; hash the actual rust-lld bytes, not
   wrapper bytes. Record linker flavor `gnu` or `link` and staged name
   `ld.lld` or `lld-link.exe`;
5. hash support state, target, host, all relevant Cargo/profile/codegen
   inputs, `neovm-core/src/**/*.rs`, `neovm-core/build_support/**/*.rs`,
   manifests, lockfile, enabled JIT features, `rustc -Vv`, Cranelift versions,
   codegen settings, builtins bytes, and actual rust-lld bytes as
   length-delimited sorted records; unsupported records omit linker/builtins
   contents; and
6. emit `SUPPORTED` plus the four identity constants and write
   `OUT_DIR/native-cache/build-metadata.json` with the exact fields above,
   absolute build-machine source paths for supported builtins and rust-lld,
   their digests/version, target, host, flavor, staged name, and build ID.
   This file is an xtask input and is never packaged.

Use:

```rust
fn mix_record(hasher: &mut sha2::Sha256, name: &str, bytes: &[u8]) {
    use sha2::Digest;
    hasher.update((name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}
```

Add `object = "0.37"` to `[build-dependencies]`.

- [ ] **Step 6: Verify direct builds regenerate identity**

Run:

```powershell
cargo clean -p neovm-core
cargo check -p neovm-core --features jit
```

Expected: supported builds contain a verified builtins object and all
constants. Unsupported builds complete with `SUPPORTED=0`, empty identity
constants, unsupported metadata, no builtins compilation, and no linker probe.

- [ ] **Step 7: Commit**

```powershell
git add neovm-core/native-cache/builtins.rs neovm-core/build_support/native_cache.rs neovm-core/tests/native_cache_build_support.rs neovm-core/build.rs neovm-core/Cargo.toml Cargo.lock
git commit -m "build(jit): define native cache toolchain identity"
```

---

### Task 3: Define cache configuration, keys, manifests, and status

**Files:**
- Create: `neovm-core/src/emacs_core/jit/native_cache.rs`
- Create: `neovm-core/src/emacs_core/jit/native_cache/format.rs`
- Create: `neovm-core/src/emacs_core/jit/native_cache_test.rs`
- Create: `neovm-core/src/emacs_core/jit/native_cache/format_test.rs`
- Modify: `neovm-core/src/emacs_core/jit.rs`
- Modify: `neovm-core/src/emacs_core/jit/stats.rs`
- Modify: `neovm-core/Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  pub enum NativeCacheAccess { Disabled, ReadOnly, ReadWrite }

  #[derive(Clone, Debug)]
  pub struct NativeCacheConfig {
      pub access: NativeCacheAccess,
      pub root: PathBuf,
      pub toolchain_dir: PathBuf,
      pub active_index_budget: Duration,
      pub maintenance_budget: Duration,
      pub emit_budget: Duration,
      pub max_emit_leaves: usize,
      pub max_cached_leaves: usize,
      pub max_cache_bytes: u64,
  }

  pub fn initialize(config: NativeCacheConfig) -> Result<NativeCacheInitReport, NativeCacheError>;
  pub fn status() -> NativeCacheStatus;
  pub fn reset_for_test();

  pub(crate) fn select_generation_candidates<'a>(
      index: &'a GenerationIndex,
      content: ContentHash,
      variant: VariantHash,
  ) -> impl Iterator<Item = &'a IndexedLeaf>;
  ```
- Produces `GenerationManifest`, `ManifestLeaf`, `FunctionPrekey`, `ContentHash`, `VariantHash`, and `GenerationId`.
- `select_generation_candidates` returns exact content/variant matches,
  newest-first, capped at four.
- Consumes: Task 2 build constants.

- [ ] **Step 1: Write failing defaults and manifest tests**

Add tests that assert:

```rust
#[test]
fn production_defaults_match_the_spec() {
    let cfg = NativeCacheConfig::for_paths(
        PathBuf::from("cache"),
        PathBuf::from("runtime/share/neomacs/native-cache"),
        NativeCacheAccess::ReadWrite,
    );
    assert_eq!(cfg.active_index_budget, Duration::from_millis(50));
    assert_eq!(cfg.maintenance_budget, Duration::from_millis(50));
    assert_eq!(cfg.emit_budget, Duration::from_secs(2));
    assert_eq!(cfg.max_emit_leaves, 128);
    assert_eq!(cfg.max_cached_leaves, 4_096);
    assert_eq!(cfg.max_cache_bytes, 512 * 1024 * 1024);
}

#[test]
fn manifest_rejects_excessive_leaf_count() {
    let bytes = br#"{"format_version":1,"leaves":[{}]}"#;
    assert!(parse_generation_manifest(bytes, ManifestLimits { max_leaves: 0 }).is_err());
}

#[test]
fn exact_variant_lookup_is_newest_first_and_capped_at_four() {
    let index = index_with_duplicate_key_generations(6);
    let ids: Vec<_> = select_generation_candidates(&index, CONTENT, VARIANT)
        .map(|leaf| leaf.generation_id)
        .collect();
    assert_eq!(ids, vec![GEN_6, GEN_5, GEN_4, GEN_3]);
}
```

- [ ] **Step 2: Run tests and confirm failure**

Run:

```powershell
cargo test -p neovm-core --lib native_cache
```

Expected: fails because the module/types are missing.

- [ ] **Step 3: Implement the bounded manifest model**

Use `serde` with `deny_unknown_fields`. Manifest fields are:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationManifest {
    pub format_version: u32,
    pub generation_id: GenerationId,
    pub build_id: String,
    pub abi_tag: u32,
    pub target: String,
    pub library_file: String,
    pub library_sha256: String,
    pub created_unix_secs: u64,
    pub leaves: Vec<ManifestLeaf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestLeaf {
    pub prekey: FunctionPrekey,
    pub content_hash: ContentHash,
    pub variant_hash: VariantHash,
    pub arity: usize,
    pub entry_symbol: String,
    pub descriptor_symbol: String,
    pub descriptor_bytes: u32,
    pub reloc_recipe_bytes: u32,
    pub spec_site_count: u32,
}
```

Reject manifests over 1 MiB, over 128 leaves, duplicate `(content_hash, variant_hash)` pairs, non-basename library names, wrong build/target/ABI, and invalid 32/64-hex fields.
Before loading a library, also reject any leaf whose `descriptor_bytes` exceeds
`MAX_DESCRIPTOR_BYTES`, `reloc_recipe_bytes` exceeds
`MAX_RELOC_RECIPE_BYTES`, or `spec_site_count` exceeds
`MAX_SPEC_SITES`. Define and reuse:

```rust
pub(crate) const MAX_DESCRIPTOR_BYTES: u32 = 4 * 1024 * 1024;
pub(crate) const MAX_RELOC_RECIPE_BYTES: u32 = 1024 * 1024;
pub(crate) const MAX_SPEC_SITES: u32 = 64 * 1024;
```

Replace the private duplicate limits in `aot.rs` with these shared constants.

- [ ] **Step 4: Implement process-global configuration and counters**

Use a `LazyLock<RwLock<NativeCacheState>>` containing only `Send + Sync` paths, manifests, prekey maps, counters, and error strings. Dynamic library handles remain thread-local in the loader.

Expose `NativeCacheStatus` with access, root, namespace, indexed leaves/generations, loaded leaves/generations, hits, misses, validation failures, emitted/skipped leaves, bytes, budget flags, and last error.

- [ ] **Step 5: Run focused tests**

Run:

```powershell
cargo test -p neovm-core --lib native_cache
cargo test -p neovm-core --lib jit_stats
```

Expected: pass.

- [ ] **Step 6: Commit**

```powershell
git add neovm-core/src/emacs_core/jit.rs neovm-core/src/emacs_core/jit/native_cache.rs neovm-core/src/emacs_core/jit/native_cache neovm-core/src/emacs_core/jit/native_cache_test.rs neovm-core/src/emacs_core/jit/stats.rs neovm-core/Cargo.toml Cargo.lock
git commit -m "feat(jit): define persistent native cache state"
```

---

### Task 4: Implement generation storage, startup indexing, and maintenance

**Files:**
- Create: `neovm-core/src/emacs_core/jit/native_cache/storage.rs`
- Create: `neovm-core/src/emacs_core/jit/native_cache/storage_test.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache/format.rs`

**Interfaces:**
- Produces:
  ```rust
  pub(crate) fn open_namespace(config: &NativeCacheConfig)
      -> Result<GenerationIndex, NativeCacheError>;
  pub(crate) fn publish_generation(
      config: &NativeCacheConfig,
      staged_dir: &Path,
      id: GenerationId,
  ) -> Result<PublishOutcome, NativeCacheError>;
  pub(crate) fn maintain(
      config: &NativeCacheConfig,
      deadline: Instant,
  ) -> Result<MaintenanceReport, NativeCacheError>;
  pub(crate) fn request_clear(config: &NativeCacheConfig) -> io::Result<()>;
  ```
- Consumes: Task 1 private-directory API; Task 3 manifest codec.

- [ ] **Step 1: Write failing atomic-publication and recovery tests**

Cover:

```rust
#[test]
fn final_generation_is_visible_only_after_directory_rename();

#[test]
fn concurrent_destination_exists_validates_winner_and_succeeds();

#[test]
fn startup_ignores_temp_and_manifestless_generation_directories();

#[test]
fn clear_marker_is_consumed_only_for_its_cache_root();
```

Use two threads/processes only for filesystem publication; no VM state crosses threads.

- [ ] **Step 2: Run storage tests and confirm failure**

Run:

```powershell
cargo test -p neovm-core --lib native_cache::storage
```

Expected: fails because storage functions are missing.

- [ ] **Step 3: Implement the namespace layout**

Use:

```text
<root>/<target>/<build-id>/
  generations/<generation-id>/
    manifest.json
    native-cache.so|dll
  journals/
  quarantine.json
  recency.json
  backoff.json
  trash/
  .maintenance-stamp
<root>/.clear-on-start
```

Validate every path component before opening. Parse only exact generation directory names and exact `manifest.json`/library basenames.

- [ ] **Step 4: Implement atomic no-replace publication**

Create the complete generation in `<namespace>/.tmp-<pid>-<nonce>`, fsync files/directories where supported, then:

- Linux: `renameat2(..., RENAME_NOREPLACE)`; return `WritesUnsupported` if unavailable.
- Windows: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`.

On destination-exists, validate the winner's manifest and library digest before returning `PublishOutcome::AlreadyPublished`.

- [ ] **Step 5: Implement bounded maintenance**

Within the supplied deadline:

- consume `.clear-on-start`;
- merge complete per-process usage journals into `recency.json`;
- remove temp directories older than 24 hours;
- quarantine exact failed leaf keys without suppressing re-emission;
- rename prune candidates to unique trash names before deletion;
- skip sharing violations;
- count trash in the 512 MiB budget;
- remove namespaces unused for 30 days; and
- prune oldest generations until both 4,096 leaves and 512 MiB are satisfied.

- [ ] **Step 6: Run storage tests**

Run:

```powershell
cargo test -p neovm-core --lib native_cache::storage
```

Expected: pass on Windows and Linux; platform-specific tests are gated.

- [ ] **Step 7: Commit**

```powershell
git add neovm-core/src/emacs_core/jit/native_cache.rs neovm-core/src/emacs_core/jit/native_cache/format.rs neovm-core/src/emacs_core/jit/native_cache/storage.rs neovm-core/src/emacs_core/jit/native_cache/storage_test.rs
git commit -m "feat(jit): add native cache generation storage"
```

---

### Task 5: Add call-one prewarming without changing JIT heat semantics

**Files:**
- Modify: `neovm-core/src/emacs_core/jit.rs`
- Modify: `neovm-core/src/emacs_core/jit/cache.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache.rs`
- Modify: `neovm-core/src/emacs_core/symbol.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache_test.rs`
- Modify: `neovm-core/src/emacs_core/eval_test.rs`

**Interfaces:**
- Produces:
  ```rust
  impl RuntimeState {
      pub(crate) fn clear_aot_prewarmed(&self);
      pub(crate) fn is_aot_prewarmed(&self) -> bool;
  }

  pub(crate) fn on_function_published(
      obarray: &Obarray,
      sym: SymId,
      function: Value,
  );
  pub fn prewarm_after_pdump(ctx: &Context) -> PrewarmReport;
  pub(crate) fn try_load_prewarmed(
      func: &ByteCodeFunction,
      obarray: &Obarray,
  ) -> NativeCacheLookup;

  #[cfg(test)]
  pub(crate) fn install_lookup_for_test(
      lookup: impl Fn(&ByteCodeFunction, &Obarray) -> NativeCacheLookup + 'static,
  );
  ```
- Consumes: Task 3 prekey index; existing `RuntimeState::mark_aot_prewarmed`.

- [ ] **Step 1: Write failing stale-prewarm tests**

Add tests that:

1. set heat to `hot_threshold() - 10`;
2. mark the function prewarmed;
3. force a manifest/content mismatch;
4. invoke once; and
5. assert the compiled cache slot remains absent, the marker is clear, and nine further calls are interpreted before ordinary JIT eligibility.

Also assert a valid indexed function returns `Plan::Compiled` and records an AOT hit on call one.

- [ ] **Step 2: Run focused tests and verify current early-JIT failure**

Run:

```powershell
cargo test -p neovm-core --lib failed_aot_prewarm
cargo test -p neovm-core --lib successful_aot_prewarm
```

Expected: stale prewarm currently falls through `get_or_insert_with` into `compile_cache_entry`, so the new test fails.

- [ ] **Step 3: Separate AOT lookup from JIT insertion**

Refactor `try_run_compiled` to:

```rust
if func.runtime.is_aot_prewarmed() {
    match native_cache::try_load_prewarmed(func, obarray) {
        NativeCacheLookup::Hit(leaf) => insert_and_run(leaf),
        NativeCacheLookup::Miss => {
            func.runtime.clear_aot_prewarmed();
            return Ok(None);
        }
    }
}
```

Only the ordinary heat-driven path may call `compile_cache_entry`. Do not insert `CacheEntry::NotCompilable` for a cache miss.

- [ ] **Step 4: Wire cheap publication and pdump scans**

After `Obarray::set_symbol_function` and `set_symbol_function_id` publish a
bytecode function, call `on_function_published(self, symbol, value)` after the
mutable cell update is complete. It may compare only `(name, required arity,
ops_len)`. Use `install_lookup_for_test` until Task 7 supplies the real loader.

Implement `prewarm_after_pdump` as a deadline-checked walk of `interned_function_cells_with_names`; skip the walk entirely when the active prekey map is empty.

- [ ] **Step 5: Run JIT/cache/AOT tests**

Run:

```powershell
cargo test -p neovm-core --lib failed_aot_prewarm
cargo test -p neovm-core --lib successful_aot_prewarm
cargo test -p neovm-core --lib emacs_core::jit::cache
cargo test -p neovm-core --features jit --test aot_pgo
```

Expected: all pass; ordinary threshold behavior remains unchanged.

- [ ] **Step 6: Commit**

```powershell
git add neovm-core/src/emacs_core/jit.rs neovm-core/src/emacs_core/jit/cache.rs neovm-core/src/emacs_core/jit/native_cache.rs neovm-core/src/emacs_core/jit/native_cache_test.rs neovm-core/src/emacs_core/symbol.rs neovm-core/src/emacs_core/eval_test.rs
git commit -m "feat(jit): prewarm native cache entries from call one"
```

---

### Task 6: Make persistent AOT objects import-free

**Files:**
- Modify: `neovm-core/src/emacs_core/jit/compile.rs`
- Modify: `neovm-core/src/emacs_core/jit/shim_names.rs`
- Modify: `neovm-core/src/emacs_core/jit/aot.rs`
- Create: `neovm-core/src/emacs_core/jit/native_cache/emitter.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache_test.rs`

**Interfaces:**
- Produces:
  ```rust
  pub(crate) const SHIM_TABLE_ABI_VERSION: u32 = 1;

  #[repr(C)]
  pub(crate) struct NativeShimTable {
      pub abi_version: u32,
      pub entry_count: u32,
      pub entries: [usize; SHIM_COUNT],
  }

  pub(crate) fn native_shim_table() -> &'static NativeShimTable;

  pub(crate) struct LeafArtifact {
      pub key: CacheKey,
      pub object_bytes: Vec<u8>,
      pub entry_symbol: String,
      pub descriptor_symbol: String,
  }
  ```
- Consumes: existing `LeafSidecar`, `NEOVM_JIT_SHIM_NAMES`, and shared AOT lowering.

- [ ] **Step 1: Write failing ABI and import-audit tests**

Tests must assert:

```rust
#[test]
fn shim_table_order_matches_single_source_name_list();

#[test]
fn persistent_leaf_object_imports_only_approved_builtins();

#[test]
fn unknown_libcall_rejects_only_that_leaf();

#[test]
fn persistent_large_frame_uses_inline_probestack();
```

The import test must parse emitted bytes with `object::File` rather than
trusting linker success. Before final linking, the only permitted undefined
symbols are `neomacs_cache_memcpy`, `neomacs_cache_memmove`, and
`neomacs_cache_memset`; Task 7 requires the linked library to have none.

- [ ] **Step 2: Run tests and confirm current host imports**

Run:

```powershell
cargo test -p neovm-core --lib persistent_leaf_object_imports_only_approved_builtins
```

Expected: fails because current AOT objects import `neovm_jit_*`.

- [ ] **Step 3: Extend `LeafSidecar` with the shim table**

Add `shim_table: *const NativeShimTable`, update constructors, offset assertions, `ABI_TAG_VERSION`, and the ABI hash. `CompiledLeaf::from_aot` installs `native_shim_table()`; JIT leaves may install the same table while retaining direct calls.

- [ ] **Step 4: Lower persistent runtime calls indirectly**

At the shared runtime-call seam, make the backend choose:

```rust
enum RuntimeCallBinding {
    DirectHostAddress,
    SidecarTable { slot: u32 },
}
```

The JIT backend uses `DirectHostAddress`; persistent AOT loads the function pointer from `LeafSidecar::shim_table.entries[slot]` and emits an indirect call with the exact shim signature.

- [ ] **Step 5: Pin persistent Cranelift settings and libcalls**

Build the persistent ISA with explicit PIC, calling convention, optimization level, ISA flags, and inline probestack settings supported by Cranelift 0.134.3. Supply libcall names:

```rust
match libcall {
    LibCall::Memcpy => "neomacs_cache_memcpy".into(),
    LibCall::Memmove => "neomacs_cache_memmove".into(),
    LibCall::Memset => "neomacs_cache_memset".into(),
    other => format!("neomacs_cache_unsupported_{other:?}"),
}
```

Hash these settings into the backend/variant identity. Reject a leaf whose
object references any `neomacs_cache_unsupported_*` or any undefined symbol
outside the three approved builtins.

- [ ] **Step 6: Emit and inspect each leaf independently**

Move the persistent per-leaf object wrapper into `native_cache/emitter.rs`. Reuse `build_mir_leaf_fn` and descriptor encoding from `aot.rs`; do not duplicate opcode lowering.

- [ ] **Step 7: Run AOT and deoptimization tests**

Run:

```powershell
cargo test -p neovm-core --lib persistent_
cargo test -p neovm-core --features jit --test aot_spec
cargo test -p neovm-core --features jit --test aot_call_bearing
```

Expected: objects are import-free; dump-time preload tests still pass with their existing direct-import backend.

- [ ] **Step 8: Commit**

```powershell
git add neovm-core/src/emacs_core/jit/compile.rs neovm-core/src/emacs_core/jit/shim_names.rs neovm-core/src/emacs_core/jit/aot.rs neovm-core/src/emacs_core/jit/native_cache.rs neovm-core/src/emacs_core/jit/native_cache/emitter.rs neovm-core/src/emacs_core/jit/native_cache_test.rs
git commit -m "feat(jit): emit import-free persistent AOT leaves"
```

---

### Task 7: Link, load, and publish batched generations

**Files:**
- Create: `neovm-core/src/emacs_core/jit/native_cache/platform.rs`
- Create: `neovm-core/src/emacs_core/jit/native_cache/platform_test.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache/emitter.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache/storage.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache.rs`
- Modify: `neovm-core/src/emacs_core/jit/aot.rs`
- Modify: `neovm-core/tests/aot_pgo.rs`

**Interfaces:**
- Produces:
  ```rust
  pub(crate) fn link_generation(
      config: &NativeCacheConfig,
      objects: &[LeafArtifact],
      output: &Path,
      deadline: Instant,
  ) -> Result<LinkReport, NativeCacheError>;

  pub(crate) fn load_generation(
      record: &GenerationRecord,
  ) -> Result<Arc<LoadedGeneration>, NativeCacheError>;

  pub(crate) fn load_leaf(
      generation: &Arc<LoadedGeneration>,
      leaf: &ManifestLeaf,
      func: &ByteCodeFunction,
      obarray: &Obarray,
  ) -> Result<CompiledLeaf, NativeCacheError>;
  ```
- Consumes: Task 2 builtins/lld metadata; Task 4 publication; Task 6 leaf objects.

- [ ] **Step 1: Write failing link-command and batch tests**

Assert exact commands:

```text
ld.lld -flavor gnu -shared --no-undefined -o <out> <builtins.o> <leaf1.o> ...
lld-link.exe -flavor link /DLL /NOENTRY /NODEFAULTLIB /NOIMPLIB /MACHINE:X64 /OUT:<out> /EXPORT:<entry1> /EXPORT:<descriptor1> ... <builtins.obj> <leaf1.obj> ...
```

Add one `/EXPORT:` argument for every accepted leaf's entry and descriptor
symbol. Also test one rejected leaf does not remove accepted siblings and 128
accepted leaves produce one library invocation. Add a loader spy test that
corrupts one library byte and asserts the SHA-256 mismatch is returned before
the dynamic-loader function is called.

- [ ] **Step 2: Run platform tests and confirm failure**

Run:

```powershell
cargo test -p neovm-core --lib native_cache::platform
```

Expected: fails because platform linking/loading is missing.

- [ ] **Step 3: Implement packaged linker resolution and timeout control**

Resolve the staged actual rust-lld:

```text
<runtime-root>/share/neomacs/native-cache/ld.lld
<runtime-root>\share\neomacs\native-cache\lld-link.exe
```

Verify the staged file's SHA-256 against embedded metadata before first use.
Launch it directly with `Command` and leading `-flavor gnu` or `-flavor link`,
assign a Unix process group or Windows job object, wait only until `deadline`,
and terminate the whole group/job on timeout. Before the first write, run an
isolated tiny probe using that same absolute executable, flavor, private cache
staging directory, and no `PATH` lookup; unsupported configurations skip the
probe.

- [ ] **Step 4: Link one import-free generation**

Write accepted object bytes plus the Task 2 builtins object into the private staging directory, invoke lld once, parse the output, and require zero undefined symbols/imports before writing `manifest.json`.

- [ ] **Step 5: Load with safe platform flags**

Before any dynamic-loader call, stream the library through SHA-256 and compare
it with `GenerationManifest::library_sha256`; a mismatch quarantines the
candidate and never reaches `dlopen`/`LoadLibraryExW`.

Linux then uses immediate/local symbol binding. Windows uses
`libloading::os::windows::Library::load_with_flags` with
`LOAD_LIBRARY_SEARCH_SYSTEM32`; never use altered search path or permit
dependency lookup in the cache directory. Keep `Arc<LoadedUnit>` in a
thread-local generation map and clone it into every `CompiledLeaf`.

- [ ] **Step 6: Publish and exercise a two-session test harness**

Adapt `aot_pgo.rs` to create generation 1, reset the test cache/index, initialize a fresh session, prewarm the function, and assert its first call records `aot_loads=1` and `total_compiles=0`.

- [ ] **Step 7: Run generation tests**

Run:

```powershell
cargo test -p neovm-core --lib native_cache::platform
cargo test -p neovm-core --features jit --test aot_pgo
```

Expected: pass on supported targets.

- [ ] **Step 8: Commit**

```powershell
git add neovm-core/src/emacs_core/jit/native_cache neovm-core/src/emacs_core/jit/native_cache.rs neovm-core/src/emacs_core/jit/aot.rs neovm-core/tests/aot_pgo.rs
git commit -m "feat(jit): link and load native cache generations"
```

---

### Task 8: Add bounded shutdown emission, journals, quarantine, and backoff

**Files:**
- Modify: `neovm-core/src/emacs_core/jit/native_cache.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache/storage.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache/emitter.rs`
- Modify: `neovm-core/src/emacs_core/jit/stats.rs`
- Modify: `neovm-core/src/emacs_core/jit/aot.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache_test.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache/storage_test.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn shutdown(ctx: &Context) -> NativeCacheShutdownReport;
  pub fn request_clear() -> Result<(), NativeCacheError>;
  pub fn record_generation_used(id: GenerationId);
  pub fn quarantine_leaf(key: CacheKey, reason: QuarantineReason);
  ```
- Consumes: existing `jit_compiled_ids()` and hotness data; Tasks 4 and 7 storage/linking.

- [ ] **Step 1: Write failing candidate/budget/backoff tests**

Cover hottest-first order, existing compatible-key exclusion, quarantined-key re-emission, read-only no-op, 128-leaf cap, two-second injected-clock deadline, space exhaustion, three-load-failure read disable, and exponential timeout backoff.

- [ ] **Step 2: Run tests and confirm failure**

Run:

```powershell
cargo test -p neovm-core --lib native_cache_shutdown
```

Expected: fails because shutdown orchestration is missing.

- [ ] **Step 3: Refactor the existing PGO candidate walk**

Reuse the required-only interned-bytecode candidate rules from `drain_aot_pgo_to_dir`. Return owned `CacheCandidate` descriptors sorted by heat; do not retain borrowed VM data across linking.

- [ ] **Step 4: Implement bounded generation emission**

For both `ReadOnly` and `ReadWrite`, publish a unique usage journal when the
session loaded at least one generation. Then, for `ReadWrite` only:

1. stop immediately during persisted backoff;
2. use `emitter::emit_probe_object()` to link/load one import-free
   constant-return probe if write capability is unknown;
3. collect at most 128 eligible leaves;
4. emit/inspect leaf objects until the deadline;
5. link/publish one non-empty generation;
6. update counters and backoff metadata.

Shutdown never prunes.

- [ ] **Step 5: Implement failure classification**

Descriptor/content/variant mismatches are leaf misses. Policy-level dynamic-load failures count toward the three-strike session disable. Probe/link timeouts count toward persistent backoff. Candidate unsupported errors increment skipped counts without backoff.

- [ ] **Step 6: Run shutdown/storage/AOT tests**

Run:

```powershell
cargo test -p neovm-core --lib native_cache_shutdown
cargo test -p neovm-core --lib native_cache::storage
cargo test -p neovm-core --features jit --test aot_pgo
```

Expected: pass.

- [ ] **Step 7: Commit**

```powershell
git add neovm-core/src/emacs_core/jit/native_cache.rs neovm-core/src/emacs_core/jit/native_cache neovm-core/src/emacs_core/jit/stats.rs neovm-core/src/emacs_core/jit/aot.rs
git commit -m "feat(jit): persist hot leaves within shutdown budgets"
```

---

### Task 9: Expose Lisp status and deferred clear commands

**Files:**
- Modify: `neovm-core/src/emacs_core/builtins/mod.rs`
- Modify: `neovm-core/src/emacs_core/builtins/tests.rs`
- Modify: `neovm-core/src/emacs_core/jit/native_cache.rs`
- Create: `lisp/native-cache.el`

**Interfaces:**
- Produces internal Lisp builtins:
  ```lisp
  (native-cache--status)
  (native-cache--request-clear)
  ```
- Produces interactive `native-cache-status` and `native-cache-clear` commands.
- `native-cache--status` returns an alist;
  `native-cache--request-clear` returns `t` after atomically writing the active
  root's `.clear-on-start`.

- [ ] **Step 1: Write failing builtin tests**

Assert a stable alist contains:

```lisp
((access . read-write)
 (root . "...")
 (namespace . "...")
 (indexed-generations . 0)
 (indexed-leaves . 0)
 (hits . 0)
 (misses . 0)
 (validation-failures . 0)
 (emitted-leaves . 0)
 (skipped-leaves . 0)
 (cache-bytes . 0)
 (budget-exhausted . nil)
 (last-error . nil))
```

Also assert `native-cache--request-clear` creates the marker but does not
delete a loaded generation.

- [ ] **Step 2: Run builtin tests and confirm failure**

Run:

```powershell
cargo test -p neovm-core --lib native_cache_status
cargo test -p neovm-core --lib native_cache_clear
```

Expected: builtins are undefined.

- [ ] **Step 3: Implement safe status conversion**

Copy strings/counters only. Never expose pointers, machine code, raw descriptor bytes, or printed Lisp constants. Return `access` as `disabled`, `read-only`, or `read-write`.

- [ ] **Step 4: Add interactive Lisp wrappers**

Create `lisp/native-cache.el`:

```lisp
;;; native-cache.el --- Persistent native cache controls -*- lexical-binding: t; -*-

;;;###autoload
(defun native-cache-status ()
  "Display persistent native cache status."
  (interactive)
  (with-help-window "*Native Cache Status*"
    (princ "Neomacs persistent native cache\n\n")
    (dolist (entry (native-cache--status))
      (princ (format "%-24s %S\n" (car entry) (cdr entry))))))

;;;###autoload
(defun native-cache-clear ()
  "Schedule persistent native cache deletion for next startup."
  (interactive)
  (native-cache--request-clear)
  (message "Native cache will be cleared at the next startup"))

(provide 'native-cache)
```

- [ ] **Step 5: Run builtin and Lisp-load tests**

Run:

```powershell
cargo test -p neovm-core --lib native_cache_status
cargo test -p neovm-core --lib native_cache_clear
cargo run -p neomacs --bin neomacs -- -Q --batch -L lisp -l native-cache --eval "(progn (unless (fboundp 'native-cache-status) (kill-emacs 1)) (kill-emacs 0))"
```

Expected: pass.

- [ ] **Step 6: Commit**

```powershell
git add neovm-core/src/emacs_core/builtins/mod.rs neovm-core/src/emacs_core/builtins/tests.rs neovm-core/src/emacs_core/jit/native_cache.rs lisp/native-cache.el
git commit -m "feat(lisp): expose native cache status and clear"
```

---

### Task 10: Integrate host options and runtime-mode lifecycle

**Files:**
- Modify: `neomacs-bin/src/args.rs`
- Modify: `neomacs-bin/src/args_test.rs`
- Modify: `neomacs-bin/src/main.rs`
- Modify: `neomacs-bin/src/main_test.rs`
- Modify: `neomacs-bin/src/build_info.rs`
- Modify: `neomacs-bin/src/build_info_test.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Debug, Default, Eq, PartialEq)]
  struct NativeCacheCli {
      disabled: bool,
      root: Option<PathBuf>,
  }

  fn resolve_native_cache_config(
      cli: &NativeCacheCli,
      env_disabled: bool,
      env_root: Option<PathBuf>,
      runtime_root: &Path,
      mode: RuntimeMode,
      batch_requested: bool,
      daemon: bool,
  ) -> NativeCacheConfig;
  ```
- Consumes: Tasks 3, 5, and 8 core lifecycle API.

- [ ] **Step 1: Write failing option-order and precedence tests**

Test:

- `--no-native-cache`;
- `--native-cache-dir X` and `--native-cache-dir=X`;
- missing directory value;
- CLI directory over environment directory;
- CLI disable over all directories;
- `NEOMACS_NATIVE_CACHE=0`;
- options before `--` removed from forwarded Lisp args;
- options after `--` left untouched; and
- `-Q` leaves access unchanged.

- [ ] **Step 2: Write failing mode-policy tests**

Assert:

```rust
assert_eq!(policy(FinalRun, false, false), ReadWrite);
assert_eq!(policy(FinalRun, false, true), ReadWrite);
assert_eq!(policy(FinalRun, true, false), ReadOnly);
assert_eq!(policy(Raw, false, false), Disabled);
assert_eq!(policy(BootstrapUse, false, false), Disabled);
```

Worker/mock-display/test construction remains disabled by never calling
`initialize`. Track `batch_requested` separately from GNU's broader
`noninteractive` state: only an explicit `--batch` selects `ReadOnly`; `--script`
does not silently inherit that policy. The resolver checks `daemon` before
`batch_requested`, so daemon mode remains `ReadWrite`.

- [ ] **Step 3: Run host tests and confirm failure**

Run:

```powershell
cargo test -p neomacs --lib native_cache
cargo test -p neomacs --lib runtime_mode
```

Expected: options and policy are missing.

- [ ] **Step 4: Register and consume host arguments**

Add the two rows to `STANDARD_ARGS` with `nargs` 0 and 1. Parse them before `forwarded_args` is finalized. Preserve GNU sorting and the literal post-`--` tail.

- [ ] **Step 5: Initialize after pdump load and before Lisp startup**

In both GUI evaluator-worker and TTY paths:

1. create/load the final evaluator;
2. resolve and initialize native cache after the pdump bytes/heap are installed;
3. call `prewarm_after_pdump`;
4. run `maybe_run_after_pdump_load_hook` (function publications inside the hook
   use the Task 5 publication seam);
5. enter `recursive_edit`.

Do not initialize from shared bootstrap/raw constructors.

- [ ] **Step 6: Implement platform-default cache roots**

Add a pure injectable resolver plus the production wrapper:

```rust
fn default_native_cache_root_for(
    windows_local_app_data: Option<&Path>,
    xdg_cache_home: Option<&Path>,
    home: Option<&Path>,
) -> io::Result<PathBuf>;
```

On Windows, use `LOCALAPPDATA\Neomacs` and require an absolute LocalAppData
path. On Linux, use absolute `XDG_CACHE_HOME/neomacs`; if XDG is absent or
relative, fall back to `HOME/.cache/neomacs`. Add tests for every branch and
missing-home failure.

- [ ] **Step 7: Replace PGO shutdown wiring**

After `recursive_edit` and before consuming `shutdown_request`, call
`native_cache::shutdown(&evaluator)` for both `ReadOnly` and `ReadWrite`.
Read-only shutdown publishes only its usage journal; generation emission
remains `ReadWrite`-only. Bootstrap/raw never initialize the cache.

- [ ] **Step 8: Include safe cache metadata in build/status output**

Show build ID, lld version, access state, hit/load counters, and last cache error. Keep `--version` deterministic and omit user paths unless explicitly requesting status.

- [ ] **Step 9: Run host/core lifecycle tests**

Run:

```powershell
cargo test -p neomacs --lib native_cache
cargo test -p neomacs --lib runtime_mode
cargo test -p neomacs --lib default_native_cache_root
cargo test -p neomacs --test neomacs_daemon_cli
cargo test -p neovm-core --lib native_cache
```

Expected: pass; daemon lifecycle remains intact.

- [ ] **Step 10: Commit**

```powershell
git add neomacs-bin/src/args.rs neomacs-bin/src/args_test.rs neomacs-bin/src/main.rs neomacs-bin/src/main_test.rs neomacs-bin/src/build_info.rs neomacs-bin/src/build_info_test.rs
git commit -m "feat(runtime): enable native cache by host mode"
```

---

### Task 11: Stage rust-lld, builtins, metadata, and licenses with xtask

**Files:**
- Modify: `xtask/src/main.rs`
- Modify: `xtask/src/main_test.rs`
- Create: `etc/licenses/llvm/LICENSE.TXT`
- Create: `etc/licenses/llvm/NOTICE.TXT`

**Interfaces:**
- Produces staged files:
  ```text
  target/<profile>/native-cache/ld.lld          # actual rust-lld, Linux only
  target/<profile>/native-cache/lld-link.exe    # actual rust-lld, Windows only
  target/<profile>/native-cache/native-cache-builtins.o|obj
  target/<profile>/native-cache/native-cache-metadata.json
  target/<profile>/native-cache/LICENSE.TXT
  target/<profile>/native-cache/NOTICE.TXT
  ```
- Consumes: supported Task 2 `OUT_DIR/native-cache` metadata and
  `rustc --print sysroot`. Unsupported metadata is a valid direct-build
  result but is rejected when a release Linux/Windows package is requested.

- [ ] **Step 1: Write failing staging contract tests**

Add fixtures for Linux and Windows metadata. Assert `--skip-build` fails when the staged toolchain is absent, target/build IDs mismatch, lld digest mismatches, or the builtins object has undefined symbols.

- [ ] **Step 2: Run xtask tests and confirm failure**

Run:

```powershell
cargo test -p xtask --bin xtask native_cache_toolchain
```

Expected: staging functions are missing.

- [ ] **Step 3: Implement `stage_native_cache_toolchain`**

After Cargo build verification and before pdump work:

```rust
fn stage_native_cache_toolchain(
    options: &FreshBuildOptions,
    paths: &PipelinePaths,
) -> Result<(), Box<dyn Error>>;
```

Find the unique supported
`target/<profile>/build/neovm-core-*/out/native-cache/build-metadata.json`
whose build ID matches the binary. This build-only input may contain absolute
source paths. Validate the target-specific `gcc-ld/ld.lld` or
`gcc-ld/lld-link` wrapper, then resolve the sibling host
`lib/rustlib/<host>/bin/rust-lld[.exe]`, verify the actual executable's
SHA-256 and version, and copy those actual bytes under the staged driver name.
Record the linker flavor (`gnu` or `link`) and staged driver name, copy
builtins/licenses, and write a new portable `native-cache-metadata.json`
containing basenames only. Never copy `build-metadata.json` into the
staged/package tree. Set executable mode on Unix. The staged driver is later
invoked as `<driver> -flavor <flavor> ...`; the wrapper is never packaged or
used at runtime.

- [ ] **Step 4: Verify staged metadata**

Metadata fields are exactly:

```json
{
  "format_version": 1,
  "supported": true,
  "unsupported_reason": "",
  "target": "x86_64-pc-windows-msvc",
  "host": "x86_64-pc-windows-msvc",
  "build_id": "<64 lowercase hex>",
  "linker_flavor": "link",
  "linker_source_file": "rust-lld.exe",
  "staged_linker_file": "lld-link.exe",
  "lld_version": "LLD 22.1.2 (https://github.com/rust-lang/llvm-project.git 1cb4e3833c1919c2e6fb579a23ac0e2b22587b7e)",
  "lld_sha256": "<64 lowercase hex>",
  "builtins_file": "native-cache-builtins.obj",
  "builtins_sha256": "<64 lowercase hex>"
}
```

Linux uses its actual target, `gnu` flavor, `ld.lld` staged name, and `.o`
filename. Reject Windows GNU and any unsupported metadata. The staged linker
and builtins digests must be recomputed from the copied bytes and match both
portable metadata and the embedded build identity.

- [ ] **Step 5: Run xtask tests and a dry build plan**

Run:

```powershell
cargo test -p xtask --bin xtask native_cache_toolchain
cargo xtask fresh-build --profile dev --dry-run
```

Expected: tests pass and the dry run includes native-cache staging.

- [ ] **Step 6: Commit**

```powershell
git add xtask/src/main.rs xtask/src/main_test.rs etc/licenses/llvm
git commit -m "build(jit): stage bundled native cache linker"
```

---

### Task 12: Package and verify the Linux native-cache toolchain

**Files:**
- Modify: `scripts/package-release.sh`
- Modify: `scripts/package-deb.sh`
- Modify: `scripts/package-rpm.sh`
- Modify: `scripts/package-appimage.sh`
- Modify: `scripts/test-linux-release-artifacts.sh`

**Interfaces:**
- Produces archive payload:
  ```text
  share/neomacs/native-cache/ld.lld
  share/neomacs/native-cache/native-cache-builtins.o
  share/neomacs/native-cache/native-cache-metadata.json
  share/neomacs/licenses/llvm/LICENSE.TXT
  share/neomacs/licenses/llvm/NOTICE.TXT
  ```
- Consumes: Task 11 staged directory.

- [ ] **Step 1: Add failing archive contract assertions**

Extend `test-linux-release-artifacts.sh` to unpack each selected format and
require all five paths, executable staged actual `rust-lld` bytes under
`ld.lld`, matching SHA-256 values for both linker and builtins, supported
Linux target/build ID, `linker_flavor: "gnu"`, and zero undefined builtins
symbols via `readelf`/`nm`. The contract must also run the isolated linker
probe from the unpacked archive with compiler/linker directories removed from
`PATH`.

- [ ] **Step 2: Run the contract against a synthetic incomplete package**

Create a tar fixture containing the ordinary binary/runtime paths but no
`share/neomacs/native-cache`, then run:

```powershell
bash scripts/test-linux-release-artifacts.sh --formats tar --tar-version test
```

Expected: fails with
`missing share/neomacs/native-cache/native-cache-metadata.json`.

- [ ] **Step 3: Copy and digest-check the staged directory in each packager**

Make `package-release.sh` copy `target/<profile>/native-cache` into
`share/neomacs/native-cache` and copy LLVM notices into
`share/neomacs/licenses/llvm`. Recompute SHA-256 for the staged actual
`ld.lld` and builtins bytes and compare with portable metadata before
packaging. Ensure deb/rpm/AppImage payload construction retains the complete
`share/neomacs` tree.

- [ ] **Step 4: Report package-size contribution**

Print total archive size and the summed bytes of lld, builtins, metadata, and notices. Do not silently omit lld to reduce size.

- [ ] **Step 5: Run Linux packaging contracts**

Run on Linux CI:

```bash
./scripts/package-release.sh --target x86_64-unknown-linux-gnu --skip-build --no-smoke
./scripts/test-linux-release-artifacts.sh --formats tar --tar-version 0.0.12
```

Expected: pass without resolving a linker from `PATH`; the archive uses only
the staged actual rust-lld and its leading `-flavor gnu` invocation.

- [ ] **Step 6: Commit**

```powershell
git add scripts/package-release.sh scripts/package-deb.sh scripts/package-rpm.sh scripts/package-appimage.sh scripts/test-linux-release-artifacts.sh
git commit -m "build(linux): package native cache toolchain"
```

---

### Task 13: Package and verify the Windows native-cache toolchain

**Files:**
- Modify: `scripts/emacs-build.ps1`
- Modify: `scripts/test-windows-gstreamer-setup.ps1`
- Create: `scripts/test-packaged-native-cache.ps1`

**Interfaces:**
- Produces ZIP payload:
  ```text
  share\neomacs\native-cache\lld-link.exe
  share\neomacs\native-cache\native-cache-builtins.obj
  share\neomacs\native-cache\native-cache-metadata.json
  share\neomacs\licenses\llvm\LICENSE.TXT
  share\neomacs\licenses\llvm\NOTICE.TXT
  ```
- Consumes: Task 11 staged x86_64-pc-windows-msvc toolchain.

- [ ] **Step 1: Add failing PowerShell packaging tests**

Extend the build-wrapper contract to require supported MSVC target metadata,
reject Windows GNU, verify SHA-256 for the staged actual `rust-lld.exe` and
builtins object, require all ZIP entries, and run the isolated
`lld-link.exe -flavor link` probe with Rust/LLVM/Visual Studio directories
removed from `PATH`. Add a unit fixture whose builtins object reports an
undefined symbol and assert packaging fails.

- [ ] **Step 2: Run the contract and confirm failure**

Run:

```powershell
.\scripts\test-windows-gstreamer-setup.ps1
```

Expected: native-cache package assertions fail.

- [ ] **Step 3: Stage files and digest-check the Windows package**

After `fresh-build` succeeds, copy the staged directory into
`share\neomacs\native-cache`, copy notices, validate supported metadata and
recomputed linker/builtins digests, and add all paths to the ZIP required-entry
check. The staged `lld-link.exe` is the actual rust-lld bytes and runtime
invokes it with leading `-flavor link`; no PATH lookup or sysroot wrapper is
used.

- [ ] **Step 4: Report size contribution**

Print total ZIP size and separate lld/builtins/native-cache payload bytes.

- [ ] **Step 5: Run Windows packaging contracts**

Run:

```powershell
.\scripts\test-windows-gstreamer-setup.ps1
.\scripts\emacs-build.ps1 -Profile dev-release -SkipPackage
```

Expected: wrapper contract passes; dev-release build stages a valid MSVC
native-cache toolchain without packaging and the isolated linker probe passes.

- [ ] **Step 6: Commit**

```powershell
git add scripts/emacs-build.ps1 scripts/test-windows-gstreamer-setup.ps1 scripts/test-packaged-native-cache.ps1
git commit -m "build(windows): package native cache toolchain"
```

---

### Task 14: Add packaged two-process tests and release gates

**Files:**
- Create: `test/native-cache/packaged-runtime-scenario.el`
- Create: `scripts/test-packaged-native-cache.sh`
- Modify: `scripts/test-packaged-native-cache.ps1`
- Modify: `scripts/test-linux-release-artifacts.sh`
- Modify: `.github/workflows/release.yml`

**Interfaces:**
- Produces one cross-platform scenario with modes `emit`, `hit`, and
  `stale-prewarm`, driven through a named daemon so emission uses the real
  `ReadWrite` host policy.
- Consumes: Tasks 5-13 complete packaged runtime.

- [ ] **Step 1: Write the shared Lisp workload**

The fixture must define named bytecode functions that:

- run past the JIT threshold during `emit`;
- exercise memory lowering;
- create a native frame over 4 KiB;
- return a deterministic value;
- signal a deterministic Lisp error; and
- redefine one symbol with the same prekey but incompatible content for `stale-prewarm`.

Expose fixture entry points and write machine-readable result lines:

```lisp
(defun native-cache-test-run (mode)
  (let ((result (pcase mode
                  ("emit" (native-cache-test-emit))
                  ("hit" (native-cache-test-hit))
                  ("stale-prewarm" (native-cache-test-stale-prewarm)))))
    (princ (format "NATIVE-CACHE result=%S mode=%s\n" result mode))
    result))
```

- [ ] **Step 2: Implement the Linux two-process runner**

Unpack the archive into a temporary root, set temporary `HOME` and cache root,
and remove compiler/linker directories from `PATH`. For each phase, start:

```bash
<package>/bin/neomacs --daemon=native-cache-test -Q
<package>/bin/neomacsclient --socket-name native-cache-test \
  --eval "(progn (load \"<fixture>\") (native-cache-test-run \"emit\") (kill-emacs 0))"
```

Wait for the daemon to exit before inspecting the cache. Repeat with `hit` in
a fresh daemon. Assert:

1. `emit` publishes exactly one generation;
2. its library has no undefined symbols;
3. `hit` records one AOT load and zero JIT compiles on the first invocation; and
4. `stale-prewarm` stays interpreted until the ordinary threshold.

- [ ] **Step 3: Implement the Windows two-process runner**

Use `Start-Process` with a minimal environment excluding Rust, Cargo, LLVM,
Git, and Visual Studio paths. Drive the same named-daemon/client sequence.
Rename `neomacs.exe` before the hit run. Assert the same counters plus
`dumpbin /DEPENDENTS` showing no import of the executable.

- [ ] **Step 4: Add host-mode scenarios**

Run final `--batch` against an existing generation and assert it reads but does not publish. Run bootstrap/temacs test hosts and assert they neither read nor write.

- [ ] **Step 5: Add release workflow gates**

After Linux and Windows archives are built, invoke their packaged runners before artifact upload. Preserve existing macOS verification and create-release dependencies.

- [ ] **Step 6: Run targeted release tests**

Run:

```powershell
cargo test -p neovm-core --features jit --test aot_pgo
cargo test -p neomacs --lib native_cache
.\scripts\test-windows-gstreamer-setup.ps1
```

On Linux CI also run:

```bash
./scripts/test-linux-release-artifacts.sh --formats tar --tar-version 0.0.12
./scripts/test-packaged-native-cache.sh dist/neomacs-0.0.12-x86_64-unknown-linux-gnu.tar.gz
```

Expected: all pass.

- [ ] **Step 7: Record performance and size baselines**

Capture:

- empty-cache startup;
- 4,096-leaf index/prewarm startup;
- maintenance capped at 50 ms;
- first-call native hit versus cold JIT;
- shutdown capped at two seconds; and
- archive size before/after bundled lld.

Store benchmark drivers with existing performance tooling; do not commit transient result logs.

- [ ] **Step 8: Run formatting and focused final verification**

Run:

```powershell
cargo fmt --all -- --check
cargo test -p neovm-core --lib native_cache
cargo test -p neovm-core --features jit --test aot_pgo
cargo test -p neovm-core --features jit --test aot_spec
cargo test -p neovm-core --features jit --test aot_call_bearing
cargo test -p neomacs --lib native_cache
cargo test -p xtask --bin xtask native_cache_toolchain
```

Expected: all pass. Do not substitute a full `neovm-core` run for these targeted suites.

- [ ] **Step 9: Commit**

```powershell
git add test/native-cache scripts/test-packaged-native-cache.sh scripts/test-packaged-native-cache.ps1 scripts/test-linux-release-artifacts.sh .github/workflows/release.yml
git commit -m "test(jit): gate packaged persistent native cache"
```
