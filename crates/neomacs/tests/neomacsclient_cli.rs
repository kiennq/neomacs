use std::process::Command;

#[test]
fn neomacsclient_version_matches_emacs_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .arg("--version")
        .output()
        .expect("neomacsclient --version should run");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "neomacsclient 31.0.90\n"
    );
}

#[test]
fn neomacsclient_sends_gnu_server_request_over_local_socket() {
    use std::fs;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

    use neovm_core::local_socket::{accept_stream, bind_stream_listener, stream_supported};

    if !stream_supported() {
        return;
    }

    let repo_tmp = PathBuf::from(env!("CARGO_WORKSPACE_DIR")).join("tmp");
    fs::create_dir_all(&repo_tmp).expect("repo-local tmp dir");
    let dir = tempfile::Builder::new()
        .prefix("neomacsclient-cli-")
        .tempdir_in(repo_tmp)
        .expect("repo-local tempdir");
    let socket = dir.path().join("server");
    let listener = bind_stream_listener(&socket, 1).expect("bind local socket");

    let server = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("set local listener nonblocking");
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut stream, _) = loop {
            match accept_stream(&listener) {
                Ok(connection) => break connection,
                Err(error)
                    if (error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(10035))
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept local client: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set accepted local socket blocking");
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        stream
            .write_all(b"-print OK&&done\n")
            .expect("write response");
        String::from_utf8(request).expect("utf8 request")
    });

    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .arg("--socket-name")
        .arg(&socket)
        .arg("--no-wait")
        .arg("--eval")
        .arg("(message \"a b\")")
        .arg("--frame-parameters")
        .arg("((name . ignored-on-current-frame))")
        .output()
        .expect("neomacsclient should run");

    assert!(output.status.success(), "{output:?}");
    let request = server.join().expect("server thread should finish");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "OK&done");
    assert!(request.starts_with("-dir "));
    assert!(request.contains(" -nowait "));
    assert!(request.contains(" -current-frame "));
    assert!(!request.contains(" -frame-parameters "));
    assert!(request.contains(" -eval (message&_\"a&_b\") "));
    assert!(request.ends_with(" \n"));
}

#[cfg(unix)]
#[test]
fn neomacsclient_parent_id_implies_a_new_graphical_frame() {
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;

    let repo_tmp = PathBuf::from(env!("CARGO_WORKSPACE_DIR")).join("tmp");
    fs::create_dir_all(&repo_tmp).expect("repo-local tmp dir");
    let dir = tempfile::Builder::new()
        .prefix("neomacsclient-parent-frame-")
        .tempdir_in(repo_tmp)
        .expect("repo-local tempdir");
    let socket = dir.path().join("server");
    let listener = UnixListener::bind(&socket).expect("bind local socket");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        stream.write_all(b"-print PARENT_OK\n").expect("reply");
        String::from_utf8(request).expect("utf8 request")
    });

    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .arg("--socket-name")
        .arg(&socket)
        .arg("--parent-id")
        .arg("42")
        .env("DISPLAY", ":9")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("neomacsclient should run");

    assert!(output.status.success(), "{output:?}");
    let request = server.join().expect("server thread should finish");
    assert!(request.contains(" -parent-id 42 "));
    assert!(request.contains(" -window-system "));
    assert!(!request.contains(" -current-frame "));
}

#[cfg(unix)]
#[test]
fn neomacsclient_create_frame_uses_neomacs_display_fallback() {
    use std::fs;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

    use neovm_core::local_socket::{accept_stream, bind_stream_listener, stream_supported};

    if !stream_supported() {
        return;
    }

    let repo_tmp = PathBuf::from(env!("CARGO_WORKSPACE_DIR")).join("tmp");
    fs::create_dir_all(&repo_tmp).expect("repo-local tmp dir");
    let dir = tempfile::Builder::new()
        .prefix("neomacsclient-create-frame-")
        .tempdir_in(repo_tmp)
        .expect("repo-local tempdir");
    let socket = dir.path().join("server");
    let listener = bind_stream_listener(&socket, 1).expect("bind local socket");

    let server = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("set local listener nonblocking");
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut stream, _) = loop {
            match accept_stream(&listener) {
                Ok(connection) => break connection,
                Err(error)
                    if (error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(10035))
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept local client: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("set accepted local socket blocking");
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        stream
            .write_all(b"-print FRAME_OK\n")
            .expect("write response");
        String::from_utf8(request).expect("utf8 request")
    });

    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .arg("--socket-name")
        .arg(&socket)
        .arg("--create-frame")
        .arg("--no-wait")
        .arg("file.txt")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("neomacsclient should run");

    let request = server.join().expect("server thread should finish");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "FRAME_OK");
    // GNU emacsclient sends the creating client's environment before `-dir',
    // so a daemon-created frame observes DISPLAY and the rest of the client's
    // process environment (lib-src/emacsclient.c, "Send over our environment").
    assert!(request.starts_with("-env "));
    assert!(request.contains(" -dir "));
    assert!(request.contains(" -nowait "));
    // A fresh headless daemon has no frame display for server.el to inherit.
    assert!(request.contains(" -display neomacs "));
    assert!(request.contains(" -window-system "));
    assert!(!request.contains(" -current-frame "));
    assert!(request.contains(" -file file.txt "));
    assert!(request.ends_with(" \n"));
}

