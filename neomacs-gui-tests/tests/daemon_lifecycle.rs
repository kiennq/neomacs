#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use neomacs_gui_tests::{
    CapturedProcess, DisplayHarness, GuiBackend, binary_path, native_window_ids, read_log_tail,
    wait_for_condition,
};
use neovm_core::local_socket::stream_supported;

const DAEMON_NAME: &str = "integration";
const CONDITION_TIMEOUT: Duration = Duration::from_secs(30);
const GUI_FRAME_COUNT: &str = "(let ((count 0)) (dolist (frame (frame-list) count) \
       (when (window-system frame) (setq count (1+ count)))))";

#[test]
fn foreground_named_daemon_preserves_terminal_frame_and_server() {
    if !stream_supported() {
        eprintln!("skipping daemon lifecycle; AF_UNIX stream sockets are unsupported");
        return;
    }
    let display = requested_backend().map(|backend| {
        DisplayHarness::for_backend(backend)
            .start_session(workspace_root().join("target/neomacs-gui-tests"))
            .expect("display session should start")
    });
    let session = DaemonSession::spawn_named(display.as_ref().map(|session| session.env()));
    let endpoint = session.wait_for_endpoint();
    assert_platform_transport(&endpoint, &session.failure_context());
    assert!(
        session.wait_for_responsive(),
        "{}",
        session.failure_context()
    );

    assert_eq!(
        session.eval("(daemonp)"),
        format!("\"{DAEMON_NAME}\""),
        "{}",
        session.failure_context()
    );
    assert_eq!(
        session.eval("(emacs-pid)"),
        session
            .process
            .as_ref()
            .expect("named daemon process")
            .pid()
            .to_string(),
        "{}",
        session.failure_context()
    );
    assert_eq!(
        session.eval(
            "(if (and (frame-live-p terminal-frame) \
                      (eq (selected-frame) terminal-frame) \
                      (null (window-system terminal-frame))) \
                 \"terminal-survives\" \
               \"terminal-lost\")"
        ),
        "\"terminal-survives\"",
        "{}",
        session.failure_context()
    );

    session.kill_emacs();
    assert!(
        session.wait_for_process_exit(),
        "{}",
        session.failure_context()
    );
}

#[test]
fn foreground_named_daemon_recreates_gui_frame_after_last_close() {
    if !stream_supported() {
        eprintln!("skipping daemon lifecycle; AF_UNIX stream sockets are unsupported");
        return;
    }
    let Some(backend) = requested_backend() else {
        eprintln!(
            "skipping display-dependent daemon lifecycle; set \
             NEOMACS_GUI_TEST_BACKEND=x11, wayland, macos, or windows"
        );
        return;
    };
    let display = DisplayHarness::for_backend(backend)
        .start_session(workspace_root().join("target/neomacs-gui-tests"))
        .expect("display session should start");
    let session = DaemonSession::spawn_named(Some(display.env()));
    let endpoint = session.wait_for_endpoint();
    assert_platform_transport(&endpoint, &session.failure_context());
    assert!(
        session.wait_for_responsive(),
        "{}",
        session.failure_context()
    );
    assert_eq!(
        session.eval("(daemonp)"),
        format!("\"{DAEMON_NAME}\""),
        "{}",
        session.failure_context()
    );

    let first = session.client(&["-c", "-n"]);
    assert!(
        first.status.success(),
        "first GUI client failed: {first:?}\n{}",
        session.failure_context()
    );
    assert!(
        session.wait_for_responsive(),
        "{}",
        session.failure_context()
    );
    assert_eq!(
        session.eval(GUI_FRAME_COUNT),
        "1",
        "first GUI client must create exactly one Lisp-visible window-system frame\n{}",
        session.failure_context()
    );
    let first_native_ids = session.native_window_ids(backend);

    let delete = session.client(&[
        "--eval",
        "(progn \
            (mapc (lambda (frame) \
                    (when (window-system frame) (delete-frame frame))) \
                  (frame-list)) \
            (if (and (frame-live-p terminal-frame) \
                     (eq (selected-frame) terminal-frame) \
                     (null (window-system terminal-frame))) \
                \"terminal-survives\" \
              \"terminal-lost\"))",
    ]);
    assert!(
        delete.status.success(),
        "deleting GUI frame failed: {delete:?}\n{}",
        session.failure_context()
    );
    assert_eq!(
        stdout_text(&delete),
        "\"terminal-survives\"",
        "{}",
        session.failure_context()
    );
    assert_eq!(
        session.eval(GUI_FRAME_COUNT),
        "0",
        "deleting GUI frames must leave zero Lisp-visible window-system frames\n{}",
        session.failure_context()
    );
    session.wait_for_native_window_ids_to_drain(backend, first_native_ids.as_deref());
    assert!(
        session.wait_for_responsive(),
        "{}",
        session.failure_context()
    );
    assert!(
        session.process_is_alive(),
        "daemon exited after its GUI frame was deleted\n{}",
        session.failure_context()
    );

    let second = session.client(&["-c", "-n"]);
    assert!(
        second.status.success(),
        "second GUI client failed: {second:?}\n{}",
        session.failure_context()
    );
    assert!(
        session.wait_for_responsive(),
        "{}",
        session.failure_context()
    );
    assert_eq!(
        session.eval(GUI_FRAME_COUNT),
        "1",
        "second GUI client must create exactly one Lisp-visible window-system frame\n{}",
        session.failure_context()
    );
    let second_native_ids = session.native_window_ids(backend);
    if let (Some(first), Some(second)) = (first_native_ids, second_native_ids) {
        assert!(
            first.iter().all(|id| !second.contains(id)),
            "second GUI frame reused a native identity: first={first:?}, second={second:?}"
        );
    }

    session.kill_emacs();
    assert!(
        session.wait_for_process_exit(),
        "{}",
        session.failure_context()
    );
}

