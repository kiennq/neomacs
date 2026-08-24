# Neomacs Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add cross-platform GNU-style daemon support so `neomacsclient -c -n -a ""` starts or connects to a persistent Neomacs server and creates GUI client frames.

**Architecture:** Native startup records daemon mode before any threads or GPU work, then runs the existing Lisp startup against one visible non-window-system terminal frame. The renderer defers its primary native window until the first GUI client frame and re-arms that path after the last GUI frame closes. Unix uses local sockets and spawn/exec readiness pipes; Windows uses TCP auth files and a named readiness event.

**Tech Stack:** Rust, Emacs Lisp startup/server code, winit, wgpu, Unix `libc`, Windows `windows-sys`, Cargo tests, nextest-compatible integration tests.

**Spec:** `docs/.superpowers/specs/2026-08-23-neomacs-daemon-design.md`

## Global Constraints

- Preserve ordinary GUI, TTY, batch, and explicit `--server-file` behavior.
- Support `--daemon`, `--bg-daemon[=NAME]`, and `--fg-daemon[=NAME]`.
- Support GUI client frames only; `neomacsclient -t/-nw` remains unsupported.
- A daemon starts with one visible selected frame whose window system is nil.
- Do not create a native window before the first GUI client-frame request.
- Closing the last GUI client frame must not terminate the daemon.
- Unix backgrounding must spawn/exec a clean child; never continue after a bare fork.
- Windows defaults to TCP auth files and uses `HOME`, `APPDATA`, then `USERPROFILE`.
- Daemon readiness timeout is 30 seconds.
- Land all daemon/client work as one commit after whitespace commit `9bf40fd63`.
- Do not push automatically.

---

## File Map

### Core daemon state

- Create `neovm-core/src/emacs_core/daemon.rs`: process-global daemon request, initialization state, and readiness signaling.
- Modify `neovm-core/src/emacs_core/mod.rs`: export the daemon module.
- Modify `neovm-core/src/emacs_core/builtins/misc_pure.rs`: implement `daemonp` and `daemon-initialized`.
- Modify `neovm-core/src/emacs_core/builtins/mod.rs`: register the context-aware daemon builtins.
- Modify/add tests beside `misc_pure` and daemon state.

### Native startup

- Modify `neomacs-bin/src/args.rs` and its tests: restore GNU daemon option priority rows.
- Modify `neomacs-bin/src/main.rs` and `main_test.rs`: parse daemon requests, configure core state, select daemon startup topology, and skip initial GUI publication.
- Create `neomacs-bin/src/daemon.rs`: background child creation and readiness wait.
- Modify `neomacs-bin/Cargo.toml`: add only platform dependencies already present in the workspace if needed.

### Server transport

- Modify the `make-network-process` subfeature declaration and its tests in `neovm-core`: advertise local sockets only on Unix.

### Renderer lifecycle

- Modify `neomacs-display-runtime/src/render_thread/state.rs`: daemon/deferred-primary lifecycle flags.
- Modify `bootstrap.rs`, `lifecycle.rs`, `frame_windows.rs`, `window_events.rs`, and command-processing tests: deferred primary creation, persistent GPU state, and re-arming after last-window close.
- Modify the render-loop public entry points and their callers in `neomacs-bin/src/main.rs`.

### Client

- Modify `neomacs-bin/src/bin/neomacsclient.rs`: default Windows TCP discovery, platform display request, daemon executable discovery, empty-alternate startup, reconnect, and platform shell execution.
- Modify `neomacs-bin/tests/neomacsclient_cli.rs`: transport and daemon retry tests.

### End-to-end coverage

- Modify `neomacs-gui-tests/src/lib.rs` or the existing GUI process harness module: foreground daemon lifecycle and repeated GUI-frame creation.
- Update command help only where behavior becomes supported.

---

### Task 1: Correct Platform Server Capabilities