#[cfg(unix)]
#[test]
fn neomacsclient_tty_identifies_its_terminal_to_the_server() {
    use std::ffi::CStr;
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::thread;

    let repo_tmp = PathBuf::from(env!("CARGO_WORKSPACE_DIR")).join("tmp");
    fs::create_dir_all(&repo_tmp).expect("repo-local tmp dir");
    let dir = tempfile::Builder::new()
        .prefix("neomacsclient-tty-")
        .tempdir_in(repo_tmp)
        .expect("repo-local tempdir");
    let socket = dir.path().join("server");
    let listener = UnixListener::bind(&socket).expect("bind local socket");

    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        master_fd >= 0,
        "posix_openpt: {}",
        std::io::Error::last_os_error()
    );
    let master = unsafe { File::from_raw_fd(master_fd) };
    assert_eq!(unsafe { libc::grantpt(master_fd) }, 0, "grantpt failed");
    assert_eq!(unsafe { libc::unlockpt(master_fd) }, 0, "unlockpt failed");
    let mut slave_name = vec![0i8; 1024];
    assert_eq!(
        unsafe { libc::ptsname_r(master_fd, slave_name.as_mut_ptr(), slave_name.len()) },
        0,
        "ptsname_r failed"
    );
    let slave_name = unsafe { CStr::from_ptr(slave_name.as_ptr()) }
        .to_str()
        .expect("PTY path should be UTF-8")
        .to_owned();
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_name)
        .expect("open PTY slave");

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        stream
            .write_all(b"-print FRAME_OK\n")
            .expect("write response");
        String::from_utf8(request).expect("utf8 request")
    });

    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .arg("--socket-name")
        .arg(&socket)
        .arg("-t")
        .env("TERM", "xterm-256color")
        .stdout(Stdio::from(slave))
        .output()
        .expect("neomacsclient -t should run");

    let request = server.join().expect("server thread should finish");
    drop(master);
    assert!(output.status.success(), "{output:?}");
    assert!(request.contains(&format!(" -tty {slave_name} xterm-256color ")));
    assert!(request.contains(" -env TERM=xterm-256color "));
    assert!(!request.contains(" -window-system "));
    assert!(!request.contains(" -current-frame "));
}

#[cfg(unix)]
#[test]
fn neomacsclient_tty_forwards_resize_to_the_server_process() {
    use std::ffi::CStr;
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    let repo_tmp = PathBuf::from(env!("CARGO_WORKSPACE_DIR")).join("tmp");
    fs::create_dir_all(&repo_tmp).expect("repo-local tmp dir");
    let dir = tempfile::Builder::new()
        .prefix("neomacsclient-resize-")
        .tempdir_in(repo_tmp)
        .expect("repo-local tempdir");
    let socket = dir.path().join("server");
    let listener = UnixListener::bind(&socket).expect("bind local socket");

    let resize_seen = Arc::new(AtomicBool::new(false));
    let signal_id = signal_hook::flag::register(libc::SIGWINCH, Arc::clone(&resize_seen))
        .expect("install resize observer");
    let test_pid = std::process::id();
    let (release_server, await_release) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            if byte[0] == b'\n' {
                break;
            }
        }
        writeln!(stream, "-emacs-pid {test_pid}").expect("advertise server PID");
        stream.flush().expect("flush server PID");
        await_release.recv().expect("test releases server");
        stream.write_all(b"-print DONE\n").expect("finish response");
    });

    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master_fd >= 0, "posix_openpt failed");
    let master = unsafe { File::from_raw_fd(master_fd) };
    assert_eq!(unsafe { libc::grantpt(master_fd) }, 0, "grantpt failed");
    assert_eq!(unsafe { libc::unlockpt(master_fd) }, 0, "unlockpt failed");
    let mut slave_name = vec![0i8; 1024];
    assert_eq!(
        unsafe { libc::ptsname_r(master_fd, slave_name.as_mut_ptr(), slave_name.len()) },
        0,
        "ptsname_r failed"
    );
    let slave_name = unsafe { CStr::from_ptr(slave_name.as_ptr()) }
        .to_str()
        .expect("PTY path should be UTF-8");
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .open(slave_name)
        .expect("open PTY slave");
    let mut client = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .arg("--socket-name")
        .arg(&socket)
        .arg("-t")
        .env("TERM", "xterm-256color")
        .stdout(Stdio::from(slave))
        .spawn()
        .expect("spawn neomacsclient -t");

    let deadline = Instant::now() + Duration::from_secs(2);
    while !resize_seen.load(Ordering::Acquire) && Instant::now() < deadline {
        unsafe { libc::kill(client.id() as libc::pid_t, libc::SIGWINCH) };
        thread::sleep(Duration::from_millis(10));
    }
    let forwarded = resize_seen.load(Ordering::Acquire);
    release_server.send(()).expect("release fake server");
    let status = client.wait().expect("wait for neomacsclient");
    server.join().expect("server thread should finish");
    signal_hook::low_level::unregister(signal_id);
    drop(master);

    assert!(status.success(), "neomacsclient failed: {status}");
    assert!(
        forwarded,
        "SIGWINCH was not forwarded to the PID advertised by the server"
    );
}

