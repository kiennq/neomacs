use super::*;

#[test]
fn test_portable_pty_explicit_cmd() {
    use std::io::{Read, Write};

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("create pty");
    #[cfg(windows)]
    let mut cmd = {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut cmd = CommandBuilder::new(shell);
        cmd.args(["/D", "/S", "/C", "echo PORTABLE_PTY_OK"]);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "echo PORTABLE_PTY_OK; sleep 1"]);
        cmd
    };
    let mut child = pair.slave.spawn_command(cmd).expect("spawn child");
    let mut reader = pair.master.try_clone_reader().expect("clone");
    let mut writer = pair.master.take_writer().expect("take writer");
    let (output_tx, output_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let result = match reader.read(&mut buf) {
                Ok(0) => Ok(Vec::new()),
                Ok(n) => Ok(buf[..n].to_vec()),
                Err(error) => Err(error.to_string()),
            };
            let done = result.as_ref().is_ok_and(Vec::is_empty) || result.is_err();
            if output_tx.send(result).is_err() || done {
                break;
            }
        }
    });

    const MARKER: &[u8] = b"PORTABLE_PTY_OK";
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut output = Vec::new();
    let mut failure = None;
    while !output.windows(MARKER.len()).any(|chunk| chunk == MARKER) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let chunk = match output_rx.recv_timeout(remaining) {
            Ok(Ok(chunk)) if chunk.is_empty() => {
                failure = Some("PTY reached EOF before emitting marker".to_owned());
                break;
            }
            Ok(Ok(chunk)) => chunk,
            Ok(Err(error)) => {
                failure = Some(format!("PTY read failed: {error}"));
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                failure = Some("PTY timed out before emitting marker".to_owned());
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                failure = Some("PTY reader stopped before emitting marker".to_owned());
                break;
            }
        };
        let scan_start = output.len().saturating_sub(3);
        output.extend_from_slice(&chunk);
        if output[scan_start..]
            .windows(4)
            .any(|chunk| chunk == b"\x1b[6n")
        {
            if let Err(error) = writer.write_all(b"\x1b[1;1R").and_then(|()| writer.flush()) {
                failure = Some(format!("failed to answer cursor position query: {error}"));
                break;
            }
        }
    }

    if let Some(failure) = failure {
        let _ = child.kill();
        let _ = child.wait();
        panic!("{failure}; output={:?}", String::from_utf8_lossy(&output));
    }

    let _ = child.wait();
}

#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    // SAFETY: signal 0 performs existence/permission checking only.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(target_os = "linux")]
fn reader_thread_exists(name: &str) -> bool {
    std::fs::read_dir("/proc/self/task")
        .expect("read process task directory")
        .filter_map(Result::ok)
        .any(|task| {
            std::fs::read_to_string(task.path().join("comm")).is_ok_and(|comm| comm.trim() == name)
        })
}

/// Naming a thread is the thread's own first act, so the `comm` entry appears
/// some time after `spawn` returns -- on a loaded machine, long after.  Waiting
/// for the precondition keeps the test measuring what it is about (destroy
/// reaps) instead of how promptly the scheduler ran a new thread.  Only the
/// precondition waits: the post-destroy check stays instantaneous, because
/// `destroy` joins the thread and a joined thread is gone.
#[cfg(target_os = "linux")]
fn wait_for_reader_thread(name: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if reader_thread_exists(name) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    reader_thread_exists(name)
}

#[test]
#[cfg(target_os = "linux")]
fn destroying_terminal_reaps_child_and_joins_reader_thread() {
    let id = TerminalId::new(77).expect("nonzero terminal id");
    let size = TerminalGridSize::new(20, 5).expect("positive terminal grid");
    let view = TerminalView::new(
        id,
        size,
        TerminalDisplayTarget::Window {
            buffer: neovm_core::buffer::BufferId(9),
        },
        Some("/bin/sh"),
    )
    .expect("create real PTY shell");
    let pid = view.child_process_id().expect("shell process id");
    let thread_name = format!("neo-term-{id}-pty");
    let mut manager = TerminalManager::new();
    manager.terminals.insert(id, view);

    assert!(process_exists(pid));
    assert!(
        wait_for_reader_thread(&thread_name),
        "reader thread {thread_name} never started"
    );
    assert!(manager.destroy(id).expect("destroy terminal"));

    let process_leaked = process_exists(pid);
    let thread_leaked = reader_thread_exists(&thread_name);
    if process_leaked {
        // Leave no child behind when this regression intentionally fails.
        // SAFETY: PID came from the child spawned immediately above.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }

    assert!(!process_leaked, "destroy left PTY child {pid} alive");
    assert!(
        !thread_leaked,
        "destroy left reader thread {thread_name} alive"
    );
}