**Files:**
- Modify: the `make_network_process_subfeatures` implementation in `neovm-core/src/emacs_core/process.rs` or its current split module.
- Test: the adjacent process test containing the current `(:family local)` expectations.

**Interfaces:**
- Consumes: target `cfg(unix)`.
- Produces: Lisp `(featurep 'make-network-process '(:family local))` is non-nil only on Unix.

- [ ] **Step 1: Write the failing platform test**

Use the existing subfeature test and make its expectation explicit:

```rust
#[test]
fn make_network_process_local_family_matches_platform() {
    let features = make_network_process_subfeatures();
    assert_eq!(
        features_contain_local_family(features),
        cfg!(unix),
        "local sockets must not be advertised where make-network-process rejects them"
    );
}
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```powershell
cargo test -p neovm-core --lib make_network_process_local_family_matches_platform
```

Expected on Windows: FAIL because `(:family local)` is currently advertised.

- [ ] **Step 3: Gate only the local-family entry**

Build the subfeature list normally, but append the local-family capability only under Unix:

```rust
#[cfg(unix)]
features.push(Value::list(vec![
    Value::symbol(":family"),
    Value::symbol("local"),
]));
```

Do not change TCP/IPv4 capabilities.

- [ ] **Step 4: Run focused and process tests**

```powershell
cargo test -p neovm-core --lib make_network_process
```

Expected: all selected tests PASS.

- [ ] **Step 5: Record a clean task checkpoint**

```powershell
git diff --check
git status --short
```

Expected: no whitespace errors. Do not commit yet; Task 8 creates the single daemon/client commit required by the spec.

---

### Task 2: Add Core Daemon State and Lisp Semantics

**Files:**
- Create: `neovm-core/src/emacs_core/daemon.rs`
- Modify: `neovm-core/src/emacs_core/mod.rs`
- Modify: `neovm-core/src/emacs_core/builtins/misc_pure.rs`
- Modify: `neovm-core/src/emacs_core/builtins/mod.rs`
- Test: `neovm-core/src/emacs_core/daemon_test.rs` or the repository-standard adjacent test module.

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DaemonRequest {
    Background { name: Option<String> },
    Foreground { name: Option<String> },
}

pub fn configure(request: Option<DaemonRequest>) -> Result<(), DaemonStateError>;
pub fn daemon_value() -> Value;
pub fn mark_initialized() -> Result<(), DaemonStateError>;
pub fn is_daemon() -> bool;
pub fn is_initialized() -> bool;
```

- Readiness inputs:

```text
NEOMACS_DAEMON_READY_FD=<unix write fd>
NEOMACS_DAEMON_READY_EVENT=<windows event name>
```

- [ ] **Step 1: Write failing daemon-state tests**

Cover default, named, duplicate initialization, and non-daemon errors:

```rust
#[test]
fn named_daemon_reports_name_and_initializes_once() {
    reset_for_tests();
    configure(Some(DaemonRequest::Foreground {
        name: Some("work".into()),
    }))
    .unwrap();

    assert_eq!(daemon_value().as_utf8_str(), Some("work"));
    assert!(!is_initialized());
    mark_initialized().unwrap();
    assert!(is_initialized());
    assert_eq!(
        mark_initialized(),
        Err(DaemonStateError::AlreadyInitialized)
    );
}
```

Add builtin tests asserting:

```elisp
(daemonp) ; => nil, t, or name
```

and that `daemon-initialized` signals outside daemon mode.

- [ ] **Step 2: Run tests and verify RED**

```powershell
cargo test -p neovm-core --lib daemon
```

Expected: compile failure because the daemon module/API does not exist.

- [ ] **Step 3: Implement process-global state**

Use one `OnceLock<Mutex<DaemonState>>`. Keep mutations small and return typed errors:

```rust
#[derive(Default)]
struct DaemonState {
    request: Option<DaemonRequest>,
    initialized: bool,
}
```