#[test]
fn neomacsclient_sends_gnu_auth_for_tcp_server_file() {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::thread;

    let repo_tmp = PathBuf::from(env!("CARGO_WORKSPACE_DIR")).join("tmp");
    fs::create_dir_all(&repo_tmp).expect("repo-local tmp dir");
    let dir = tempfile::Builder::new()
        .prefix("neomacsclient-tcp-")
        .tempdir_in(repo_tmp)
        .expect("repo-local tempdir");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind tcp listener");
    let port = listener.local_addr().expect("local addr").port();
    let auth_key = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!?";
    let auth_file = dir.path().join("server-auth");
    fs::write(&auth_file, format!("127.0.0.1:{port} 12345\n{auth_key}")).expect("write auth file");

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        stream
            .write_all(b"-print TCP_OK\n")
            .expect("write response");
        String::from_utf8(request).expect("utf8 request")
    });

    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .arg("--server-file")
        .arg(&auth_file)
        .arg("--eval")
        .arg("(+ 1 2)")
        .output()
        .expect("neomacsclient should run");

    let request = server.join().expect("server thread should finish");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "TCP_OK");
    assert!(request.starts_with(&format!("-auth {auth_key} -dir ")));
    assert!(request.contains(" -current-frame "));
    assert!(request.contains(" -eval (+&_1&_2) "));
    assert!(request.ends_with(" \n"));
}

#[test]
fn relative_explicit_server_file_uses_home_auth_directory() {
    use std::fs;

    let home = tempfile::tempdir().expect("temporary home");
    let name = format!("neomacsclient-relative-{}", std::process::id());
    let auth_file = home.path().join(".emacs.d").join("server").join(&name);
    fs::create_dir_all(auth_file.parent().expect("auth parent")).expect("auth directory");
    fs::write(
        &auth_file,
        "127.0.0.1:1 12345\nabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!?",
    )
    .expect("auth file");

    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .args(["--server-file", &name, "--eval", "(+ 1 2)"])
        .env("HOME", home.path())
        .env_remove("APPDATA")
        .env_remove("USERPROFILE")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("EMACS_SERVER_FILE")
        .output()
        .expect("neomacsclient should run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr.contains("127.0.0.1:1"),
        "expected auth file {}, got: {stderr}",
        auth_file.display()
    );
}

#[test]
fn relative_environment_server_file_uses_home_auth_directory() {
    use std::fs;

    let home = tempfile::tempdir().expect("temporary home");
    let name = format!("neomacsclient-env-relative-{}", std::process::id());
    let auth_file = home.path().join(".emacs.d").join("server").join(&name);
    fs::create_dir_all(auth_file.parent().expect("auth parent")).expect("auth directory");
    fs::write(
        &auth_file,
        "127.0.0.1:1 12345\nabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!?",
    )
    .expect("auth file");

    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .args(["--eval", "(+ 1 2)"])
        .env("HOME", home.path())
        .env_remove("APPDATA")
        .env_remove("USERPROFILE")
        .env_remove("XDG_CONFIG_HOME")
        .env("EMACS_SERVER_FILE", &name)
        .output()
        .expect("neomacsclient should run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr.contains("127.0.0.1:1"),
        "expected auth file {}, got: {stderr}",
        auth_file.display()
    );
}

#[cfg(windows)]
#[test]
fn windows_explicit_server_file_overrides_environment() {
    let explicit = tempfile::tempdir().expect("temporary explicit server directory");
    let explicit_auth = explicit.path().join("explicit-server");
    write_windows_auth_file_at(&explicit_auth, 1);
    let home = tempfile::tempdir().expect("temporary home");
    let home_auth = write_windows_auth_file(home.path(), "server", 2);

    let output = run_windows_client(
        &[
            "--server-file",
            explicit_auth.to_str().expect("explicit auth path"),
            "--eval",
            "(+ 1 2)",
        ],
        &[
            ("HOME", Some(home.path())),
            ("EMACS_SERVER_FILE", Some(home_auth.as_path())),
        ],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("127.0.0.1:1"), "{stderr}");
    assert!(!stderr.contains("127.0.0.1:2"), "{stderr}");
}

#[cfg(windows)]
#[test]
fn windows_server_file_environment_overrides_default_target() {
    let home = tempfile::tempdir().expect("temporary home");
    write_windows_auth_file(home.path(), "server", 2);
    let env_file = tempfile::tempdir().expect("temporary environment server directory");
    let env_auth = env_file.path().join("server-auth");
    write_windows_auth_file_at(&env_auth, 1);

    let output = run_windows_client(
        &["--eval", "(+ 1 2)"],
        &[
            ("HOME", Some(home.path())),
            ("EMACS_SERVER_FILE", Some(env_auth.as_path())),
        ],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("127.0.0.1:1"), "{stderr}");
    assert!(!stderr.contains("127.0.0.1:2"), "{stderr}");
}

