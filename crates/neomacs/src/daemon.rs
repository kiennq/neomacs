use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Child;
#[cfg(windows)]
use std::process::Child;
#[cfg(unix)]
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use neovm_core::emacs_core::daemon::DaemonRequest;

use super::StartupOptions;

#[derive(Debug, PartialEq, Eq)]
pub enum DaemonLaunch {
    Continue(StartupOptions),
    ParentExit(i32),
}

const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForegroundChildCommand {
    pub(crate) executable: PathBuf,
    pub(crate) args: Vec<std::ffi::OsString>,
}

#[cfg(test)]
pub(crate) fn foreground_child_command(
    executable: &Path,
    request: &DaemonRequest,
) -> ForegroundChildCommand {
    let name = match request {
        DaemonRequest::Background { name } | DaemonRequest::Foreground { name } => name.as_deref(),
    };
    let argument = name
        .map(|name| format!("--fg-daemon={name}").into())
        .unwrap_or_else(|| "--fg-daemon".into());
    ForegroundChildCommand {
        executable: executable.to_path_buf(),
        args: vec![argument],
    }
}

fn foreground_child_command_with_raw_args(
    executable: &Path,
    raw_args: &[std::ffi::OsString],
    request: &DaemonRequest,
) -> ForegroundChildCommand {
    let name = match request {
        DaemonRequest::Background { name } | DaemonRequest::Foreground { name } => name.as_deref(),
    };
    let foreground_argument = name
        .map(|name| format!("--fg-daemon={name}"))
        .unwrap_or_else(|| "--fg-daemon".to_string());
    let mut args = Vec::with_capacity(raw_args.len().max(2));
    let mut replaced = false;
    let mut index = 1;
    let mut options_enabled = true;

    while let Some(raw_arg) = raw_args.get(index) {
        if options_enabled {
            if raw_arg == "--" {
                options_enabled = false;
            } else {
                if let Some(chdir_arity) = chdir_argument_arity(raw_arg) {
                    if chdir_arity == 1 || raw_args.get(index + 1).is_some() {
                        index += chdir_arity;
                        continue;
                    }
                }
                let is_background_daemon = raw_arg
                    .to_str()
                    .map(is_background_daemon_argument)
                    .unwrap_or(false);
                if !replaced && is_background_daemon {
                    args.push(foreground_argument.clone().into());
                    replaced = true;
                    index += 1;
                    continue;
                }
            }
        }
        args.push(raw_arg.clone());
        index += 1;
    }

    if !replaced {
        args.insert(0, foreground_argument.into());
    }

    ForegroundChildCommand {
        executable: executable.to_path_buf(),
        args,
    }
}

fn is_background_daemon_argument(argument: &str) -> bool {
    matches!(
        argument,
        "-daemon" | "--daemon" | "-bg-daemon" | "--bg-daemon"
    ) || argument.starts_with("-daemon=")
        || argument.starts_with("--daemon=")
        || argument.starts_with("-bg-daemon=")
        || argument.starts_with("--bg-daemon=")
}

fn chdir_argument_arity(argument: &std::ffi::OsString) -> Option<usize> {
    let argument = argument.to_str()?;
    if argument == "-chdir" {
        return Some(2);
    }

    let equals = argument.find('=');
    let prefix = &argument[..equals.unwrap_or(argument.len())];
    if prefix.len() >= 4 && "--chdir".starts_with(prefix) {
        Some(if equals.is_some() { 1 } else { 2 })
    } else {
        None
    }
}

#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessObservation {
    Pending,
    Ready,
    Eof,
}

#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildObservation {
    Running,
    Exited,
}

#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    Pending,
    Ready,
    ChildExited,
    ReadinessEof,
    TimedOut,
}

#[cfg(any(unix, test))]
struct WaitState {
    timeout: Duration,
}

#[cfg(any(unix, test))]
impl WaitState {
    fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    fn observe(
        &self,
        elapsed: Duration,
        readiness: ReadinessObservation,
        child: ChildObservation,
    ) -> WaitOutcome {
        if readiness == ReadinessObservation::Ready {
            return WaitOutcome::Ready;
        }
        if readiness == ReadinessObservation::Eof {
            return WaitOutcome::ReadinessEof;
        }
        if child == ChildObservation::Exited {
            return WaitOutcome::ChildExited;
        }
        if elapsed >= self.timeout {
            return WaitOutcome::TimedOut;
        }
        WaitOutcome::Pending
    }
}

