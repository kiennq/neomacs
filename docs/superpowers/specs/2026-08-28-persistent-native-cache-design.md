# Persistent Native Elisp Cache

## Problem

Neomacs already compiles hot Elisp bytecode to machine code with Cranelift.
That JIT code is held in a thread-local, process-local cache, so each new
session must rediscover hot functions and compile them again.

Neomacs also has two AOT mechanisms:

- dump-time preload generation writes one shared library beside the pdump; and
- an opt-in PGO path writes proven-hot JIT leaves as content-addressed shared
  libraries at clean shutdown and can load them in a later process.

The PGO path already provides most of the required compiler and validation
machinery, but it is controlled by internal environment variables, is off by
default, depends on an ambient system linker, and is wired for Linux rather
than packaged as a cross-platform product feature.

## Goal

Provide a default-on persistent native cache for JIT-enabled Neomacs builds.
Hot bytecode functions compiled in one session should be available as native
code from their first call in a later compatible session.

The cache must:

- preserve the interpreter as the semantic source of truth;
- support packaged Linux and Windows builds without requiring a developer
  toolchain on the user's machine;
- remain an optional optimization even when enabled by default;
- fail through to the ordinary JIT and interpreter on every cache miss or
  cache-specific error;
- bound startup, shutdown, and disk costs; and
- expose enough status to diagnose why caching is unavailable or ineffective.

## Non-goals

- No GNU Emacs `.eln` compatibility.
- No user-facing `native-compile` command for selected source files.
- No redistributable native package format.
- No replacement for `.el` or `.elc` files.
- No guarantee that every bytecode function can be persisted. The initial
  feature uses the existing conservative AOT-supported subset.
- No background compiler or worker-thread access to live VM state.
- No custom executable-memory image or relocation format.

The native files are disposable machine-local cache entries, not bytecode and
not portable application artifacts.

### Build-support matrix

Native-cache build metadata is generated on every target, but support is
explicit rather than assumed. The build script considers
`CARGO_FEATURE_JIT`, `TARGET`, `HOST`, the Cargo target family, and whether a
usable target driver and host `rust-lld` are available. Linux targets and
Windows MSVC targets are the supported release families. macOS is intentionally
omitted from this feature: its builds continue normally with native-cache
support reported as unavailable. Windows GNU, non-JIT builds, and ordinary
cross builds without a usable target driver likewise report unsupported
metadata rather than failing the build.

Unsupported configurations must emit
`NEOMACS_NATIVE_CACHE_SUPPORTED=0` plus all four identity constants with empty
values. They write metadata with `supported: false` and a human-readable
`unsupported_reason`, do not compile the builtins object, do not probe a
linker, do not panic, and do not emit an identity warning. A supported
configuration emits `SUPPORTED=1` only after all inputs have been resolved and
verified. `NEOMACS_NATIVE_CACHE_LLD` is an optional absolute path to the actual
`rust-lld` executable for deliberate cross builds or other toolchain layouts;
it is tracked as a Cargo rebuild input. Release packaging, not the build
script, enforces that Linux and Windows release artifacts are supported.

## Existing execution model

All bytecode calls pass through the existing JIT dispatch seam. A function
starts in the interpreter, accumulates heat, and may become eligible for native
execution. The default JIT threshold is 1,000 calls, with additional loop heat
and restricted on-stack replacement support.

The JIT lowers bytecode through shared Cranelift machinery. Native execution
can return deoptimization or signal statuses; the runtime either reruns the
operation in Tier 0 or resumes the VM at the precise bytecode PC. This behavior
does not change.

The existing AOT path uses Cranelift `ObjectModule`, links the object into a
dynamic library, reconstructs canonical heap constants from a descriptor,
validates the artifact, and inserts an AOT-backed leaf into the same
thread-local compiled-code cache used by the JIT.

## Architecture

Add a platform-neutral `NativeCache` layer around the existing AOT-PGO
producer and loader.

At startup, Neomacs initializes a cache namespace and builds a lightweight
index from generation manifests. It does not eagerly load each dynamic
library. Each manifest maps a cheap function prekey (interned function name,
required arity, and bytecode operation count) to the full content and
compilation-variant hashes stored in that generation.

