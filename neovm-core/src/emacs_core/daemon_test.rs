#[cfg(unix)]
use super::daemon::signal_readiness_fd;
use super::daemon::{
    DaemonRequest, DaemonStateError, configure, daemon_value, is_daemon, is_initialized,
    mark_initialized, reset_for_tests,
};
use super::{Context, Value, format_eval_result};
use std::sync::{Mutex, MutexGuard, OnceLock};

struct TestGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        reset_for_tests();
    }
}

fn test_guard() -> TestGuard {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    reset_for_tests();
    TestGuard { _lock: guard }
}

#[test]
fn daemon_state_reports_default_named_and_unnamed_modes() {
    let _guard = test_guard();

    assert_eq!(daemon_value(), Value::NIL);
    assert!(!is_daemon());
    assert!(!is_initialized());
    assert_eq!(mark_initialized(), Err(DaemonStateError::NotDaemon));

    configure(Some(DaemonRequest::Background { name: None })).unwrap();
    assert_eq!(daemon_value(), Value::T);
    assert!(is_daemon());
    assert!(!is_initialized());

    configure(None).unwrap();
    assert_eq!(daemon_value(), Value::NIL);
    assert!(!is_daemon());

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

#[test]
fn daemon_builtins_expose_state_and_reject_non_daemon_initialization() {
    let _guard = test_guard();

    let mut ctx = Context::new();
    assert_eq!(ctx.eval_str("(daemonp)").unwrap(), Value::NIL);
    let error = format_eval_result(&ctx.eval_str("(daemon-initialized)"));
    assert_eq!(
        error,
        "ERR (error (\"This function can only be called if emacs is run as a daemon\"))"
    );

    configure(Some(DaemonRequest::Background { name: None })).unwrap();
    assert_eq!(ctx.eval_str("(daemonp)").unwrap(), Value::T);

    reset_for_tests();
    configure(Some(DaemonRequest::Foreground {
        name: Some("work".into()),
    }))
    .unwrap();
    assert_eq!(
        ctx.eval_str("(daemonp)").unwrap().as_utf8_str(),
        Some("work")
    );
}

#[test]
fn daemon_initialized_builtin_waits_for_after_init_time_and_rejects_duplicates() {
    let _guard = test_guard();

    configure(Some(DaemonRequest::Foreground { name: None })).unwrap();
    let mut ctx = Context::new();

    let premature = format_eval_result(&ctx.eval_str("(daemon-initialized)"));
    assert_eq!(
        premature,
        "ERR (error (\"This function can only be called after loading the init files\"))"
    );

    ctx.set_variable("after-init-time", Value::T);
    assert_eq!(ctx.eval_str("(daemon-initialized)").unwrap(), Value::NIL);

    ctx.set_variable("after-init-time", Value::NIL);
    let duplicate = format_eval_result(&ctx.eval_str("(daemon-initialized)"));
    assert_eq!(
        duplicate,
        "ERR (error (\"The daemon has already been initialized\"))"
    );
}

#[cfg(windows)]
#[test]
fn daemon_initialization_readiness_failure_does_not_commit_initialized_state() {
    let _guard = test_guard();

    let previous = std::env::var_os("NEOMACS_DAEMON_READY_EVENT");
    unsafe {
        std::env::set_var(
            "NEOMACS_DAEMON_READY_EVENT",
            format!(r"Local\neomacs-test-missing-event-{}", std::process::id()),
        );
    }

    configure(Some(DaemonRequest::Foreground { name: None })).unwrap();
    assert_eq!(
        mark_initialized(),
        Err(DaemonStateError::ReadinessSignalFailed)
    );
    assert!(!is_initialized());

    unsafe {
        std::env::remove_var("NEOMACS_DAEMON_READY_EVENT");
    }
    assert_eq!(mark_initialized(), Ok(()));
    assert!(is_initialized());

    unsafe {
        match previous {
            Some(value) => std::env::set_var("NEOMACS_DAEMON_READY_EVENT", value),
            None => std::env::remove_var("NEOMACS_DAEMON_READY_EVENT"),
        }
    }
}

#[cfg(unix)]
#[test]
fn daemon_initialization_signals_unix_readiness_fd() {
    let _guard = test_guard();

    let previous = std::env::var_os("NEOMACS_DAEMON_READY_FD");
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe {
        std::env::set_var("NEOMACS_DAEMON_READY_FD", fds[1].to_string());
    }

    configure(Some(DaemonRequest::Foreground { name: None })).unwrap();
    mark_initialized().unwrap();

    let mut byte = [0u8; 1];
    assert_eq!(
        unsafe { libc::read(fds[0], byte.as_mut_ptr().cast(), byte.len()) },
        1
    );
    assert_eq!(byte, [1]);
    assert_eq!(
        unsafe { libc::read(fds[0], byte.as_mut_ptr().cast(), byte.len()) },
        0
    );
    unsafe {
        libc::close(fds[0]);
        match previous {
            Some(value) => std::env::set_var("NEOMACS_DAEMON_READY_FD", value),
            None => std::env::remove_var("NEOMACS_DAEMON_READY_FD"),
        }
    }
}

#[cfg(unix)]
#[test]
fn daemon_readiness_failure_retains_fd_for_later_retry() {
    let _guard = test_guard();

    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);

    for &fd in &fds {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
    }

    let chunk = [0u8; 4096];
    loop {
        let written = unsafe { libc::write(fds[1], chunk.as_ptr().cast(), chunk.len()) };
        if written > 0 {
            continue;
        }
        assert_eq!(written, -1);
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        break;
    }

    assert_eq!(
        signal_readiness_fd(fds[1]),
        Err(DaemonStateError::ReadinessSignalFailed)
    );

    let mut drained = [0u8; 4096];
    loop {
        let read = unsafe { libc::read(fds[0], drained.as_mut_ptr().cast(), drained.len()) };
        if read > 0 {
            continue;
        }
        assert_eq!(read, -1);
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        break;
    }

    let mut unrelated = [0; 2];
    assert_eq!(unsafe { libc::pipe(unrelated.as_mut_ptr()) }, 0);
    let marker = [7u8];
    assert_eq!(
        unsafe { libc::write(unrelated[1], marker.as_ptr().cast(), marker.len()) },
        1
    );

    assert_eq!(signal_readiness_fd(fds[1]), Ok(()));

    let mut byte = [0u8; 1];
    assert_eq!(
        unsafe { libc::read(fds[0], byte.as_mut_ptr().cast(), byte.len()) },
        1
    );
    assert_eq!(byte, [1]);
    assert_eq!(
        unsafe { libc::read(fds[0], byte.as_mut_ptr().cast(), byte.len()) },
        0
    );
    assert_eq!(
        unsafe { libc::read(unrelated[0], byte.as_mut_ptr().cast(), byte.len()) },
        1
    );
    assert_eq!(byte, marker);

    unsafe {
        libc::close(fds[0]);
        libc::close(unrelated[0]);
        libc::close(unrelated[1]);
    }
}