#[test]
fn empty_alternate_starts_and_reuses_one_daemon() {
    if !stream_supported() {
        eprintln!("skipping daemon lifecycle; AF_UNIX stream sockets are unsupported");
        return;
    }
    let Some(backend) = requested_backend() else {
        eprintln!(
            "skipping display-dependent empty-alternate lifecycle; set \
             NEOMACS_GUI_TEST_BACKEND=x11, wayland, macos, or windows"
        );
        return;
    };
    let display = DisplayHarness::for_backend(backend)
        .start_session(workspace_root().join("target/neomacs-gui-tests"))
        .expect("display session should start");
    let session = DaemonSession::new(Some(display.env()));
    let first = session.client_default(&["-c", "-n", "-a", ""]);
    assert!(
        first.status.success(),
        "empty alternate should start a daemon: {first:?}\n{}",
        session.failure_context()
    );
    let endpoint = session.wait_for_endpoint_named("server");
    assert_platform_transport(&endpoint, &session.failure_context());

    let first_pid = session.wait_for_eval("(emacs-pid)");
    assert!(
        session.wait_for_responsive_named("server"),
        "{}",
        session.failure_context()
    );

    let second = session.client_default(&["-c", "-n", "-a", ""]);
    assert!(
        second.status.success(),
        "second empty alternate should connect to the existing daemon: {second:?}\n{}",
        session.failure_context()
    );
    let second_pid = session.wait_for_eval("(emacs-pid)");
    assert_eq!(
        first_pid,
        second_pid,
        "second invocation started another daemon\n{}",
        session.failure_context()
    );

    let kill = session.client_named("server", &["--eval", "(kill-emacs)"]);
    assert!(
        kill.status.success(),
        "empty-alternate daemon should accept explicit kill-emacs: {kill:?}\n{}",
        session.failure_context()
    );
}

struct DaemonSession {
    home: tempfile::TempDir,
    binary: PathBuf,
    client_binary: PathBuf,
    env: Vec<(String, String)>,
    process: Option<CapturedProcess>,
    server_name: String,
}

impl DaemonSession {
    fn new(display_env: Option<&[(String, String)]>) -> Self {
        let home = tempfile::Builder::new()
            .prefix("neomacs-daemon-integration-")
            .tempdir()
            .expect("isolated HOME/APPDATA");
        let binary = binary_path("neomacs").expect("Neomacs binary should be available");
        let client_binary =
            binary_path("neomacsclient").expect("neomacsclient binary should be available");
        let log_path = home.path().join("daemon.log");
        let mut env = vec![
            ("HOME".to_string(), home.path().display().to_string()),
            ("APPDATA".to_string(), home.path().display().to_string()),
            (
                "LOCALAPPDATA".to_string(),
                home.path().display().to_string(),
            ),
            ("USERPROFILE".to_string(), home.path().display().to_string()),
            (
                "XDG_RUNTIME_DIR".to_string(),
                home.path().display().to_string(),
            ),
            (
                "XDG_CONFIG_HOME".to_string(),
                home.path().display().to_string(),
            ),
            ("TMPDIR".to_string(), home.path().display().to_string()),
            ("TEMP".to_string(), home.path().display().to_string()),
            ("TMP".to_string(), home.path().display().to_string()),
            (
                "NEOMACS_LOG_FILE".to_string(),
                log_path.display().to_string(),
            ),
            ("RUST_LOG".to_string(), "info".to_string()),
            (
                "NEOMACS_RUNTIME_ROOT".to_string(),
                workspace_root().display().to_string(),
            ),
            ("NEOMACS".to_string(), binary.display().to_string()),
        ];
        if let Some(display_env) = display_env {
            env.extend(display_env.iter().cloned());
        }
        env.push((
            "NEOMACS_SERVER_SOCKET_DIR".to_string(),
            home.path().join("socket").display().to_string(),
        ));

        Self {
            home,
            binary,
            client_binary,
            env,
            process: None,
            server_name: DAEMON_NAME.to_string(),
        }
    }