`daemon_value` maps no request to nil, unnamed request to `t`, and named request to a Lisp string.

- [ ] **Step 4: Implement readiness signaling**

`mark_initialized` changes state before signaling.

Unix:

```rust
if let Some(fd) = std::env::var("NEOMACS_DAEMON_READY_FD")
    .ok()
    .and_then(|value| value.parse::<libc::c_int>().ok())
{
    let byte = [1u8];
    let written = unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
    unsafe { libc::close(fd) };
    if written != 1 {
        return Err(DaemonStateError::ReadinessSignalFailed);
    }
}
```

Windows opens `NEOMACS_DAEMON_READY_EVENT`, calls `SetEvent`, then closes the handle. Use `windows-sys`; do not persist raw handles in Lisp state.

- [ ] **Step 5: Wire builtins**

Change builtin registration so `daemon-initialized` can inspect `after-init-time` through `Context`:

```rust
ctx.defsubr("daemonp", |_ctx, args| builtin_daemonp(args), 0, Some(0));
ctx.defsubr(
    "daemon-initialized",
    |ctx, args| builtin_daemon_initialized(ctx, args),
    0,
    Some(0),
);
```

Return GNU-compatible errors for non-daemon, duplicate initialization, and premature initialization.

- [ ] **Step 6: Run tests**

```powershell
cargo test -p neovm-core --lib daemon
cargo test -p neovm-core --lib daemonp
```

Expected: PASS.

- [ ] **Step 7: Record a clean task checkpoint**

```powershell
git diff --check
git status --short
```

Expected: only daemon/client work is dirty. Do not commit yet.

---

### Task 3: Parse GNU Daemon Options

**Files:**
- Modify: `neomacs-bin/src/args.rs`
- Modify: `neomacs-bin/src/args_test.rs`
- Modify: `neomacs-bin/src/main.rs`
- Modify: `neomacs-bin/src/main_test.rs`

**Interfaces:**
- Consumes: `neovm_core::emacs_core::daemon::DaemonRequest`.
- Produces: `StartupOptions.daemon: Option<DaemonRequest>`.

- [ ] **Step 1: Add failing argument-order tests**

Assert the exact GNU rows:

```rust
StandardArg {
    name: "-daemon",
    longname: Some("--daemon"),
    priority: 99,
    nargs: 0,
}
```

Repeat for `-bg-daemon` and `-fg-daemon`.

Add parser cases:

```rust
parse(["neomacs", "--daemon"]).daemon
    == Some(DaemonRequest::Background { name: None });
parse(["neomacs", "--bg-daemon=work"]).daemon
    == Some(DaemonRequest::Background { name: Some("work".into()) });
parse(["neomacs", "--fg-daemon=work"]).daemon
    == Some(DaemonRequest::Foreground { name: Some("work".into()) });
```

Assert daemon options do not remain in `forwarded_args`.

- [ ] **Step 2: Verify RED**

```powershell
cargo test -p neomacs-bin --lib daemon_option
```

Expected: FAIL because `StartupOptions` has no daemon field and rows are absent.

- [ ] **Step 3: Restore GNU priority rows**

Insert after script and before help:

```rust
{ "-daemon", "--daemon", 99, 0 }
{ "-bg-daemon", "--bg-daemon", 99, 0 }
{ "-fg-daemon", "--fg-daemon", 99, 0 }
```

using the repository's `StandardArg` syntax.

- [ ] **Step 4: Parse bare and `=NAME` forms**

Add `daemon` to `StartupOptions` and consume each daemon spelling. Treat `--daemon` and `--bg-daemon` as background requests.

Reject:

```text
daemon + --batch
daemon + --script
daemon + -nw/--no-window-system
more than one daemon option
```

Perform validation after the full parse so option ordering does not affect the error.

- [ ] **Step 5: Configure core state once**

Immediately after parsing and before logging/thread/event-loop setup:

```rust
neovm_core::emacs_core::daemon::configure(startup.daemon.clone())
    .map_err(|error| format!("neomacs: {error}"))?;
```

- [ ] **Step 6: Run parser tests**

```powershell
cargo test -p neomacs-bin --lib daemon_option
cargo test -p neomacs-bin --lib sort_args
```

Expected: PASS.

- [ ] **Step 7: Record a clean task checkpoint**

```powershell
git diff --check
git status --short
```

Expected: no whitespace errors. Do not commit yet.

---

### Task 4: Build GNU-Compatible Foreground Daemon Topology

**Files:**
- Modify: `neomacs-bin/src/main.rs`
- Modify: `neomacs-bin/src/main_test.rs`
- Modify: any existing startup snapshot test fixtures that assert frame topology.

**Interfaces:**
- Consumes: `StartupOptions.daemon`.
- Produces:

```rust
fn configure_daemon_startup_frame(
    eval: &mut Context,
    frame_id: FrameId,
) -> FrameId;
```

- [ ] **Step 1: Write failing topology test**

Construct startup state with `Foreground { name: None }` and assert:

```rust
let result = eval.eval_str(
    "(list (daemonp)
           (= (length (frame-list)) 1)
           (eq (selected-frame) terminal-frame)
           (frame-visible-p terminal-frame)
           (window-system terminal-frame)
           window-system
           initial-window-system
           frame-initial-frame
           default-minibuffer-frame)",
)?;
```

Expected values:

```elisp
(t t t t nil nil nil nil nil)
```

- [ ] **Step 2: Verify RED**

```powershell
cargo test -p neomacs-bin --lib daemon_startup_frame
```

Expected: FAIL because normal GUI startup creates/publishes a GUI frame and `daemonp` is not reflected in topology.

- [ ] **Step 3: Add daemon startup branch**

Keep loading `term/neo-win` so later client frames can call `x-create-frame`, but normalize the bootstrap frame:

```rust
frame.visible = true;
frame.set_window_system(None);
frame.set_display_identity(FrameDisplayIdentity::default());
```

Set:

```rust
window-system = nil
initial-window-system = nil
terminal-frame = bootstrap frame
frame-initial-frame = nil
default-minibuffer-frame = nil
```

Do not call `ensure_gnu_startup_terminal_frame`, because it creates an extra frame and makes it invisible.

- [ ] **Step 4: Skip initial GUI adoption/publication**

In `run_gui_evaluator_worker`, condition these calls on non-daemon startup:

```rust
adopt_existing_primary_gui_frame(...)
publish_gui_frame(...)
```

Install the display host and redisplay callback in both modes; the first later `x-create-frame` must still work.

- [ ] **Step 5: Force daemon-safe logging**

Route daemon logs to file so background mode never writes to a detached console.

- [ ] **Step 6: Run startup tests**

```powershell
cargo test -p neomacs-bin --lib daemon_startup
cargo test -p neomacs-bin --lib configure_gnu_startup_state
```

Expected: PASS.

- [ ] **Step 7: Record a clean task checkpoint**

```powershell
git diff --check
git status --short
```

Expected: no whitespace errors. Do not commit yet.

---

### Task 5: Defer and Re-Arm the Renderer Primary Window