When a named bytecode function is published into the obarray, the runtime
checks the prekey index. A match marks the function AOT-prewarmed without
hashing its full bytecode. Its first call enters the compiled dispatch seam,
which validates the full content hash and asks `NativeCache` for a matching
leaf. A validated hit is inserted into the thread-local compiled cache.

If prewarming proves stale or the library fails validation, the runtime clears
the prewarm marker and returns to the interpreter. It must not immediately
compile the function merely because the failed prewarm forced early compiled
dispatch. Ordinary heat-based JIT eligibility remains unchanged.

At clean shutdown, Neomacs selects the hottest eligible functions that were
JIT-compiled during the session but were not served from AOT. It emits one
generation containing as many selected leaves as fit within the time, count,
and disk budgets. Each leaf is lowered to an independently inspectable object;
accepted objects are linked together as one generation. The producer uses the
existing AOT lowering, descriptor, constant-reconstruction, and speculation
contracts. Batching avoids one linker process and one loaded DLL per function
without allowing one unsupported leaf to poison the full batch.

The cache remains synchronous at the two existing safe points:

- lazy lookup and loading at a compiled-code cache miss; and
- bounded emission on the evaluator thread during clean shutdown.

No background task may borrow the obarray, bytecode constants, GC roots, or
other live VM state.

## Components

### Native cache configuration

Configuration is resolved before Lisp startup can execute cached code.

Public controls:

- a host that supports native caching initializes it as enabled by default;
- `--no-native-cache` disables reads, writes, maintenance, and linker startup;
- `--native-cache-dir PATH` overrides the OS-default cache root; and
- `NEOMACS_NATIVE_CACHE=0` disables the cache; and
- `NEOMACS_NATIVE_CACHE_DIR` overrides the cache root.

Command-line options take precedence over environment variables. Existing
`NEOVM_AOT*` variables remain internal compatibility and test seams; they are
not the long-term public interface.

The host binary must explicitly initialize the cache. Linking `neovm-core`
does not enable it by itself.

- Final interactive and daemon runs enable reads, maintenance, and clean-exit
  emission.
- Final `--batch` runs enable reads but do not emit at shutdown.
- Bootstrap, temacs, pdump-production, worker, mock-display, and test-host
  modes leave the cache disabled unless a test explicitly forces it.
- `-Q` does not disable the cache because it is a runtime optimization rather
  than user configuration.

Default cache roots follow the platform user-cache convention:

- Linux: `$XDG_CACHE_HOME/neomacs` or the platform fallback when
  `XDG_CACHE_HOME` is unset.
- Windows: the current user's local application-data directory under
  `Neomacs`.

An empty or absent active namespace takes a constant-work fast path: no
maintenance, manifest parsing, or obarray prewarm walk is needed.

### Cache identity

Generations are stored under a namespace containing:

- target operating system and architecture;
- target environment (`msvc` or `gnu` where relevant);
- Neomacs compiler build identity;
- native cache format version; and
- the exact code-generation feature/options fingerprint.

The initial Windows implementation supports the packaged MSVC target. Windows
GNU support is separate follow-up work because its linker driver, import
format, and runtime conventions differ.

The compiler build identity is required in addition to the ABI tag. The ABI
tag describes the callable interface, descriptor layout, status codes, and
runtime shim set. A compiler or lowering bug fix can change generated
instructions without changing that interface. The initial implementation
therefore invalidates native cache entries across Neomacs builds. A later,
explicitly versioned code-generation compatibility identity may relax this
without weakening correctness.

The identity is a deterministic build-time hash embedded in the binary. Its
inputs include the support state, actual `neovm-core` source files,
`Cargo.lock`, target and host, enabled code-generation features, rustc version,
Cranelift versions, native-cache format, code-generation settings, and the
bundled lld and builtins bytes when supported. Unsupported identities omit
linker and builtins contents. Hashing the source tree means local
code-generation edits produce a new identity without hashing the executable at
startup.

Build metadata always contains exactly these fields: `format_version`,
`supported`, `unsupported_reason`, `target`, `host`, `build_id`,
`linker_flavor`, `linker_source_file`, `staged_linker_file`, `lld_version`,
`lld_sha256`, `builtins_file`, and `builtins_sha256`. Build-only metadata may
contain absolute source paths; the portable staged metadata contains basenames
only. Empty identity fields are used for unsupported builds.

Each leaf is identified by:

- the existing function content hash;
- the AOT ABI tag; and
- a compilation-variant hash covering baked speculation classification and
  other emit-time choices not represented by bytecode content.

The dump-time preload remains separately coupled to its pdump fingerprint.
Per-function native-cache entries do not require a pdump fingerprint because
their constants are reconstructed and verified against the live function, and
dynamic assumptions retain the existing epoch checks and deoptimization path.

### Prewarm index and loader

Each immutable generation has an adjacent manifest containing:

- generation format, namespace, ABI, and build identity;
- dynamic-library filename and whole-file digest;
- generation creation time;
- every leaf's prekey, content hash, variant hash, arity, and symbols; and
- bounded descriptor metadata needed to reject malformed records before
  loading.

Generation manifests are atomically published after their matching library.
The startup index records manifest data and paths only. Dynamic libraries are
loaded lazily by content and variant hash.

The obarray publication path uses the prekey map for both pdump-resident and
later package-loaded functions. Immediately after the final pdump image is
loaded, and before Lisp startup, the host walks existing interned function
cells once and applies the same prekey check. Anonymous closures are not
prewarmed in the initial implementation.

Active-namespace manifest parsing and the pdump function-cell walk share a
50 ms startup budget. If the budget is exhausted, indexing stops cleanly and
unseen functions use ordinary heat-based JIT behavior. Optional maintenance
has a separate 50 ms budget and runs at most once per 24 hours. Work not
finished under either budget is deferred to a later startup.

Before a leaf can execute, the loader must validate:

- namespace and filename structure;
- whole-library digest from the generation manifest;
- content hash;
- compilation-variant compatibility;
- cache format and ABI tag;
- entry and descriptor symbols;
- descriptor magic, version, lengths, and bounds;
- function arity;
- constant reconstruction against the live constant pool; and
- speculation metadata against the live obarray.

At the first prewarmed call, the runtime computes the full content hash and
the canonical live speculation classification. Their exact content/variant
pair selects the generation. If concurrent sessions produced duplicate pairs
in different generations, the loader tries at most four candidates,
newest-first. No exact compatible pair is a normal miss.

Any failed check returns a cache miss and clears the function's prewarm marker.
Successfully loaded libraries remain mapped for at least as long as any
compiled leaf points into them.

A failed prewarm leaves no positive or negative compiled-cache entry. The
function's existing heat remains intact, so later calls reach JIT compilation
only through the ordinary threshold.

The process index is a startup snapshot. Libraries published by another
process during the session become visible only to a later process.

### Candidate selection and emitter

The first implementation retains the existing conservative PGO candidate
rules: named, interned bytecode functions that were proven hot, were compiled
by the JIT, were not already AOT-backed, and satisfy the AOT lowering and
constant-reconstruction subset.

Candidates already indexed under a compatible content and variant hash are
excluded. The remainder are sorted hottest-first and collected into one
generation. Collection and linking stop when any shutdown limit is reached:

- 128 entries per session;
- two seconds of elapsed shutdown time; or
- the cache's entry-count or disk-space limit.

The linker subprocess receives the remaining shutdown budget as a hard
timeout. It runs in a process group or Windows job object so timeout
termination cannot leave child processes behind.

The producer computes the canonical speculation classification and variant
hash before lowering each leaf. It emits each leaf to its own object, inspects
that object's imports, and accepts or rejects the leaf independently. Only
accepted objects and manifest records enter the generation. There is no
whole-generation retry caused by one unsupported leaf.

An unsupported function is skipped and remains eligible for ordinary JIT
compilation in every session. Expanding AOT opcode, arity, closure, or constant
coverage is separate work.

### Platform linker backend

The emitter produces one multi-leaf Cranelift object and delegates
dynamic-library creation to a platform backend.

Persistent AOT code must not import symbols from the host executable. The
existing native entry already receives a `LeafSidecar`; the persistent backend
extends that sidecar with a versioned table of runtime shim pointers. AOT
lowering calls runtime services indirectly through this table. JIT lowering
may retain direct process-local addresses.

The AOT ABI tag covers the shim-table version, table layout, slot order, and
each pointed-to function signature. Changing any of them invalidates existing
native generations.

The emitted object is inspected before linking. Its undefined-symbol set must
be empty. Operations that would require unhandled libcalls, stack probes, or
other platform runtime symbols are either satisfied inside the generation or
rejected as unsupported and left to the JIT.