pub fn prepare(startup: StartupOptions) -> Result<DaemonLaunch, String> {
    match startup.daemon.as_ref() {
        None | Some(DaemonRequest::Foreground { .. }) => Ok(DaemonLaunch::Continue(startup)),
        Some(DaemonRequest::Background { .. }) => prepare_background(startup),
    }
}

#[cfg(unix)]
fn prepare_background(startup: StartupOptions) -> Result<DaemonLaunch, String> {
    use std::io;
    use std::os::unix::process::CommandExt;

    let executable = std::env::current_exe()
        .map_err(|error| format!("neomacs: cannot locate current executable: {error}"))?;
    let request = startup
        .daemon
        .as_ref()
        .expect("background daemon request checked by prepare");
    let command = foreground_child_command_with_raw_args(&executable, &startup.raw_args, request);

    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(format!(
            "neomacs: cannot create daemon readiness pipe: {}",
            io::Error::last_os_error()
        ));
    }
    let read_fd = fds[0];
    let write_fd = fds[1];

    if let Err(error) = configure_readiness_pipe_inheritance(read_fd, write_fd) {
        close_fd(read_fd);
        close_fd(write_fd);
        return Err(format!(
            "neomacs: cannot prepare daemon readiness pipe: {error}"
        ));
    }
    if let Err(error) = set_fd_nonblocking(read_fd) {
        close_fd(read_fd);
        close_fd(write_fd);
        return Err(format!(
            "neomacs: cannot prepare daemon readiness reader: {error}"
        ));
    }

    let mut child_command = Command::new(&command.executable);
    child_command
        .args(&command.args)
        .env("NEOMACS_DAEMON_READY_FD", write_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // `setsid` is the only child-side work before exec. In particular, this
    // path never returns from a bare fork and continues as the parent.
    unsafe {
        child_command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    let mut child = match child_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            close_fd(read_fd);
            close_fd(write_fd);
            return Err(format!("neomacs: cannot launch daemon child: {error}"));
        }
    };
    // The child owns the inherited write descriptor. The parent must close its
    // copy so EOF remains meaningful if exec or startup fails.
    close_fd(write_fd);

    let wait_result = wait_for_unix_readiness(&mut child, read_fd);
    close_fd(read_fd);

    match wait_result {
        Ok(WaitOutcome::Ready) => Ok(DaemonLaunch::ParentExit(0)),
        Ok(outcome) => {
            let detail = match outcome {
                WaitOutcome::ChildExited => format!(
                    "daemon child exited before readiness ({})",
                    child_status(&mut child)
                ),
                WaitOutcome::ReadinessEof => {
                    "daemon child closed readiness pipe before signaling readiness".to_string()
                }
                WaitOutcome::TimedOut => {
                    "timed out after 30 seconds waiting for daemon readiness".to_string()
                }
                WaitOutcome::Pending | WaitOutcome::Ready => unreachable!(),
            };
            terminate_child(&mut child);
            Err(format!("neomacs: {detail}"))
        }
        Err(error) => {
            terminate_child(&mut child);
            Err(format!("neomacs: daemon readiness wait failed: {error}"))
        }
    }
}

#[cfg(unix)]
fn wait_for_unix_readiness(
    child: &mut Child,
    read_fd: libc::c_int,
) -> std::io::Result<WaitOutcome> {
    let state = WaitState::new(DAEMON_STARTUP_TIMEOUT);
    let started = Instant::now();

    loop {
        let readiness = read_readiness(read_fd)?;
        let child_observation = match child.try_wait()? {
            Some(_) => ChildObservation::Exited,
            None => ChildObservation::Running,
        };
        match state.observe(started.elapsed(), readiness, child_observation) {
            WaitOutcome::Pending => {
                let remaining = DAEMON_STARTUP_TIMEOUT.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    continue;
                }
                let timeout = remaining.min(CHILD_POLL_INTERVAL);
                let timeout_ms = timeout.as_millis() as libc::c_int;
                if timeout_ms == 0 {
                    continue;
                }
                let mut poll_fd = libc::pollfd {
                    fd: read_fd,
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                loop {
                    let polled = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
                    if polled >= 0 {
                        break;
                    }
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(error);
                }
            }
            outcome => return Ok(outcome),
        }
    }
}

