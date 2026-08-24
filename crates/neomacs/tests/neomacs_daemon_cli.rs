use std::fs;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use neovm_core::local_socket::stream_supported;

const DAEMON_NAME: &str = "neomacs-task-6-smoke";

#[test]
fn foreground_named_daemon_smoke() {
    if std::env::var_os("NEOMACS_RUN_DAEMON_SMOKE").is_none() {
        eprintln!("skipping daemon smoke; set NEOMACS_RUN_DAEMON_SMOKE=1 to run it");
        return;
    }

    let home = tempfile::Builder::new()
        .prefix("neomacs-daemon-home-")
        .tempdir()
        .expect("isolated daemon home");
    let log_file = home.path().join("daemon.log");
    let local_socket_supported = stream_supported();
    let endpoint = if local_socket_supported {
        home.path().join("socket").join(DAEMON_NAME)
    } else {
        home.path().join("server").join(DAEMON_NAME)
    };
    let mut daemon = {
        let mut command = Command::new(env!("CARGO_BIN_EXE_neomacs"));
        command
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("TMPDIR", home.path())
            .env("TEMP", home.path())
            .env("TMP", home.path())
            .env("XDG_RUNTIME_DIR", home.path())
            .env("NEOMACS_SERVER_SOCKET_DIR", home.path().join("socket"))
            .env("NEOMACS_LOG_FILE", &log_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("EMACS_SERVER_FILE")
            .env_remove("EMACS_SOCKET_NAME");
        if !local_socket_supported {
            let auth_dir = home.path().join("server");
            fs::create_dir_all(&auth_dir).expect("TCP auth directory");
            command.arg("--eval").arg(format!(
                "(setq server-use-tcp t server-auth-dir {})",
                elisp_string_literal(&auth_dir.to_string_lossy())
            ));
        }
        command.arg(format!("--fg-daemon={DAEMON_NAME}"));
        ChildGuard::spawn(&mut command)
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = daemon
            .try_wait()
            .expect("daemon child status should be readable")
        {
            panic!("foreground daemon exited before server startup: {status}");
        }
        let output = run_client(local_socket_supported, &endpoint, home.path(), "(daemonp)");
        if output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim() == format!("\"{DAEMON_NAME}\"")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon endpoint under {}",
            home.path().display()
        );
        thread::sleep(Duration::from_millis(50));
    }

    if local_socket_supported {
        assert_local_socket_endpoint(&endpoint);
    }

    let output = run_client(local_socket_supported, &endpoint, home.path(), "(daemonp)");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("\"{DAEMON_NAME}\"")
    );

    let _ = run_client(
        local_socket_supported,
        &endpoint,
        home.path(),
        "(kill-emacs)",
    );
    daemon.wait_for_exit();
}

fn run_client(
    local_socket_supported: bool,
    endpoint: &Path,
    home: &Path,
    form: &str,
) -> std::process::Output {
    let endpoint_option = if local_socket_supported {
        "--socket-name"
    } else {
        "--server-file"
    };
    let endpoint_argument = if local_socket_supported {
        DAEMON_NAME.to_string()
    } else {
        endpoint.display().to_string()
    };
    Command::new(env!("CARGO_BIN_EXE_neomacsclient"))
        .args([endpoint_option, &endpoint_argument, "--eval", form])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("TMPDIR", home)
        .env("TEMP", home)
        .env("TMP", home)
        .env("XDG_RUNTIME_DIR", home)
        .env("NEOMACS_SERVER_SOCKET_DIR", home.join("socket"))
        .env_remove("EMACS_SERVER_FILE")
        .env_remove("EMACS_SOCKET_NAME")
        .output()
        .expect("neomacsclient should run")
}

fn assert_local_socket_endpoint(endpoint: &Path) {
    let metadata = endpoint
        .symlink_metadata()
        .expect("local daemon endpoint metadata");
    #[cfg(unix)]
    assert!(
        metadata.file_type().is_socket(),
        "local daemon endpoint must be a Unix socket: {}",
        endpoint.display()
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        assert_ne!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT,
            0,
            "local daemon endpoint must be an AF_UNIX reparse-point socket: {}",
            endpoint.display()
        );
    }
}

fn elisp_string_literal(value: &str) -> String {
    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => literal.push_str(r"\\"),
            '"' => literal.push_str(r#"\""#),
            '\n' => literal.push_str(r"\n"),
            '\r' => literal.push_str(r"\r"),
            '\t' => literal.push_str(r"\t"),
            ch => literal.push(ch),
        }
    }
    literal.push('"');
    literal
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        Self {
            child: command.spawn().expect("foreground daemon should spawn"),
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self
                .child
                .try_wait()
                .expect("daemon child status should be readable")
                .is_some()
            {
                return;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self
            .child
            .try_wait()
            .expect("daemon child status should be readable")
            .is_none()
        {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}