#[test]
fn neomacsclient_create_frame_uses_neomacs_window_system_without_display() {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::thread;

    let repo_tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("neomacs-bin should live under the repository root")
        .join("tmp");
    fs::create_dir_all(&repo_tmp).expect("repo-local tmp dir");
    let dir = tempfile::Builder::new()
        .prefix("neomacsclient-create-frame-default-")
        .tempdir_in(repo_tmp)
        .expect("repo-local tempdir");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind tcp listener");
    let port = listener.local_addr().expect("local addr").port();
    let auth_key = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!?";
    let auth_file = dir.path().join("server-auth");
    fs::write(&auth_file, format!("127.0.0.1:{port} 12345\n{auth_key}")).expect("write auth file");

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        stream
            .write_all(b"-print FRAME_OK\n")
            .expect("write response");
        String::from_utf8(request).expect("utf8 request")
    });

    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .args([
            "--server-file",
            auth_file.to_str().expect("auth file path"),
            "--create-frame",
            "--no-wait",
            "file.txt",
        ])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("neomacsclient should run");

    let request = server.join().expect("server thread should finish");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "FRAME_OK");
    assert!(request.contains(" -display neomacs "));
    assert!(request.contains(" -window-system "));
}

#[test]
fn neomacsclient_reports_unsupported_window_system_frame() {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::thread;

    let repo_tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("neomacs-bin should live under the repository root")
        .join("tmp");
    fs::create_dir_all(&repo_tmp).expect("repo-local tmp dir");
    let dir = tempfile::Builder::new()
        .prefix("neomacsclient-window-system-unsupported-")
        .tempdir_in(repo_tmp)
        .expect("repo-local tempdir");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind tcp listener");
    let port = listener.local_addr().expect("local addr").port();
    let auth_key = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!?";
    let auth_file = dir.path().join("server-auth");
    fs::write(&auth_file, format!("127.0.0.1:{port} 12345\n{auth_key}")).expect("write auth file");

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            request.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        stream
            .write_all(b"-window-system-unsupported \n")
            .expect("write response");
        String::from_utf8(request).expect("utf8 request")
    });

    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .args([
            "--server-file",
            auth_file.to_str().expect("auth file path"),
            "-c",
            "--no-wait",
            "file.txt",
        ])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("neomacsclient should run");

    let request = server.join().expect("server thread should finish");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr.contains("server does not support creating a window-system frame"),
        "{stderr}"
    );
    assert!(request.contains(" -window-system "));
}

#[cfg(windows)]
fn run_windows_client(
    args: &[&str],
    envs: &[(&str, Option<&std::path::Path>)],
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_neomacsclient"));
    command.args(args);
    for name in [
        "HOME",
        "APPDATA",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "EMACS_SERVER_FILE",
        "EMACS_SOCKET_NAME",
    ] {
        command.env_remove(name);
    }
    for (name, value) in envs {
        if let Some(value) = value {
            command.env(name, value);
        } else {
            command.env_remove(name);
        }
    }
    command.output().expect("neomacsclient should run")
}

#[cfg(windows)]
fn write_windows_auth_file(home: &std::path::Path, name: &str, port: u16) -> std::path::PathBuf {
    let path = home.join(".emacs.d").join("server").join(name);
    write_windows_auth_file_at(&path, port);
    path
}

#[cfg(windows)]
fn write_windows_auth_file_at(path: &std::path::Path, port: u16) {
    std::fs::create_dir_all(path.parent().expect("auth file parent")).expect("auth file directory");
    std::fs::write(
        path,
        format!(
            "127.0.0.1:{port} 12345\nabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!?"
        ),
    )
    .expect("auth file");
}

#[cfg(windows)]
#[test]
fn windows_terminal_client_frames_are_rejected_before_connection_or_startup() {
    let output = Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .args(["-t", "-a", "", "--eval", "(+ 1 2)"])
        .output()
        .expect("neomacsclient should run");

    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("creating a TTY frame is not supported on this platform")
    );
}

#[cfg(windows)]
#[test]
fn windows_alternate_editor_uses_cmd_shell() {
    let output = run_windows_client(
        &[
            "--server-file",
            "missing",
            "-a",
            "exit 0",
            "--eval",
            "(+ 1 2)",
        ],
        &[],
    );

    assert!(output.status.success(), "{output:?}");
}

#[cfg(test)]
#[allow(dead_code)]
mod neomacsclient_impl_tests {
    include!("../src/bin/neomacsclient.rs");