#[cfg(unix)]
fn read_readiness(read_fd: libc::c_int) -> std::io::Result<ReadinessObservation> {
    let mut byte = [0u8; 1];
    loop {
        let read = unsafe { libc::read(read_fd, byte.as_mut_ptr().cast(), byte.len()) };
        if read > 0 {
            return Ok(ReadinessObservation::Ready);
        }
        if read == 0 {
            return Ok(ReadinessObservation::Eof);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if matches!(
            error.raw_os_error(),
            Some(libc::EAGAIN) | Some(libc::EWOULDBLOCK)
        ) {
            return Ok(ReadinessObservation::Pending);
        }
        return Err(error);
    }
}

#[cfg(unix)]
fn set_fd_inheritable(fd: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn set_fd_cloexec(fd: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn configure_readiness_pipe_inheritance(
    read_fd: libc::c_int,
    write_fd: libc::c_int,
) -> std::io::Result<()> {
    set_fd_cloexec(read_fd)
        .and_then(|_| set_fd_cloexec(write_fd))
        .and_then(|_| set_fd_inheritable(write_fd))
}

#[cfg(unix)]
fn set_fd_nonblocking(fd: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn close_fd(fd: libc::c_int) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(unix)]
fn child_status(child: &mut Child) -> String {
    match child.try_wait() {
        Ok(Some(status)) => status
            .code()
            .map(|code| format!("exit code {code}"))
            .unwrap_or_else(|| "terminated by signal".to_string()),
        Ok(None) => "still running".to_string(),
        Err(error) => format!("status unavailable: {error}"),
    }
}

#[cfg(unix)]
fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
fn prepare_background(startup: StartupOptions) -> Result<DaemonLaunch, String> {
    use std::os::windows::process::CommandExt;

    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CreateEventW, DETACHED_PROCESS, WaitForMultipleObjects,
    };

    let executable = std::env::current_exe()
        .map_err(|error| format!("neomacs: cannot locate current executable: {error}"))?;
    let request = startup
        .daemon
        .as_ref()
        .expect("background daemon request checked by prepare");
    let command = foreground_child_command_with_raw_args(&executable, &startup.raw_args, request);
    let event_name = unique_event_name();
    let mut wide_name: Vec<u16> = event_name.encode_utf16().collect();
    wide_name.push(0);

    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide_name.as_ptr()) };
    if event.is_null() {
        return Err(format!(
            "neomacs: cannot create daemon readiness event: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut child_command = std::process::Command::new(&command.executable);
    child_command
        .args(&command.args)
        .env("NEOMACS_DAEMON_READY_EVENT", &event_name)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match child_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            unsafe {
                CloseHandle(event);
            }
            return Err(format!("neomacs: cannot launch daemon child: {error}"));
        }
    };

    let handles = [event, {
        use std::os::windows::io::AsRawHandle;
        child.as_raw_handle()
    }];
    let wait = unsafe {
        WaitForMultipleObjects(
            handles.len() as u32,
            handles.as_ptr(),
            0,
            DAEMON_STARTUP_TIMEOUT.as_millis() as u32,
        )
    };
    unsafe {
        CloseHandle(event);
    }

    match wait {
        WAIT_OBJECT_0 => Ok(DaemonLaunch::ParentExit(0)),
        result if result == WAIT_OBJECT_0 + 1 => {
            let detail = match child.wait() {
                Ok(status) => status
                    .code()
                    .map(|code| format!("exit code {code}"))
                    .unwrap_or_else(|| "terminated by signal".to_string()),
                Err(error) => format!("status unavailable: {error}"),
            };
            terminate_child(&mut child);
            Err(format!(
                "neomacs: daemon child exited before readiness ({detail})"
            ))
        }
        WAIT_TIMEOUT => {
            terminate_child(&mut child);
            Err("neomacs: timed out after 30 seconds waiting for daemon readiness".to_string())
        }
        WAIT_FAILED => {
            let error = std::io::Error::last_os_error();
            terminate_child(&mut child);
            Err(format!("neomacs: daemon readiness wait failed: {error}"))
        }
        _ => {
            terminate_child(&mut child);
            Err(format!(
                "neomacs: daemon readiness wait returned unexpected result {wait:#x}"
            ))
        }
    }
}

#[cfg(windows)]
fn unique_event_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(r"Local\neomacs-daemon-{}-{sequence}", std::process::id())
}