**Files:**
- Modify: `neomacs-display-runtime/src/render_thread/state.rs`
- Modify: `neomacs-display-runtime/src/render_thread/bootstrap.rs`
- Modify: `neomacs-display-runtime/src/render_thread/lifecycle.rs`
- Modify: `neomacs-display-runtime/src/render_thread/frame_windows.rs`
- Modify: `neomacs-display-runtime/src/render_thread/window_events.rs`
- Modify: render-thread tests adjacent to these modules.
- Modify: render-loop entry-point signatures and `neomacs-bin/src/main.rs` callers.

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderStartupMode {
    ImmediatePrimary,
    DeferredPrimary,
}
```

Render-loop entry points accept `RenderStartupMode`.

Lifecycle state adds:

```rust
daemon_mode: bool,
primary_deferred: bool,
device_recovery_deferred: bool,
```

- [ ] **Step 1: Write failing renderer state tests**

Test deferred startup:

```rust
let state = RenderApp::new_for_test(RenderStartupMode::DeferredPrimary);
assert!(state.frame_windows.primary_window().is_none());
assert!(state.gpu.is_none());
```

Test re-arm:

```rust
state.install_first_client_as_primary(frame_id);
state.handle_daemon_primary_destroyed(frame_id);
assert!(state.frame_windows.primary_window().is_none());
assert!(state.frame_windows.primary_emacs_frame_id().is_none());
assert!(state.lifecycle_flags.primary_deferred);
assert!(!state.lifecycle_flags.shutdown_requested);
```

- [ ] **Step 2: Verify RED**

```powershell
cargo test -p neomacs-display-runtime --lib deferred_primary
```

Expected: compile failure because `RenderStartupMode` and daemon lifecycle do not exist.

- [ ] **Step 3: Make resumed tolerate no primary**

Replace the unconditional primary `.expect` in `handle_resumed`. In deferred mode:

```rust
if self.lifecycle_flags.primary_deferred
    && self.frame_windows.primary_window().is_none()
{
    self.lifecycle_flags.resumed_seen = true;
    return;
}
```

Do not initialize clipboard, native window, surface, or GPU yet.

- [ ] **Step 4: Adopt the first requested GUI frame**

When command processing sees a create/realize request and no primary exists:

```rust
self.frame_windows.set_primary_pending(
    GuiFrameWindowState::pending(emacs_frame_id, width, height, title),
);
self.lifecycle_flags.primary_deferred = false;
self.create_pending_primary(event_loop);
```

Run native creation from a callback that has `&ActiveEventLoop`, such as `handle_about_to_wait`; do not create winit windows on the evaluator thread.

- [ ] **Step 5: Initialize or reuse GPU state**

When realizing the first-ever client window, initialize the adapter/device/queue, primary surface, and renderer through the existing `init_wgpu` path. When realizing a later primary after all GUI frames were closed, retain the existing `GpuState` and renderer, create only the new primary surface/window-owned compositor state, and repopulate that frame's glyph atlas.

Add a test seam that counts full GPU initialization separately from primary-surface creation. The second GUI frame must increment only the surface count.

- [ ] **Step 6: Re-arm instead of exiting**

For daemon primary `CloseRequested`/`Destroyed`:

```rust
self.frame_windows.take_primary_window();
self.frame_windows.clear_primary_mapping();
self.lifecycle_flags.primary_deferred = true;
self.lifecycle_flags.shutdown_requested = false;
```

Send the existing `InputEvent::WindowClose` so Lisp deletes the Emacs frame. Do not call `event_loop.exit()`. Non-daemon behavior remains unchanged.

- [ ] **Step 7: Return selection to the terminal frame**

After the GUI frame is deleted, assert `(selected-frame)` is the daemon's non-window-system `terminal-frame`. If the existing Lisp deletion path does not select it, update close-event handling to select the live terminal frame before clearing the native mapping.

- [ ] **Step 8: Defer device-loss recovery without a window**

If no primary native window exists:

```rust
self.lifecycle_flags.device_recovery_deferred = true;
return;
```

Retry after the next primary window is created, then clear the flag only after successful GPU initialization.

- [ ] **Step 9: Verify primary recreation**

Add a state-level test that creates primary A, destroys it, creates primary B with a different Emacs frame ID, and verifies no stale `winit_to_emacs` or primary mapping remains.

- [ ] **Step 10: Run renderer tests**

```powershell
cargo test -p neomacs-display-runtime --lib deferred_primary
cargo test -p neomacs-display-runtime --lib primary
cargo check -p neomacs-bin
```

Expected: PASS.

- [ ] **Step 11: Record a clean task checkpoint**

```powershell
git diff --check
git status --short
```

Expected: no whitespace errors. Do not commit yet.

---

### Task 6: Implement Foreground and Background Daemon Launch

**Files:**
- Create: `neomacs-bin/src/daemon.rs`
- Modify: `neomacs-bin/src/main.rs`
- Modify: `neomacs-bin/src/main_test.rs`
- Modify: `neomacs-bin/Cargo.toml` only if platform imports are not already direct dependencies.

**Interfaces:**
- Produces:

```rust
pub enum DaemonLaunch {
    Continue(StartupOptions),
    ParentExit(i32),
}