    struct EnvironmentGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvironmentGuard {
        fn new(names: &[&'static str]) -> Self {
            Self {
                saved: names
                    .iter()
                    .map(|&name| (name, env::var_os(name)))
                    .collect(),
            }
        }

        fn set(&self, name: &'static str, value: Option<&std::ffi::OsStr>) {
            unsafe {
                if let Some(value) = value {
                    env::set_var(name, value);
                } else {
                    env::remove_var(name);
                }
            }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            unsafe {
                for (name, value) in &self.saved {
                    if let Some(value) = value {
                        env::set_var(name, value);
                    } else {
                        env::remove_var(name);
                    }
                }
            }
        }
    }

    static ENVIRONMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn local_socket_path_resolution_error_is_connection_error() {
        if !neovm_core::local_socket::stream_supported() {
            return;
        }

        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let env_guard = EnvironmentGuard::new(&["EMACS_SERVER_FILE", "EMACS_SOCKET_NAME"]);
        env_guard.set("EMACS_SERVER_FILE", None);
        env_guard.set("EMACS_SOCKET_NAME", None);

        let options = Options {
            socket_name: Some("x".repeat(256)),
            alternate_editor: Some(String::new()),
            eval: true,
            args: vec!["(+ 1 2)".to_string()],
            ..Options::default()
        };

        let result = try_client("neomacsclient", &options);
        assert!(matches!(
            result,
            Err(ClientConnectError::Connection(message)) if message.contains("local socket path")
        ));
    }