    fn spawn_named(display_env: Option<&[(String, String)]>) -> Self {
        let mut session = Self::new(display_env);
        let mut command = Command::new(&session.binary);
        command
            .stdin(Stdio::null())
            .envs(session.env.iter().map(|(key, value)| (key, value)))
            .env_remove("EMACS_SERVER_FILE")
            .env_remove("EMACS_SOCKET_NAME");
        command.arg(format!("--fg-daemon={DAEMON_NAME}"));
        let process = CapturedProcess::spawn(&mut command, session.home.path(), "daemon")
            .expect("foreground daemon should spawn");
        session.process = Some(process);
        session
    }

    fn client(&self, args: &[&str]) -> Output {
        self.client_named(&self.server_name, args)
    }

    fn client_default(&self, args: &[&str]) -> Output {
        self.run_client(None, args)
    }

    fn client_named(&self, name: &str, args: &[&str]) -> Output {
        self.run_client(Some(name), args)
    }

    fn run_client(&self, name: Option<&str>, args: &[&str]) -> Output {
        let mut command = Command::new(&self.client_binary);
        let mut target = Vec::with_capacity(args.len() + 2);
        if let Some(name) = name {
            target.extend(["--socket-name", name]);
        }
        target.extend(args.iter().copied());
        command
            .args(target)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .env_remove("EMACS_SERVER_FILE")
            .env_remove("EMACS_SOCKET_NAME");
        command.output().expect("neomacsclient should run")
    }

    fn wait_for_endpoint(&self) -> PathBuf {
        self.wait_for_endpoint_named(&self.server_name)
    }

    fn wait_for_endpoint_named(&self, name: &str) -> PathBuf {
        let endpoint = self.endpoint_path(name);
        let mut daemon_exited = false;
        let ready = wait_for_condition(CONDITION_TIMEOUT, || {
            if self.owned_process_exited() {
                daemon_exited = true;
                return true;
            }
            let output = self.client_named(name, &["--timeout", "5", "--eval", "(daemonp)"]);
            output.status.success() && stdout_text(&output) == format!("\"{name}\"")
        });
        assert!(
            !daemon_exited,
            "owned daemon exited before endpoint readiness\n{}",
            self.failure_context()
        );
        assert!(ready, "{}", self.failure_context());
        endpoint
    }

    fn wait_for_responsive(&self) -> bool {
        self.wait_for_responsive_named(&self.server_name)
    }

    fn wait_for_responsive_named(&self, name: &str) -> bool {
        let mut daemon_exited = false;
        let ready = wait_for_condition(CONDITION_TIMEOUT, || {
            if self.owned_process_exited() {
                daemon_exited = true;
                return true;
            }
            let output = self.client_named(name, &["--timeout", "5", "--eval", "(daemonp)"]);
            output.status.success() && stdout_text(&output) == format!("\"{name}\"")
        });
        if daemon_exited {
            eprintln!(
                "owned daemon exited before responsiveness check completed\n{}",
                self.failure_context()
            );
        }
        ready && !daemon_exited
    }

    fn eval(&self, form: &str) -> String {
        stdout_text(&self.client(&["--eval", form]))
    }

    fn wait_for_eval(&self, form: &str) -> String {
        let mut value = String::new();
        let mut daemon_exited = false;
        let ready = wait_for_condition(CONDITION_TIMEOUT, || {
            if self.owned_process_exited() {
                daemon_exited = true;
                return true;
            }
            let output = self.client_named("server", &["--timeout", "5", "--eval", form]);
            if output.status.success() {
                value = stdout_text(&output);
                !value.is_empty()
            } else {
                false
            }
        });
        assert!(
            !daemon_exited,
            "owned daemon exited before evaluation became ready\n{}",
            self.failure_context()
        );
        assert!(ready, "{}", self.failure_context());
        value
    }

    fn native_window_ids(&self, backend: GuiBackend) -> Option<Vec<String>> {
        if backend != GuiBackend::LinuxX11 {
            return None;
        }
        let pid = self.process.as_ref().expect("named daemon process").pid();
        let mut ids = None;
        let mut error = None;
        let mut daemon_exited = false;
        wait_for_condition(CONDITION_TIMEOUT, || {
            if self.owned_process_exited() {
                daemon_exited = true;
                return true;
            }
            match native_window_ids(pid, &self.env) {
                Ok(values) if !values.is_empty() => {
                    ids = Some(values);
                    true
                }
                Ok(_) => false,
                Err(value) => {
                    error = Some(value);
                    true
                }
            }
        });
        assert!(
            !daemon_exited,
            "owned daemon exited before native window identity was captured\n{}",
            self.failure_context()
        );
        if let Some(error) = error {
            eprintln!("native window identity unavailable: {error}");
        }
        ids
    }