pub fn prepare(startup: StartupOptions) -> Result<DaemonLaunch, String>;
```

- Child environment:

```text
NEOMACS_DAEMON_READY_FD
NEOMACS_DAEMON_READY_EVENT
```

- [ ] **Step 1: Write failing launch-plan tests**

Extract pure command construction:

```rust
#[test]
fn background_named_daemon_execs_foreground_child() {
    let command = foreground_child_command(
        Path::new("neomacs"),
        &DaemonRequest::Background {
            name: Some("work".into()),
        },
    );
    assert_eq!(command.args, ["--fg-daemon=work"]);
}
```

Add timeout and child-exit state-machine tests using a fake readiness reader.

- [ ] **Step 2: Verify RED**

```powershell
cargo test -p neomacs-bin --lib daemon_launch
```

Expected: compile failure because `neomacs-bin/src/daemon.rs` does not exist.

- [ ] **Step 3: Implement Unix launch**

Before any threads or event loop:

1. create a pipe;
2. clear close-on-exec on the child write descriptor;
3. spawn `current_exe --fg-daemon[=NAME]`;
4. in `pre_exec`, call `setsid`;
5. pass the write descriptor through `NEOMACS_DAEMON_READY_FD`;
6. close the parent's write descriptor;
7. wait up to 30 seconds for one byte, while checking child exit.

Redirect child stdin/stdout/stderr to null; daemon logs go to file.

- [ ] **Step 4: Implement Windows launch**

1. create a unique manual-reset event;
2. pass its name in `NEOMACS_DAEMON_READY_EVENT`;
3. spawn `current_exe --fg-daemon[=NAME]` with `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`;
4. redirect standard handles;
5. wait up to 30 seconds for the event or child exit;
6. close all handles.

Use PID-specific process termination only when cleaning up the child started by this request.

- [ ] **Step 5: Run `prepare` before logging**

At the native entry point:

```rust
let startup = parse_startup_options(...)?;
match daemon::prepare(startup)? {
    DaemonLaunch::Continue(startup) => run_editor(startup),
    DaemonLaunch::ParentExit(code) => process::exit(code),
}
```

`--fg-daemon` returns `Continue`; non-daemon startup is unchanged.

- [ ] **Step 6: Add a foreground smoke test**

Start a named foreground daemon subprocess, wait for its server endpoint, query `(daemonp)`, and terminate it. Use a short isolated home directory.

- [ ] **Step 7: Run launch tests**

```powershell
cargo test -p neomacs-bin --lib daemon_launch
cargo test -p neomacs-bin --test neomacs_daemon_cli
```

Expected: PASS.

- [ ] **Step 8: Record a clean task checkpoint**

```powershell
git diff --check
git status --short
```

Expected: no whitespace errors. Do not commit yet.

---

### Task 7: Fix Neomacsclient Transport and Empty Alternate Startup

**Files:**
- Modify: `neomacs-bin/src/bin/neomacsclient.rs`
- Modify: `neomacs-bin/tests/neomacsclient_cli.rs`

**Interfaces:**
- Produces:

```rust
enum ServerTarget {
    #[cfg(unix)]
    Local(PathBuf),
    Tcp(PathBuf),
}