    #[test]
    fn alternate_editor_empty_starts_daemon_once_and_retries_original_request() {
        use std::fs;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::path::Path;
        use std::sync::{Arc, Mutex};

        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let dir = tempfile::tempdir().expect("temporary server directory");
        let auth_file = dir.path().join("server-auth");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind tcp listener");
        let port = listener.local_addr().expect("local addr").port();
        let request = Arc::new(Mutex::new(None));
        let received = Arc::clone(&request);

        let options = Options {
            nowait: true,
            frame: FrameRequest::NewGraphical,
            server_file: Some(auth_file.to_string_lossy().into_owned()),
            alternate_editor: Some(String::new()),
            args: vec!["file.txt".to_string()],
            ..Options::default()
        };
        #[cfg(unix)]
        let expected_args = [
            OsString::from("--eval"),
            OsString::from(format!(
                r#"(setq server-use-tcp t server-auth-dir "{}")"#,
                dir.path().display()
            )),
            OsString::from("--daemon=server-auth"),
        ];
        #[cfg(not(unix))]
        let expected_args = {
            let escaped_parent = auth_file
                .parent()
                .expect("auth file parent")
                .display()
                .to_string()
                .replace('\\', r"\\")
                .replace('"', r#"\""#);
            [
                OsString::from("--eval"),
                OsString::from(format!(
                    "(setq server-use-tcp t server-auth-dir \"{}\")",
                    escaped_parent
                )),
                OsString::from("--daemon=server-auth"),
            ]
        };

        let result = start_daemon_and_retry_with_runner(
            "neomacsclient",
            options,
            Path::new("fake-neomacs"),
            |_, args| {
                assert_eq!(args, expected_args);
                fs::write(
                    &auth_file,
                    format!(
                        "127.0.0.1:{port} 12345\nabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!?"
                    ),
                )
                .expect("write auth file");
                std::thread::spawn(move || {
                    let (mut stream, _) = listener.accept().expect("accept client");
                    let mut bytes = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        stream.read_exact(&mut byte).expect("read request byte");
                        bytes.push(byte[0]);
                        if byte[0] == b'\n' {
                            break;
                        }
                    }
                    *received.lock().expect("request mutex") =
                        Some(String::from_utf8(bytes).expect("request utf8"));
                    stream.write_all(b"-print OK\n").expect("write response");
                });
                Ok(true)
            },
        );

        assert!(result.is_ok());
        let request = request
            .lock()
            .expect("request mutex")
            .clone()
            .expect("server should receive request");
        assert!(request.contains(" -nowait "));
        assert!(request.contains(" -window-system "));
        assert!(request.contains(" -file file.txt "));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_server_file_daemon_arguments_escape_lisp_path_and_use_basename() {
        use std::ffi::OsString;
        use std::path::Path;
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().expect("temporary server directory");
        let parent = dir.path().join(r#"parent with \ and "quotes"#);
        std::fs::create_dir_all(&parent).expect("server parent directory");
        let auth_file = parent.join(r#"auth "file"\name"#);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let options = Options {
            server_file: Some(auth_file.to_string_lossy().into_owned()),
            alternate_editor: Some(String::new()),
            args: vec!["file.txt".to_string()],
            ..Options::default()
        };

        let result = start_daemon_and_retry_with_runner(
            "neomacsclient",
            options,
            Path::new("fake-neomacs"),
            move |_, args| {
                *captured.lock().expect("runner mutex") = args.to_vec();
                Ok(false)
            },
        );

        let escaped_parent = parent
            .to_string_lossy()
            .replace('\\', r"\\")
            .replace('"', r#"\""#);
        let expected = vec![
            OsString::from("--eval"),
            OsString::from(format!(
                r#"(setq server-use-tcp t server-auth-dir "{}")"#,
                escaped_parent
            )),
            OsString::from(format!(
                "--daemon={}",
                auth_file
                    .file_name()
                    .expect("auth file name")
                    .to_string_lossy()
            )),
        ];
        assert_eq!(*observed.lock().expect("runner mutex"), expected);
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn relative_tcp_auth_candidates_follow_established_order() {
        let root = tempfile::tempdir().expect("temporary candidate root");
        let home = root.path().join("home");
        let xdg = root.path().join("xdg");
        let name = "ordered-server";
        let candidates = tcp_server_file_candidates_from_paths(name, Some(&home), Some(&xdg));

        assert_eq!(
            candidates,
            vec![
                home.join(".emacs.d").join("server").join(name),
                xdg.join("emacs").join("server").join(name),
                home.join(".config").join("emacs").join("server").join(name),
            ]
        );
    }

    #[test]
    fn relative_tcp_auth_resolution_selects_first_existing_candidate() {
        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let env_guard =
            EnvironmentGuard::new(&["HOME", "APPDATA", "USERPROFILE", "XDG_CONFIG_HOME"]);
        let root = tempfile::tempdir().expect("temporary candidate root");
        let home = root.path().join("home");
        let xdg = root.path().join("xdg");
        let name = format!("neomacsclient-order-{}", std::process::id());
        let xdg_candidate = xdg.join("emacs").join("server").join(&name);
        let config_candidate = home
            .join(".config")
            .join("emacs")
            .join("server")
            .join(&name);
        std::fs::create_dir_all(xdg_candidate.parent().expect("xdg parent"))
            .expect("xdg directory");
        std::fs::create_dir_all(config_candidate.parent().expect("config parent"))
            .expect("config directory");
        std::fs::write(&xdg_candidate, "").expect("xdg candidate");
        std::fs::write(&config_candidate, "").expect("config candidate");
        env_guard.set("HOME", Some(home.as_os_str()));
        env_guard.set("XDG_CONFIG_HOME", Some(xdg.as_os_str()));
        for variable in ["APPDATA", "USERPROFILE"] {
            env_guard.set(variable, None);
        }

        assert_eq!(resolve_tcp_server_file(&name), Some(xdg_candidate.clone()));

        let emacs_d_candidate = home.join(".emacs.d").join("server").join(&name);
        std::fs::create_dir_all(emacs_d_candidate.parent().expect("emacs.d parent"))
            .expect("emacs.d directory");
        std::fs::write(&emacs_d_candidate, "").expect("emacs.d candidate");
        assert_eq!(resolve_tcp_server_file(&name), Some(emacs_d_candidate));
    }

    #[cfg(unix)]
    #[test]
    fn relative_tcp_auth_candidates_do_not_fall_back_to_cwd_without_home() {
        assert!(tcp_server_file_candidates_from_paths("server", None, None).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn relative_tcp_auth_candidates_use_windows_platform_home_fallback() {
        let fallback = Path::new(r"C:\");
        let candidates = tcp_server_file_candidates_from_paths("server", Some(fallback), None);
        assert_eq!(
            candidates.first(),
            Some(&fallback.join(".emacs.d").join("server").join("server"))
        );
    }

    #[test]
    fn relative_missing_server_file_daemon_targets_first_auth_candidate() {
        use std::ffi::OsString;
        use std::path::Path;
        use std::sync::{Arc, Mutex};

        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let env_guard =
            EnvironmentGuard::new(&["HOME", "APPDATA", "USERPROFILE", "XDG_CONFIG_HOME"]);
        let home = tempfile::tempdir().expect("temporary home");
        env_guard.set("HOME", Some(home.path().as_os_str()));
        for name in ["APPDATA", "USERPROFILE", "XDG_CONFIG_HOME"] {
            env_guard.set(name, None);
        }

        let name = format!("neomacsclient-missing-{}", std::process::id());
        let expected_parent = home.path().join(".emacs.d").join("server");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let options = Options {
            server_file: Some(name.clone()),
            alternate_editor: Some(String::new()),
            args: vec!["file.txt".to_string()],
            ..Options::default()
        };

        let result = start_daemon_and_retry_with_runner(
            "neomacsclient",
            options,
            Path::new("fake-neomacs"),
            move |_, args| {
                *captured.lock().expect("runner mutex") = args.to_vec();
                Ok(false)
            },
        );

        let escaped_parent = expected_parent
            .to_string_lossy()
            .replace('\\', r"\\")
            .replace('"', r#"\""#);
        let expected = vec![
            OsString::from("--eval"),
            OsString::from(format!(
                r#"(setq server-use-tcp t server-auth-dir "{}")"#,
                escaped_parent
            )),
            OsString::from(format!("--daemon={name}")),
        ];
        assert_eq!(*observed.lock().expect("runner mutex"), expected);
        assert!(result.is_err(), "{result:?}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_explicit_server_file_daemon_arguments_escape_path_and_use_basename() {
        use std::ffi::OsString;
        use std::path::Path;
        use std::sync::{Arc, Mutex};

        let server_file = r#"C:\Program Files\Neo "Macs"\server"#;
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let options = Options {
            server_file: Some(server_file.to_string()),
            alternate_editor: Some(String::new()),
            args: vec!["file.txt".to_string()],
            ..Options::default()
        };

        let result = start_daemon_and_retry_with_runner(
            "neomacsclient",
            options,
            Path::new("fake-neomacs"),
            move |_, args| {
                *captured.lock().expect("runner mutex") = args.to_vec();
                Ok(false)
            },
        );

        let expected = vec![
            OsString::from("--eval"),
            OsString::from(
                "(setq server-use-tcp t server-auth-dir \"C:\\\\Program Files\\\\Neo \\\"Macs\\\"\")",
            ),
            OsString::from("--daemon=server"),
        ];
        assert_eq!(*observed.lock().expect("runner mutex"), expected);
        assert!(result.is_err(), "{result:?}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_bare_socket_name_target_follows_runtime_support() {
        use std::path::PathBuf;

        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let name = "named-server";
        let options = Options {
            socket_name: Some(name.to_string()),
            ..Options::default()
        };

        let expected_path = neovm_core::local_socket::socket_path_for_name(name)
            .expect("resolve expected local socket path");
        assert_eq!(
            resolve_server_target_with(&options, true).expect("resolve supported target"),
            ServerTarget::Local(expected_path)
        );
        assert_eq!(
            resolve_server_target_with(&options, false).expect("resolve fallback target"),
            ServerTarget::Tcp(PathBuf::from(name))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_bare_name_directory_policy_failure_falls_back_to_tcp() {
        use std::ffi::OsString;
        use std::path::PathBuf;

        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let env_guard = EnvironmentGuard::new(&[
            "TEMP",
            "LOCALAPPDATA",
            "NEOMACS_SERVER_SOCKET_DIR",
            "EMACS_SERVER_FILE",
            "EMACS_SOCKET_NAME",
        ]);
        let unusable_directory = format!(r"C:\{}", "x".repeat(300));
        env_guard.set(
            "TEMP",
            Some(OsString::from(&unusable_directory).as_os_str()),
        );
        env_guard.set(
            "LOCALAPPDATA",
            Some(OsString::from(&unusable_directory).as_os_str()),
        );
        env_guard.set("NEOMACS_SERVER_SOCKET_DIR", None);
        env_guard.set("EMACS_SERVER_FILE", None);
        env_guard.set("EMACS_SOCKET_NAME", None);

        let options = Options::default();
        assert_eq!(
            resolve_server_target_with(&options, true).expect("bare target fallback"),
            ServerTarget::Tcp(PathBuf::from("server"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_explicit_overlong_socket_path_is_retryable_connection_error() {
        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let env_guard = EnvironmentGuard::new(&["EMACS_SERVER_FILE", "EMACS_SOCKET_NAME"]);
        env_guard.set("EMACS_SERVER_FILE", None);
        env_guard.set("EMACS_SOCKET_NAME", None);

        let socket_name = format!(r"C:\{}", "x".repeat(300));
        let options = Options {
            socket_name: Some(socket_name),
            alternate_editor: Some(String::new()),
            ..Options::default()
        };

        assert!(matches!(
            try_client("neomacsclient", &options),
            Err(ClientConnectError::Connection(message))
                if message.contains("local socket path")
        ));
    }

    #[cfg(windows)]
    fn assert_windows_unsupported_target_uses_tcp_auth_file(home_variable: &'static str) {
        use std::fs;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::path::PathBuf;
        use std::thread;

        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let env_guard = EnvironmentGuard::new(&[
            "HOME",
            "APPDATA",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "EMACS_SERVER_FILE",
            "EMACS_SOCKET_NAME",
        ]);
        let root = tempfile::tempdir().expect("temporary auth root");
        let home = root.path().join("home");
        fs::create_dir_all(&home).expect("auth home");
        env_guard.set("HOME", None);
        env_guard.set("APPDATA", None);
        env_guard.set("USERPROFILE", None);
        env_guard.set("XDG_CONFIG_HOME", None);
        env_guard.set("EMACS_SERVER_FILE", None);
        env_guard.set("EMACS_SOCKET_NAME", None);
        env_guard.set(home_variable, Some(home.as_os_str()));

        let auth_file = home.join(".emacs.d").join("server").join("server");
        fs::create_dir_all(auth_file.parent().expect("auth parent")).expect("auth directory");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind tcp listener");
        let port = listener.local_addr().expect("local address").port();
        let auth_key = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!?";
        fs::write(&auth_file, format!("127.0.0.1:{port} 12345\n{auth_key}"))
            .expect("write auth file");

        let options = Options {
            eval: true,
            args: vec!["(+ 1 2)".to_string()],
            ..Options::default()
        };
        let target =
            resolve_server_target_with(&options, false).expect("resolve unsupported target");
        assert_eq!(target, ServerTarget::Tcp(PathBuf::from("server")));

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept tcp client");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                stream.read_exact(&mut byte).expect("read request byte");
                request.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            stream
                .write_all(b"-print TCP_FALLBACK\n")
                .expect("write response");
            request
        });

        let result = match target {
            ServerTarget::Tcp(server_file) => run_tcp_client(&options, &server_file),
            ServerTarget::Local(_) => panic!("unsupported target must use TCP"),
        };
        assert!(result.is_ok());
        let request = server.join().expect("server thread should finish");
        assert!(String::from_utf8_lossy(&request).starts_with(&format!("-auth {auth_key} ")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_unsupported_default_uses_home_tcp_auth_file() {
        assert_windows_unsupported_target_uses_tcp_auth_file("HOME");
    }

    #[cfg(windows)]
    #[test]
    fn windows_unsupported_default_uses_appdata_tcp_auth_file() {
        assert_windows_unsupported_target_uses_tcp_auth_file("APPDATA");
    }

    #[cfg(windows)]
    #[test]
    fn windows_unsupported_default_uses_userprofile_tcp_auth_file() {
        assert_windows_unsupported_target_uses_tcp_auth_file("USERPROFILE");
    }

    #[cfg(windows)]
    #[test]
    fn windows_implicit_empty_alternate_uses_local_daemon_argument() {
        use std::ffi::OsString;
        use std::path::Path;
        use std::sync::{Arc, Mutex};

        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let env_guard = EnvironmentGuard::new(&[
            "HOME",
            "APPDATA",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "LOCALAPPDATA",
            "TEMP",
            "NEOMACS_SERVER_SOCKET_DIR",
            "EMACS_SERVER_FILE",
            "EMACS_SOCKET_NAME",
        ]);
        let home = tempfile::tempdir().expect("temporary home");
        let socket_dir = tempfile::tempdir().expect("temporary socket directory");
        env_guard.set("HOME", Some(home.path().as_os_str()));
        env_guard.set(
            "NEOMACS_SERVER_SOCKET_DIR",
            Some(socket_dir.path().as_os_str()),
        );
        for name in [
            "APPDATA",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "LOCALAPPDATA",
            "TEMP",
            "EMACS_SERVER_FILE",
            "EMACS_SOCKET_NAME",
        ] {
            env_guard.set(name, None);
        }

        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let options = Options {
            frame: FrameRequest::NewGraphical,
            alternate_editor: Some(String::new()),
            args: vec!["file.txt".to_string()],
            ..Options::default()
        };

        let result = start_daemon_and_retry_with_runner(
            "neomacsclient",
            options,
            Path::new("fake-neomacs"),
            move |_, args| {
                *captured.lock().expect("runner mutex") = args.to_vec();
                Ok(false)
            },
        );

        assert_eq!(
            *observed.lock().expect("runner mutex"),
            vec![OsString::from("--daemon=server")]
        );
        assert!(result.is_err(), "{result:?}");
    }

    #[cfg(windows)]
    fn assert_windows_unsupported_daemon_arguments_use_auth_dir(home_variable: &'static str) {
        use std::ffi::OsString;

        let _lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let env_guard = EnvironmentGuard::new(&[
            "HOME",
            "APPDATA",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "LOCALAPPDATA",
            "TEMP",
            "NEOMACS_SERVER_SOCKET_DIR",
            "EMACS_SERVER_FILE",
            "EMACS_SOCKET_NAME",
        ]);
        let home = tempfile::tempdir().expect("temporary auth home");
        let socket_dir = tempfile::tempdir().expect("temporary socket directory");
        for name in [
            "HOME",
            "APPDATA",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "LOCALAPPDATA",
            "TEMP",
            "EMACS_SERVER_FILE",
            "EMACS_SOCKET_NAME",
        ] {
            env_guard.set(name, None);
        }
        env_guard.set(home_variable, Some(home.path().as_os_str()));
        env_guard.set(
            "NEOMACS_SERVER_SOCKET_DIR",
            Some(socket_dir.path().as_os_str()),
        );

        let options = Options {
            frame: FrameRequest::NewGraphical,
            alternate_editor: Some(String::new()),
            args: vec!["file.txt".to_string()],
            ..Options::default()
        };
        let observed = daemon_arguments_with(&options, false).expect("TCP daemon arguments");
        let auth_dir = home.path().join(".emacs.d").join("server");
        let escaped_auth_dir = auth_dir
            .to_string_lossy()
            .replace('\\', r"\\")
            .replace('"', r#"\""#);
        assert_eq!(
            observed,
            vec![
                OsString::from("--eval"),
                OsString::from(format!(
                    "(setq server-use-tcp t server-auth-dir \"{escaped_auth_dir}\")"
                )),
                OsString::from("--daemon=server"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_unsupported_empty_alternate_uses_home_tcp_daemon_arguments() {
        assert_windows_unsupported_daemon_arguments_use_auth_dir("HOME");
    }

    #[cfg(windows)]
    #[test]
    fn windows_unsupported_empty_alternate_uses_appdata_tcp_daemon_arguments() {
        assert_windows_unsupported_daemon_arguments_use_auth_dir("APPDATA");
    }
}