    fn wait_for_native_window_ids_to_drain(
        &self,
        backend: GuiBackend,
        first_ids: Option<&[String]>,
    ) {
        if backend != GuiBackend::LinuxX11 || first_ids.is_none() {
            return;
        }
        let mut daemon_exited = false;
        let mut error = None;
        let drained = wait_for_condition(CONDITION_TIMEOUT, || {
            if self.owned_process_exited() {
                daemon_exited = true;
                return true;
            }
            match native_window_ids(
                self.process.as_ref().expect("named daemon process").pid(),
                &self.env,
            ) {
                Ok(ids) => ids.is_empty(),
                Err(value) => {
                    error = Some(value);
                    true
                }
            }
        });
        assert!(
            !daemon_exited,
            "owned daemon exited while waiting for native X11 IDs to drain\n{}",
            self.failure_context()
        );
        if let Some(error) = error {
            eprintln!("native window identity unavailable: {error}");
            return;
        }
        assert!(
            drained,
            "native X11 window identities did not drain after deleting the GUI frame\n{}",
            self.failure_context()
        );
    }

    fn kill_emacs(&self) {
        let output = self.client(&["--eval", "(kill-emacs)"]);
        assert!(
            output.status.success(),
            "kill-emacs failed: {output:?}\n{}",
            self.failure_context()
        );
    }

    fn wait_for_process_exit(&self) -> bool {
        let Some(process) = &self.process else {
            return true;
        };
        wait_for_condition(CONDITION_TIMEOUT, || {
            process
                .try_wait()
                .expect("daemon child status should be readable")
                .is_some()
        })
    }

    fn process_is_alive(&self) -> bool {
        self.process
            .as_ref()
            .and_then(|process| process.try_wait().ok())
            .is_some_and(|status| status.is_none())
    }

    fn owned_process_exited(&self) -> bool {
        self.process
            .as_ref()
            .map(|process| {
                process
                    .try_wait()
                    .expect("daemon child status should be readable")
                    .is_some()
            })
            .unwrap_or(false)
    }

    fn failure_context(&self) -> String {
        let log = self.home.path().join("daemon.log");
        let process = self
            .process
            .as_ref()
            .map(CapturedProcess::diagnostics)
            .unwrap_or_else(|| "daemon is client-spawned".to_string());
        format!(
            "daemon diagnostics:\n{process}\nlog {}:\n{}",
            log.display(),
            read_log_tail(&log)
        )
    }

    fn endpoint_path(&self, name: &str) -> PathBuf {
        endpoint_path(self.socket_directory(), name)
    }

    fn socket_directory(&self) -> &Path {
        self.env
            .iter()
            .find_map(|(key, value)| {
                (key == "NEOMACS_SERVER_SOCKET_DIR").then_some(Path::new(value))
            })
            .expect("isolated local socket directory")
    }
}

impl Drop for DaemonSession {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            drop(process);
        } else {
            let _ = self.run_client(None, &["--timeout", "1", "--eval", "(kill-emacs)"]);
        }
    }
}

fn requested_backend() -> Option<GuiBackend> {
    match std::env::var("NEOMACS_GUI_TEST_BACKEND").ok()?.as_str() {
        "x11" | "linux-x11" => Some(GuiBackend::LinuxX11),
        "wayland" | "linux-wayland" => Some(GuiBackend::LinuxWayland),
        "macos" => Some(GuiBackend::Macos),
        "windows" => Some(GuiBackend::Windows),
        other => panic!("unsupported NEOMACS_GUI_TEST_BACKEND={other:?}"),
    }
}

fn assert_platform_transport(endpoint: &Path, failure_context: &str) {
    #[cfg(unix)]
    {
        assert!(
            endpoint
                .symlink_metadata()
                .expect("Unix server socket metadata")
                .file_type()
                .is_socket(),
            "Unix daemon endpoint must be a local socket: {}\n{}",
            endpoint.display(),
            failure_context
        );
    }
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        assert_ne!(
            endpoint
                .symlink_metadata()
                .expect("Windows local socket metadata")
                .file_attributes()
                & FILE_ATTRIBUTE_REPARSE_POINT,
            0,
            "Windows daemon endpoint must be an AF_UNIX reparse-point socket: {}\n{}",
            endpoint.display(),
            failure_context
        );
    }
}

fn endpoint_path(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("GUI test crate should live below workspace root")
        .to_path_buf()
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_path_is_deterministic() {
        let root = tempfile::tempdir().expect("temporary endpoint root");
        assert_eq!(
            endpoint_path(root.path(), "server"),
            root.path().join("server")
        );
    }
}