fn resolve_server_target(options: &Options) -> Result<ServerTarget, String>;
fn find_neomacs_executable() -> Result<PathBuf, String>;
fn start_daemon_and_retry(prog: &str, options: Options) -> Result<(), String>;
fn selected_server_name(options: &Options) -> String;
fn existing_platform_home_fallback() -> Option<PathBuf>;
```

- Executable precedence: `NEOMACS`, `EMACS`, sibling executable, then `PATH`.

- [ ] **Step 1: Write failing Windows default-target tests**

Use environment guards and temporary directories:

```rust
#[test]
fn windows_default_server_uses_appdata_auth_file() {
    let appdata = temp.path().join("AppData/Roaming");
    write_auth_file(appdata.join(".emacs.d/server/server"));
    with_env([("HOME", None), ("APPDATA", Some(&appdata))], || {
        assert_eq!(
            default_tcp_server_file("server"),
            appdata.join(".emacs.d/server/server")
        );
    });
}
```

Repeat for `HOME`, `USERPROFILE`, explicit `--server-file`, and named server. Add a case with all three environment variables unset and assert the existing platform home fallback is retained.

- [ ] **Step 2: Write failing empty-alternate retry test**

Use the real sibling `neomacs` test binary or an existing process-runner seam. Assert:

```text
first connect fails
daemon command runs once
second connect succeeds
original -c -n request is preserved
```

- [ ] **Step 3: Verify RED**

```powershell
cargo test -p neomacs-bin --test neomacsclient_cli windows_default
cargo test -p neomacs-bin --test neomacsclient_cli alternate_editor_empty
```

Expected: FAIL with the current unsupported-local-socket and automatic-daemon-stub errors.

- [ ] **Step 4: Resolve platform server targets**

Selection order:

1. explicit `--server-file`;
2. `EMACS_SERVER_FILE`;
3. Unix local socket from `--socket-name`, `EMACS_SOCKET_NAME`, or `server`;
4. non-Unix TCP auth file using the same name.

Do not interpret an explicit path-like `--socket-name` as an auth-file name on Windows; return the targeted unsupported-socket error for that explicit request.

- [ ] **Step 5: Match Windows home policy**

Implement:

```rust
fn effective_home() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("APPDATA"))
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(existing_platform_home_fallback)
}