Code-generation settings explicitly enable inline stack probing, so large
native frames do not import a platform `__chkstk`-style symbol. Approved
Cranelift libcalls use fixed local symbol names supplied by a target-specific
native-cache builtins object. That object is produced and verified at Neomacs
build time, linked into each generation, and covered by the compiler build
identity. Unapproved libcalls reject only the leaf that requested them.

The code-generation options fingerprint covers probestack enablement and
strategy, libcall naming, calling convention, relocation model, target ISA
flags, and optimization level.

Linux:

- produces an ELF shared object;
- links with no default libraries; and
- loads entries through `libloading`/`dlopen`.

Windows:

- produces a PE DLL;
- links with `/DLL`, `/NOENTRY`, and `/NODEFAULTLIB`;
- requires no import library and has no executable-name dependency; and
- loads entries with explicit safe `LoadLibraryExW` search flags and resolves
  exported entry symbols through `GetProcAddress`.

Release packages include:

- the actual `rust-lld` bytes staged as `ld.lld` on Linux or `lld-link.exe` on
  Windows, invoked by absolute runtime-relative path with leading
  `-flavor gnu` or `-flavor link`;
- the matching target-specific native-cache builtins object; and
- metadata identifying its version and the native-cache compiler build.

The `gcc-ld/ld.lld` and `gcc-ld/lld-link` files in a Rust sysroot are wrappers;
they require the sibling host `lib/rustlib/<host>/bin/rust-lld[.exe]`.
Build-time resolution hashes the actual sibling `rust-lld` bytes, not wrapper
bytes. The linker is invoked directly with structured arguments, never
through a shell. Cache build identity covers the Neomacs build, sidecar ABI,
and actual bundled lld version and bytes.

Tests must force representative memory operations and a native frame larger
than 4 KiB. The result must either have no undefined symbols and load
successfully or be rejected before publication; a trivial leaf is not an
adequate Windows linker test.

### Publication and concurrency

Generation libraries and manifests are immutable. A generation ID is the hash
of its namespace plus the sorted `(content hash, variant hash)` leaf set. An
existing final generation is never rewritten.

The emitter writes objects, the linked library, and its manifest inside one
uniquely named temporary directory. It publishes the complete generation with
one atomic no-replace rename of that directory. A final generation directory
therefore contains a matching library/manifest pair or does not exist.
Temporary directory names cannot match the loader's index pattern.

Linux uses `renameat2(RENAME_NOREPLACE)` for the same-filesystem generation
directory rename. If that operation is unavailable, writes are disabled rather
than emulating publication with a racy check-then-rename sequence. Windows uses
a same-volume directory move without replacement. A destination-exists result
is treated as concurrent publication success after the existing generation
validates.

Concurrent Neomacs processes may emit the same generation. If another process
wins publication, the loser discards only its own temporary output. A process
must never delete another process's active temporary file.

## Cache lifecycle and disk policy

The cache directory is private to the current OS user. Initialization creates
it with restrictive platform permissions and rejects a directory that is not
safe for executable cache content.

Cache maintenance runs at startup before any entry is dynamically loaded. This
ordering is mandatory on Windows because loaded DLLs cannot be reliably
replaced or deleted.

Maintenance:

- removes temporary generation directories older than 24 hours;
- removes inactive compiler-build namespaces not used for 30 days;
- prunes least-recently-used generations when the leaf-count or disk budget is
  exceeded; and
- leaves the active process's loaded entries untouched.

The initial global limits are 4,096 cached leaves and 512 MiB across all build
namespaces. Because libraries are immutable batches, pruning removes a whole
generation at a time until both limits are satisfied. These defaults may
become user-configurable after measurements show a concrete need; the first
implementation keeps them fixed so the public configuration surface stays
small.

Generation use is recorded without touching DLL timestamps or rewriting
immutable files. At clean shutdown, each process atomically publishes a
uniquely named usage journal containing the generation IDs it loaded. Startup
maintenance merges every complete journal into replaceable recency metadata
and then removes the merged journal; incomplete temporary journals follow the
24-hour cleanup rule. Generation creation time is the recency fallback when no
usage record exists. Loss or corruption of recency metadata may reduce cache
effectiveness but must not invalidate an otherwise valid library.