#[cfg(windows)]
fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(any(unix, windows)))]
fn prepare_background(_startup: StartupOptions) -> Result<DaemonLaunch, String> {
    Err("neomacs: background daemon launch is unsupported on this platform".to_string())
}

#[cfg(test)]
mod daemon_launch_tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

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

    #[test]
    fn background_reexec_preserves_original_arguments_and_replaces_daemon_flag() {
        let named = foreground_child_command_with_raw_args(
            Path::new("neomacs"),
            &[
                "neomacs".into(),
                "-Q".into(),
                "--dump-file".into(),
                "custom.pdump".into(),
                "--daemon=work".into(),
                "--eval".into(),
                "(setq daemon-test t)".into(),
                "init.el".into(),
            ],
            &DaemonRequest::Background {
                name: Some("work".into()),
            },
        );
        assert_eq!(
            named
                .args
                .iter()
                .map(|arg| arg.to_str().expect("test argument is UTF-8"))
                .collect::<Vec<_>>(),
            vec![
                "-Q",
                "--dump-file",
                "custom.pdump",
                "--fg-daemon=work",
                "--eval",
                "(setq daemon-test t)",
                "init.el",
            ]
        );

        let default = foreground_child_command_with_raw_args(
            Path::new("neomacs"),
            &[
                "neomacs".into(),
                "--load".into(),
                "early-init.el".into(),
                "-bg-daemon".into(),
                "--funcall".into(),
                "server-start".into(),
                "README".into(),
            ],
            &DaemonRequest::Background { name: None },
        );
        assert_eq!(
            default
                .args
                .iter()
                .map(|arg| arg.to_str().expect("test argument is UTF-8"))
                .collect::<Vec<_>>(),
            vec![
                "--load",
                "early-init.el",
                "--fg-daemon",
                "--funcall",
                "server-start",
                "README",
            ]
        );
    }

    #[test]
    fn background_reexec_removes_parent_applied_chdir_options() {
        let old_spelling = foreground_child_command_with_raw_args(
            Path::new("neomacs"),
            &[
                "neomacs".into(),
                "-chdir".into(),
                "relative/dir".into(),
                "--daemon".into(),
                "-Q".into(),
                "--eval".into(),
                "(princ 1)".into(),
            ],
            &DaemonRequest::Background { name: None },
        );
        assert_eq!(
            old_spelling
                .args
                .iter()
                .map(|arg| arg.to_str().expect("test argument is UTF-8"))
                .collect::<Vec<_>>(),
            vec!["--fg-daemon", "-Q", "--eval", "(princ 1)"]
        );

        let long_spelling = foreground_child_command_with_raw_args(
            Path::new("neomacs"),
            &[
                "neomacs".into(),
                "--chdir".into(),
                "relative/dir".into(),
                "--bg-daemon=work".into(),
                "--load".into(),
                "init.el".into(),
            ],
            &DaemonRequest::Background {
                name: Some("work".into()),
            },
        );
        assert_eq!(
            long_spelling
                .args
                .iter()
                .map(|arg| arg.to_str().expect("test argument is UTF-8"))
                .collect::<Vec<_>>(),
            vec!["--fg-daemon=work", "--load", "init.el"]
        );

        let after_terminator = foreground_child_command_with_raw_args(
            Path::new("neomacs"),
            &[
                "neomacs".into(),
                "--daemon".into(),
                "--".into(),
                "--chdir".into(),
                "lisp-argument".into(),
            ],
            &DaemonRequest::Background { name: None },
        );
        assert_eq!(
            after_terminator
                .args
                .iter()
                .map(|arg| arg.to_str().expect("test argument is UTF-8"))
                .collect::<Vec<_>>(),
            vec!["--fg-daemon", "--", "--chdir", "lisp-argument"]
        );
    }

    #[test]
    fn wait_state_reports_ready_before_deadline() {
        let mut reader = FakeReadinessReader::new([ReadinessObservation::Ready]);
        let state = WaitState::new(Duration::from_secs(30));

        assert_eq!(
            state.observe(
                Duration::from_secs(1),
                reader.next(),
                ChildObservation::Running
            ),
            WaitOutcome::Ready
        );
    }

    #[test]
    fn wait_state_reports_child_exit_without_readiness() {
        let mut reader = FakeReadinessReader::new([ReadinessObservation::Pending]);
        let state = WaitState::new(Duration::from_secs(30));

        assert_eq!(
            state.observe(
                Duration::from_secs(1),
                reader.next(),
                ChildObservation::Exited
            ),
            WaitOutcome::ChildExited
        );
    }

    #[test]
    fn wait_state_times_out_at_exact_deadline() {
        let mut reader = FakeReadinessReader::new([ReadinessObservation::Pending]);
        let state = WaitState::new(Duration::from_secs(30));

        assert_eq!(
            state.observe(
                Duration::from_secs(30),
                reader.next(),
                ChildObservation::Running
            ),
            WaitOutcome::TimedOut
        );
    }

    #[test]
    fn wait_state_reports_readiness_eof_as_failure() {
        let mut reader = FakeReadinessReader::new([ReadinessObservation::Eof]);
        let state = WaitState::new(Duration::from_secs(30));

        assert_eq!(
            state.observe(
                Duration::from_secs(1),
                reader.next(),
                ChildObservation::Running
            ),
            WaitOutcome::ReadinessEof
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_readiness_pipe_is_inheritable_and_reports_byte_then_eof() {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_fd = fds[0];
        let write_fd = fds[1];

        configure_readiness_pipe_inheritance(read_fd, write_fd)
            .expect("readiness fd inheritance should be configured");
        let read_flags = unsafe { libc::fcntl(read_fd, libc::F_GETFD) };
        let write_flags = unsafe { libc::fcntl(write_fd, libc::F_GETFD) };
        assert!(read_flags >= 0);
        assert!(write_flags >= 0);
        assert_ne!(read_flags & libc::FD_CLOEXEC, 0);
        assert_ne!(write_flags & libc::FD_CLOEXEC, 0);

        set_fd_inheritable(write_fd).expect("child readiness fd should be inheritable");
        let read_flags_after = unsafe { libc::fcntl(read_fd, libc::F_GETFD) };
        let write_flags_after = unsafe { libc::fcntl(write_fd, libc::F_GETFD) };
        assert!(read_flags_after >= 0);
        assert!(write_flags_after >= 0);
        assert_ne!(read_flags_after & libc::FD_CLOEXEC, 0);
        assert_eq!(write_flags_after & libc::FD_CLOEXEC, 0);
        set_fd_nonblocking(read_fd).expect("parent readiness fd should be nonblocking");

        let byte = [1u8];
        assert_eq!(
            unsafe { libc::write(write_fd, byte.as_ptr().cast(), byte.len()) },
            1
        );
        assert_eq!(
            read_readiness(read_fd).expect("readiness byte"),
            ReadinessObservation::Ready
        );

        close_fd(write_fd);
        assert_eq!(
            read_readiness(read_fd).expect("readiness EOF"),
            ReadinessObservation::Eof
        );
        close_fd(read_fd);
    }

    #[cfg(windows)]
    #[test]
    fn windows_readiness_event_is_manual_reset_and_unique() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            CreateEventW, ResetEvent, SetEvent, WaitForSingleObject,
        };

        let name = unique_event_name();
        let mut wide_name: Vec<u16> = name.encode_utf16().collect();
        wide_name.push(0);
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide_name.as_ptr()) };
        assert!(!event.is_null());
        assert_ne!(unique_event_name(), name);

        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_TIMEOUT);
        assert_ne!(unsafe { SetEvent(event) }, 0);
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(
            unsafe { WaitForSingleObject(event, 0) },
            WAIT_OBJECT_0,
            "manual-reset event remains signaled"
        );
        assert_ne!(unsafe { ResetEvent(event) }, 0);
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_TIMEOUT);
        unsafe {
            CloseHandle(event);
        }
    }

    struct FakeReadinessReader {
        observations: std::collections::VecDeque<ReadinessObservation>,
    }

    impl FakeReadinessReader {
        fn new(observations: impl IntoIterator<Item = ReadinessObservation>) -> Self {
            Self {
                observations: observations.into_iter().collect(),
            }
        }

        fn next(&mut self) -> ReadinessObservation {
            self.observations
                .pop_front()
                .unwrap_or(ReadinessObservation::Pending)
        }
    }
}
