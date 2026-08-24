# Neomacs Daemon Design

## Goal

Implement cross-platform daemon support sufficient for:

```text
neomacsclient -c -n -a ""
```

The command must connect to an existing Neomacs server or start a background
Neomacs daemon, wait until its server is ready, reconnect, and create a GUI
client frame.

The daemon work is separate from commit `9bf40fd63`, which fixes whitespace
display-table glyph faces. Daemon and client changes will land as one later
commit and will not be pushed automatically.

## Scope

Supported:

- `--daemon`, `--bg-daemon[=NAME]`, and `--fg-daemon[=NAME]`.
- GNU-compatible `daemonp` and `daemon-initialized` state.
- Unix and Windows local-socket servers.
- Explicit TCP auth-file servers on every platform.
- Graphical `neomacsclient -c` frames.
- Closing every GUI client frame without terminating the daemon.
- Creating another GUI client frame after all prior GUI frames were closed.
- Empty alternate editor (`-a ""`) daemon startup and reconnect.

Deferred:

- `neomacsclient -t`, `-nw`, and terminal client frames. Neomacs does not yet
  implement the required `server-create-tty-frame`/`make-frame-on-tty` path.
- Restarting a daemon through `restart-emacs`.
- A renderer redesign that removes the primary-window concept entirely.

## Rejected Approaches

### Hidden normal editor

Launching an ordinary hidden editor would avoid daemon state changes, but its
window lifecycle would remain authoritative. Closing the last client frame
could still terminate the process, and `daemonp`/startup semantics would be
wrong.

### Separate daemon supervisor

A separate process supervising GUI editor children would duplicate editor
lifecycle and server ownership. It is larger than fixing the existing
single-process architecture and diverges from GNU Emacs.

### Windows-only local-socket dependency

Adding a Windows-only Unix-domain-socket crate would duplicate functionality
already provided by the existing `socket2` dependency and leave separate Unix
and Windows transport implementations to drift.

### Windows-only transport facade

Keeping the current Unix implementation and adding an unrelated Windows path
would duplicate bind, connect, accept, polling, and I/O behavior. Local stream
and listener sockets instead use one `socket2`-backed representation on every
supported platform.

## Architecture

### Daemon state

Add process-global daemon state in `neovm-core`, following existing
process-global runtime configuration patterns.

The state records:

- background or foreground daemon mode;
- optional server name;
- whether Lisp startup called `daemon-initialized`.

`daemonp` returns:

- `nil` outside daemon mode;
- `t` for the default daemon;
- the daemon name string for a named daemon.

`daemon-initialized` rejects non-daemon and duplicate calls, then marks the
daemon ready. `lisp/startup.el` remains responsible for starting the server
before this call.

### Command-line parsing

Restore the GNU daemon rows in `neomacs-bin/src/args.rs` with their GNU
priority and arity. Consume daemon options in native startup parsing instead
of forwarding them to Lisp.

Reject incompatible combinations such as daemon mode with batch, script, or
an initial terminal frontend.

Background daemon startup must occur before logging initialization, event-loop
creation, thread creation, or GPU initialization.

### Background process creation

`--fg-daemon` runs the daemon in the current process.

`--daemon` and `--bg-daemon` start a clean foreground-daemon child:

- Unix uses spawn/exec semantics and a detached session. It never continues
  execution after a bare fork because winit, AppKit, and wgpu are not
  fork-safe.
- Windows starts a detached child without inheriting the console.

The parent waits for daemon readiness or child failure. Readiness is tied to
successful server startup, not an arbitrary sleep. Startup has a bounded
timeout and reports the child/server error rather than returning success.

On Unix, the background parent passes a readiness-pipe descriptor to the
foreground child. `daemon-initialized` writes one success byte after
`server-start`; EOF before that byte is failure.

On Windows, the parent creates a uniquely named event and passes its name to
the child. `daemon-initialized` signals the event after `server-start`.

Both parents wait at most 30 seconds, also monitoring whether the child exits.

### Startup frame topology

A daemon starts with exactly one selected terminal frame:

- `visible` is true, matching GNU daemon behavior and preventing last-frame
  deletion from killing the daemon;
- it has no window system;
- `terminal-frame` points to it;
- `window-system` and `initial-window-system` are nil;
- no native GUI window is published;
- `frame-initial-frame` and `default-minibuffer-frame` are nil.

The frame is not rendered because native-window traversal already excludes
frames without a window system.

### Deferred renderer primary window

Daemon startup creates the event loop but defers native primary-window and
surface creation.

The first GUI client-frame realization:

1. creates the native window;
2. initializes or reuses the GPU device;
3. maps the native window to the new Emacs frame;
4. publishes and renders that frame.

When the daemon's last GUI window closes:

1. destroy only that window and surface;
2. clear the primary native/Emacs mapping;
3. retain the process and reusable GPU state;
4. select the non-window-system terminal frame;
5. re-arm deferred primary creation.

The next GUI client frame follows the same first-window path. Device-loss
recovery with no live window is deferred until the next window realization.
Only an explicit shutdown command exits the daemon event loop.

### Server transport

Local stream and listener sockets use `socket2::Socket` on every supported
platform. Unix-domain datagram and seqpacket support remains Unix-only because
Windows AF_UNIX supports stream sockets only.