Pruning first atomically renames a generation directory to a uniquely named
trash directory, then deletes its contents. A sharing violation or rename
failure skips that generation. Trash directories remain included in disk
accounting and are retried by later maintenance, which also sweeps orphan
generation directories older than 24 hours.

Shutdown never prunes. It checks the space budget before starting a link; if
the cache is full, it stops emitting and defers maintenance and compilation to
a later session.

## Failure handling

Native caching is strictly additive. Cache-specific failures must never make
startup, evaluation, or shutdown fail.

- An unavailable or insecure cache directory disables native caching for the
  session and records one initialization diagnostic.
- A missing or unusable bundled linker disables new writes. Existing validated
  entries may still be read.
- A malformed, stale, incompatible, or unloadable entry is treated as a miss
  and may be quarantined or removed during a safe maintenance phase.
- A link, write, rename, or concurrent-publication error skips that candidate.
- A crash or forced termination loses only the current session's pending
  writes.
- Exceeding a time, count, or space limit stops emission normally.

The loader disables further cache reads for the session after three
consecutive dynamic-load failures that indicate a platform policy or
filesystem problem rather than an entry mismatch. Before the first shutdown
emission, the backend links and loads a tiny import-free probe in the cache
directory within the same time budget. Failure makes the session read-only and
prevents repeatedly paying link costs on a `noexec`, WDAC-, AppLocker-, or
antivirus-blocked cache location. The probe is isolated: it invokes the same
packaged actual rust-lld bytes used for generation linking, through the
absolute staged driver path and with the leading `-flavor gnu` or
`-flavor link` argument, against a tiny temporary output in the private cache
staging directory. It never searches `PATH`, uses a shell, or runs for
unsupported builds.

Repeated validation failure for the same content and variant quarantines that
manifest entry for the session. Quarantined or failed entries do not suppress
later emission. Startup maintenance records the failed leaf as unusable and
must remove the generation when none of its leaves remain usable.

Two consecutive probe or link timeouts persist a write backoff in namespace
metadata. Writes are disabled for 24 hours, doubling after each repeated
failure to a maximum of seven days. A successful probe and generation link
clear the backoff.

Repeated per-function failures are aggregated. Logging must identify the
failed phase and OS error without flooding the user or printing Lisp values
that may contain sensitive data.

The ordinary JIT and Tier-0 interpreter remain available in all these cases.

## Status and control

A small status surface reports:

- enabled, read-only, or disabled state;
- cache root and active namespace;
- entries and disk use;
- lookup hits and misses;
- load-validation failures;
- emitted and skipped entries;
- shutdown budget exhaustion; and
- the last initialization or linker error.

`M-x native-cache-status` displays this report. `M-x native-cache-clear`
creates a `.clear-on-start` marker in the active cache root. The next cache
initialization for that same root clears eligible contents before indexing and
then removes the marker. Starting with a different override does not clear the
old root. Clearing is deferred on every platform so behavior does not differ
when Windows DLLs are already mapped.

The status surface is diagnostic only and does not expose loaded native
pointers or raw constant data.

## Security

The cache contains executable code and is trusted only within the current
user's security boundary.

- The default directory must not be writable by other users.
- Symlink/reparse-point and ownership checks must follow platform-appropriate
  secure-directory rules.
- Linker paths come from the packaged runtime, not the cache directory or
  ambient `PATH`.
- Linker invocation uses fixed arguments and validated, process-generated
  paths.
- The loader indexes only exact expected filenames inside the active namespace.
- Descriptor lengths and counts remain bounded before allocation or pointer
  reads.
- Cache paths and errors may be logged; generated code bytes and Lisp constant
  contents may not.

Native caching does not protect against malicious code already executing as
the same user. Its directory policy prevents a lower-privileged local user
from turning cache loading into privilege crossing.

## Packaging

Linux and Windows release archives must contain the pinned lld binary in a
known runtime-relative location.

Packaging verification checks:

- the linker exists and is executable;
- its version matches build metadata;
- the packaged native-cache builtins object has only the approved definitions
  and no undefined symbols;
- a representative generated object has no undefined symbols; and
- a representative generated cache library can be linked and loaded without using
  tools from `PATH`.

The package-size increase from lld must be reported in release build output so
it remains visible, but package size alone does not disable the approved
self-contained design.

## Verification