#[cfg(windows)]
fn existing_platform_home_fallback() -> Option<PathBuf> {
    Some(PathBuf::from(r"C:\"))
}

#[cfg(not(windows))]
fn existing_platform_home_fallback() -> Option<PathBuf> {
    None
}
```

Search the server file relative to:

```text
~/.emacs.d/server/NAME
~/.config/emacs/server/NAME
XDG_CONFIG_HOME/emacs/server/NAME
```

Keep absolute server-file behavior unchanged.

- [ ] **Step 6: Always request a GUI frame for `-c`**

For `create_frame && !tty`, send `-window-system` even when display environment variables are absent. Supply `"neomacs"` as the display identifier when no native display string exists:

```rust
fn effective_display(options: &Options) -> Option<String> {
    // explicit, WAYLAND_DISPLAY, DISPLAY, then "neomacs" for GUI frame requests
}
```

- [ ] **Step 7: Implement daemon executable discovery**

Check:

```text
NEOMACS
EMACS
current_exe sibling neomacs[.exe]
PATH neomacs[.exe]
```

Reject paths that do not exist before spawning.

- [ ] **Step 8: Implement `-a ""` reconnect**

Refactor connection into a function callable twice:

```rust
fn try_client(prog: &str, options: &Options) -> Result<(), ClientConnectError>;
```

On the first connect error and empty alternate:

1. derive `NAME` from explicit `--socket-name`/`--server-file`, then `EMACS_SOCKET_NAME`/`EMACS_SERVER_FILE`, otherwise `server`;
2. run `neomacs --daemon[=NAME]`;
3. if the daemon command fails, retry the original connection once before reporting the command failure, because another client may have won the server-start race;
4. resolve the server target again;
5. retry once.

Do not recursively invoke fallback on the second failure.

- [ ] **Step 9: Reject terminal client frames explicitly**

Before connection or fallback, reject `-t`/`-nw` with a targeted error stating that terminal client frames are not implemented. Do not start or terminate a daemon for this request.

- [ ] **Step 10: Fix non-empty alternate shell**

Unix:

```rust
Command::new("sh").arg("-c")
```

Windows:

```rust
Command::new("cmd").arg("/C")
```

Preserve current argument forwarding.

- [ ] **Step 11: Run client tests**

```powershell
cargo test -p neomacs-bin --test neomacsclient_cli
```

Expected: PASS on Windows; Unix socket tests remain unchanged.

- [ ] **Step 12: Record a clean task checkpoint**

```powershell
git diff --check
git status --short
```

Expected: no whitespace errors. Do not commit yet.

---

### Task 8: Add Cross-Platform Daemon Lifecycle Coverage

**Files:**
- Modify: `neomacs-gui-tests/src/lib.rs` and existing process harness helpers.
- Modify: workflow test selection only if the new integration binary is not already included.
- Modify: help tests that currently expose unsupported daemon options.

**Interfaces:**
- Consumes: real `neomacs` and `neomacsclient` binaries.
- Produces: one deterministic end-to-end lifecycle test per platform.

- [ ] **Step 1: Write the failing foreground lifecycle test**

The test must:

```text
create isolated HOME/APPDATA
start neomacs --fg-daemon=integration
wait for socket/auth file
run neomacsclient against integration and assert (daemonp) == "integration"
run neomacsclient -c -n
delete the GUI frame through Lisp
assert daemon still answers
create another GUI frame
kill-emacs explicitly
assert daemon exits
```

- [ ] **Step 2: Write the failing empty-alternate end-to-end test**

With no server:

```text
neomacsclient -c -n -a ""
```

must return success, create the server endpoint, and leave a responsive daemon.

Run the same command a second time and assert it connects to the original daemon PID rather than starting another process.

- [ ] **Step 3: Verify RED before final integration changes**

Run the smallest platform-specific test filter. Expected: FAIL at the first missing lifecycle behavior, not due to harness setup.

- [ ] **Step 4: Add condition-based readiness helpers**

Poll endpoint existence and client responsiveness with a 30-second deadline. Do not use fixed sleeps. Capture daemon stdout/stderr/log path on timeout.

- [ ] **Step 5: Validate last-window and second-window behavior**

Assert the daemon PID stays alive after closing the first GUI frame and that the second frame has a new native window identity.

- [ ] **Step 6: Run targeted integration tests**

```powershell
cargo test -p neomacs-gui-tests daemon -- --nocapture
```

On Unix CI, run through the existing Xvfb/Wayland harness rather than adding a second display launcher.

- [ ] **Step 7: Run final validation**

```powershell
cargo fmt --all -- --check
cargo test -p neovm-core --lib daemon
cargo test -p neomacs-bin --lib daemon
cargo test -p neomacs-bin --test neomacsclient_cli
cargo test -p neomacs-display-runtime --lib deferred_primary
cargo check --workspace --locked
git diff --check
```

Run the new daemon GUI integration test on the current platform.

- [ ] **Step 8: Request final code review**

Review daemon state semantics, process handle/fd cleanup, renderer primary re-arming, Windows path policy, and fallback retry limits. Fix all Critical/Important findings.

- [ ] **Step 9: Commit all daemon/client work once**

```powershell
git add neovm-core neomacs-bin neomacs-display-runtime neomacs-gui-tests .github
git commit -m "feat(daemon): support persistent client frames"
```

- [ ] **Step 10: Confirm commit separation**

```powershell
git log --oneline 9bf40fd63..HEAD
```

Verify exactly one daemon/client commit is after and separate from:

```text
9bf40fd63 fix(display): honor display-table glyph faces
```

Do not push.