Windows local sockets require Windows 10 version 1803 or Windows Server 2019
or newer. Capability is determined by a cached runtime `AF_UNIX` stream-socket
probe rather than by the Rust target alone. When the probe succeeds,
`make-network-process` advertises `(:family local)` on Windows, so the existing
`server.el` policy leaves `server-use-tcp` nil and defaults to the named local
socket. If the running Windows kernel does not support AF_UNIX, Neomacs does
not advertise the capability and `server.el` falls back to TCP.

One shared native path-policy helper supplies both the editor and
`neomacsclient`. On Windows it first honors `NEOMACS_SERVER_SOCKET_DIR`, then
chooses the shorter valid candidate from `%TEMP%\neomacs-server` and
`%LOCALAPPDATA%\neomacs\server`. Native startup exports the selected directory
for `server.el`, which reads it before its Unix-oriented
`XDG_RUNTIME_DIR`/`TMPDIR` fallback. Neomacs creates the directory on NTFS and
restricts it to the current user with an explicit Windows ACL.

Socket paths are UTF-8 and must fit in the 108-byte
`sockaddr_un.sun_path`; if no default candidate fits, or an override is
overlong or non-UTF-8, startup fails with a targeted error rather than silently
selecting a different endpoint. Stale socket reparse points are removed only
after confirming that no live server owns the endpoint.

`neomacsclient` uses the same socket-directory and path-resolution policy as
`server.el`. Its default target and `--socket-name` connect through AF_UNIX on
supported Windows systems. Explicit `--server-file` and `EMACS_SERVER_FILE`
continue to select TCP authentication files on every platform.

AF_UNIX is a transport choice, not a stronger authentication primitive on
Windows: Windows exposes no stable peer-credential API equivalent to
`SO_PEERCRED`. Security therefore depends on the owner-only socket-directory
ACL, matching the filesystem-access trust model used for Unix local sockets.

### Client startup and retry

If the first connection fails and `--alternate-editor` is empty:

1. locate Neomacs through `NEOMACS`, then `EMACS`, then the executable beside
   `neomacsclient`, then `PATH`;
2. launch `neomacs --daemon` with the selected server name;
3. wait for the daemon command to report readiness;
4. resolve the socket/auth file again;
5. retry the original request once.

Non-empty alternate-editor commands keep their current behavior, using the
platform shell (`sh -c` on Unix and `cmd /C` on Windows).

GUI frame requests always carry the window-system request. Where the platform
has no `DISPLAY`/`WAYLAND_DISPLAY`, the client supplies a Neomacs display
identifier accepted by the `neo` backend instead of silently omitting GUI
frame creation.

## Error Handling

- Invalid daemon option combinations fail before process detachment.
- Child startup failure is propagated to the parent/client.
- Readiness waits have a fixed timeout and terminate only the child process
  started by that request.
- Missing or malformed TCP auth files remain explicit errors.
- Unsupported Windows AF_UNIX kernels fall back to TCP through capability
  advertisement rather than failing after `server.el` selected local sockets.
- Invalid UTF-8 or overlong Windows local-socket paths return targeted errors.
- Stale Windows socket reparse points are removed without deleting a live
  server endpoint.
- An existing server wins races: if another process creates it while startup
  is attempted, the client reconnects instead of starting a second server.
- Unsupported terminal client-frame requests return a targeted error without
  terminating the daemon.

## Testing

### Unit and contract tests

- GNU priority/order and parsing for all daemon options.
- Daemon state transitions and Lisp-visible return values.
- Local-stream capability advertised on Unix and on Windows when the runtime
  AF_UNIX probe succeeds.
- Windows local bind, listen, accept, connect, read, write, and shutdown.
- Windows capability fallback when AF_UNIX is unavailable.
- Windows socket path UTF-8 and 108-byte limit validation.
- Windows stale socket cleanup and owner-only directory ACL behavior.
- Windows auth-file path resolution with `HOME`, `APPDATA`, and `USERPROFILE`.
- Client executable discovery and platform shell selection.
- Empty alternate-editor retry state machine, timeout, and child failure.

### Startup and renderer tests

- Daemon startup has one visible selected non-window-system terminal frame.
- No native window is created before a client frame request.
- First GUI client frame creates and maps the deferred primary.
- Closing the last GUI client frame leaves the daemon alive and clears stale
  mappings.
- A later GUI client frame recreates the primary successfully.
- Device-loss recovery waits when no window exists and resumes on creation.

### End-to-end tests

On each supported CI platform:

1. start a named foreground daemon;
2. wait for its local socket;
3. query `(daemonp)` through `neomacsclient`;
4. create a no-wait GUI frame;
5. close it and verify the daemon remains responsive;
6. create a second GUI frame;
7. terminate the daemon explicitly.

A separate client test invokes `neomacsclient -c -n -a ""` with no running
server and verifies successful daemon startup and connection.

## Acceptance Criteria

- The reported Windows command no longer returns the unsupported local-socket
  error.
- On supported Windows versions,
  `(featurep 'make-network-process '(:family local))` returns non-nil and
  `server-use-tcp` remains nil by default.
- With no server, `neomacsclient -c -n -a ""` starts a daemon and creates a
  GUI frame.
- With an existing server, the same command connects without starting another
  process.
- Closing all GUI frames does not end the daemon.
- A new GUI frame can be created afterward.
- Unix and Windows local sockets work through the default client path.
- Explicit Windows TCP auth files continue to work through `--server-file`.
- Existing non-daemon startup behavior and explicit `--server-file` behavior
  remain unchanged.