### Unit tests

Targeted unit tests cover:

- configuration precedence and disable behavior;
- command-line option removal before the Lisp `command-line-args` handoff;
- final interactive, daemon, batch, bootstrap, worker, and test-host gating;
- `-Q` retaining native-cache behavior;
- default cache-root selection;
- deterministic compiler-build identity changes from source, toolchain,
  feature, target, or bundled-lld changes;
- generation manifest parsing and index filtering;
- function-publication prewarming for pdump and later package definitions;
- exact content/variant selection and the four-candidate lookup bound;
- failed prewarming clearing the marker without triggering early JIT;
- descriptor and constant validation failures;
- compilation-variant matching and stale-variant quarantine;
- Linux and Windows linker command construction;
- sidecar shim-table ABI validation;
- inline probestack and approved local-libcall code-generation settings;
- per-leaf import inspection and rejection within a surviving batch;
- rejection of every unapproved undefined symbol;
- atomic no-replace publication and concurrent-writer outcomes;
- stale-temporary cleanup;
- usage-journal merge and generation-level LRU pruning;
- size, count, and time budget decisions;
- linker timeout process-tree termination;
- systematic-load-failure self-disable and the write-capability probe;
- persistent timeout backoff and reset after success;
- startup-before-load pruning order; and
- status aggregation without per-function warning floods.

### Cross-process integration tests

Linux and Windows CI run the same packaged-runtime scenario:

1. Start with an empty cache.
2. Execute deterministic bytecode functions past the JIT threshold, including
   a leaf that forces representative memory lowering and one whose native
   frame would exceed 4 KiB.
3. Exit cleanly and assert that one native generation and its manifest were
   published through the packaged lld path and contain no undefined symbols.
4. Start a second process from the same package.
5. Invoke a cached function once and assert that it is served from the native
   cache on that first call, returns and signals identically, records an AOT
   load, and records no JIT compilation for that leaf.
6. Publish a same-prekey function whose content or variant is incompatible,
   invoke it once, and assert that it remains interpreted until the ordinary
   JIT threshold rather than compiling early.

Additional scenarios cover:

- corrupt libraries and descriptors;
- wrong build identities and ABI tags;
- unsupported functions and constants;
- read-only and insecure directories;
- missing or corrupt packaged linker files;
- systematic DLL/shared-object load rejection;
- concurrent writers for the same generation;
- interrupted temporary files;
- batched-generation pruning and usage journals;
- cache-size and shutdown-budget exhaustion; and
- Windows pruning before DLL load and DLL symbol resolution afterward.

Mode scenarios assert that final batch runs may consume existing cache entries
without publishing a generation, while bootstrap, temacs, pdump-production,
worker, and test hosts neither read nor write by default.

Every negative scenario must preserve successful execution through the
existing JIT or interpreter path.

### Packaging tests

Release archive tests verify the bundled actual rust-lld bytes and generated
metadata, including SHA-256 digests for the staged linker and builtins, then
perform the two-process integration test from the unpacked archive with system
compiler and linker directories removed from `PATH`. Renaming the packaged
Windows executable must not affect native-cache loading because generated DLLs
have no executable-module imports.

### Performance measurements

Benchmarks separately report:

- startup initialization and index time by cache entry count;
- the 50 ms active-index/prewarm budget and separate 50 ms maintenance budget;
- cache-hit lookup and dynamic-load latency;
- first-call latency from a native cache hit versus a cold JIT path;
- bounded shutdown emission time; and
- release archive size increase.

Correctness tests enforce the fixed two-second shutdown budget. Performance
reports guide default budget values; they are not replaced by a broad
end-to-end benchmark that could hide cache regressions in unrelated startup
work.

## Acceptance criteria

The feature is complete when:

- packaged Linux and Windows builds enable native caching by default;
- neither platform requires an ambient compiler or linker;
- a hot eligible function persists across two processes and executes natively
  on its first call in the second process;
- a stale prewarm never causes JIT compilation before the ordinary threshold;
- generated cache libraries have no undefined host or platform symbols;
- final batch runs may read but never emit, and non-final host modes never use
  the cache by default;
- incompatible or damaged cache state always falls back safely;
- cache disk use and shutdown work are bounded;
- users can disable caching and inspect its status; and
- dump-time preload and ordinary JIT behavior remain unchanged.
