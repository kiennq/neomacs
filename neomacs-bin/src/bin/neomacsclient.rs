use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Stdio;
use std::process::{self, Command};
use std::time::Duration;

use neovm_core::local_socket::{connect_stream, socket_path_for_name, stream_supported};

const EMACS_VERSION: &str = "31.0.50";

#[derive(Debug, Default)]
struct Options {
    nowait: bool,
    quiet: bool,
    suppress_output: bool,
    eval: bool,
    create_frame: bool,
    tty: bool,
    reuse_frame: bool,
    socket_name: Option<String>,
    server_file: Option<String>,
    alternate_editor: Option<String>,
    timeout: Option<Duration>,
    tramp_prefix: Option<String>,
    display: Option<String>,
    parent_id: Option<String>,
    frame_parameters: Option<String>,
    args: Vec<String>,
}

fn main() {
    let code = match run(env::args_os().collect()) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("*ERROR*: {err}");
            1
        }
    };
    process::exit(code);
}

fn run(argv: Vec<OsString>) -> Result<(), String> {
    let prog = argv
        .first()
        .and_then(|arg| arg.to_str())
        .unwrap_or("neomacsclient")
        .to_string();
    let options = parse_options(&prog, argv.into_iter().skip(1))?;

    if options.tty {
        return Err(format!(
            "{prog}: terminal client frames are not implemented"
        ));
    }
    if !(options.eval || options.create_frame || !options.args.is_empty()) {
        return Err(format!(
            "{prog}: file name or argument required\nTry '{prog} --help' for more information"
        ));
    }

    run_client(&prog, options)
}

fn parse_options(prog: &str, args: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut options = Options::default();
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            options.args.extend(args[i + 1..].iter().cloned());
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            options.args.push(arg.clone());
            i += 1;
            continue;
        }

        match arg.as_str() {
            "-n" | "--no-wait" => options.nowait = true,
            "-q" | "--quiet" => options.quiet = true,
            "-u" | "--suppress-output" => options.suppress_output = true,
            "-e" | "--eval" => options.eval = true,
            "-V" | "--version" => {
                println!("neomacsclient {EMACS_VERSION}");
                process::exit(0);
            }
            "-H" | "--help" => {
                print_help(prog);
                process::exit(0);
            }
            "-t" | "-nw" | "--tty" | "--nw" | "--no-window-system" => {
                options.create_frame = true;
                options.tty = true;
            }
            "-c" | "--create-frame" => options.create_frame = true,
            "-r" | "--reuse-frame" => {
                options.create_frame = true;
                options.reuse_frame = true;
            }
            _ => {
                if let Some(value) = option_value(arg, "--socket-name", "-s", &args, &mut i)? {
                    options.socket_name = Some(value);
                } else if let Some(value) = option_value(arg, "--server-file", "-f", &args, &mut i)?
                {
                    options.server_file = Some(value);
                } else if let Some(value) =
                    option_value(arg, "--alternate-editor", "-a", &args, &mut i)?
                {
                    options.alternate_editor = Some(value);
                } else if let Some(value) = option_value(arg, "--timeout", "-w", &args, &mut i)? {
                    let seconds = value
                        .parse::<u64>()
                        .map_err(|_| format!("Invalid timeout: \"{value}\""))?;
                    options.timeout = Some(Duration::from_secs(seconds));
                } else if let Some(value) = option_value(arg, "--tramp", "-T", &args, &mut i)? {
                    options.tramp_prefix = Some(value);
                } else if let Some(value) = option_value(arg, "--display", "-d", &args, &mut i)? {
                    options.display = Some(value);
                } else if let Some(value) = option_value(arg, "--parent-id", "", &args, &mut i)? {
                    options.parent_id = Some(value);
                } else if let Some(value) =
                    option_value(arg, "--frame-parameters", "-F", &args, &mut i)?
                {
                    options.frame_parameters = Some(value);
                } else {
                    return Err(format!(
                        "{prog}: unrecognized option '{arg}'\nTry '{prog} --help' for more information"
                    ));
                }
            }
        }
        i += 1;
    }

    Ok(options)
}

fn option_value(
    arg: &str,
    long: &str,
    short: &str,
    args: &[String],
    index: &mut usize,
) -> Result<Option<String>, String> {
    if !long.is_empty() {
        if arg == long {
            *index += 1;
            return args
                .get(*index)
                .cloned()
                .map(Some)
                .ok_or_else(|| format!("{long} requires an argument"));
        }
        if let Some(value) = arg.strip_prefix(&format!("{long}=")) {
            return Ok(Some(value.to_string()));
        }
    }

    if !short.is_empty() && arg == short {
        *index += 1;
        return args
            .get(*index)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("{short} requires an argument"));
    }

    Ok(None)
}

fn print_help(prog: &str) {
    println!(
        "\
Usage: {prog} [OPTIONS] FILE...
Tell a Neomacs server to visit files or evaluate forms.

Options:
  -V, --version              Print version info and return
  -H, --help                 Print this help
  -n, --no-wait              Do not wait for the server to return
  -e, --eval                 Treat FILE arguments as Elisp expressions
  -q, --quiet                Do not display success messages
  -u, --suppress-output      Do not display return values
  -s, --socket-name SOCKET   Use a local Unix server socket
-f, --server-file FILE     Use a TCP authentication file
  -a, --alternate-editor CMD Run CMD if the server is not available
  -w, --timeout SECONDS      Wait this many seconds for server replies
  -T, --tramp PREFIX         Prefix absolute file names for Tramp
"
    );
}

#[derive(Debug, PartialEq, Eq)]
enum ServerTarget {
    Local(PathBuf),
    Tcp(PathBuf),
}

enum ClientConnectError {
    Connection(String),
    Other(String),
}

impl ClientConnectError {
    fn message(&self) -> &str {
        match self {
            Self::Connection(message) | Self::Other(message) => message,
        }
    }
}

fn run_client(prog: &str, options: Options) -> Result<(), String> {
    if options.tty {
        return Err(format!(
            "{prog}: terminal client frames are not implemented"
        ));
    }

    match try_client(prog, &options) {
        Ok(()) => Ok(()),
        Err(ClientConnectError::Connection(_message))
            if options.alternate_editor.as_deref() == Some("") =>
        {
            start_daemon_and_retry(prog, options)
        }
        Err(error @ ClientConnectError::Connection(_)) => {
            fail_or_alternate(prog, &options, error.message())
        }
        Err(ClientConnectError::Other(message)) => Err(format!("{prog}: {message}")),
    }
}

fn resolve_server_target(options: &Options) -> Result<ServerTarget, String> {
    resolve_server_target_with(options, stream_supported())
}

fn resolve_server_target_with(
    options: &Options,
    local_supported: bool,
) -> Result<ServerTarget, String> {
    if let Some(server_file) = selected_server_file(options) {
        return Ok(ServerTarget::Tcp(PathBuf::from(server_file)));
    }

    let name = selected_socket_name(options);
    if local_supported {
        return match socket_path_for_name(&name) {
            Ok(path) => Ok(ServerTarget::Local(path)),
            Err(error)
                if cfg!(windows)
                    && !is_path_like_socket_name(&name)
                    && error.to_string() == "no usable local socket directory" =>
            {
                Ok(ServerTarget::Tcp(PathBuf::from(name)))
            }
            Err(error) => Err(format!(
                "cannot resolve local socket path for {name}: {error}"
            )),
        };
    }

    Ok(ServerTarget::Tcp(PathBuf::from(name)))
}

fn is_path_like_socket_name(name: &str) -> bool {
    let path = Path::new(name);
    path.is_absolute() || name.contains('/') || name.contains('\\')
}

fn selected_server_file(options: &Options) -> Option<String> {
    options
        .server_file
        .clone()
        .or_else(|| env::var("EMACS_SERVER_FILE").ok())
}

fn selected_socket_name(options: &Options) -> String {
    options
        .socket_name
        .clone()
        .or_else(|| env::var("EMACS_SOCKET_NAME").ok())
        .unwrap_or_else(|| "server".to_string())
}

fn try_client(_prog: &str, options: &Options) -> Result<(), ClientConnectError> {
    let target = resolve_server_target(options).map_err(ClientConnectError::Connection)?;
    match target {
        ServerTarget::Local(socket) => run_local_client(options, &socket),
        ServerTarget::Tcp(server_file) => run_tcp_client(options, &server_file),
    }
}

fn run_local_client(options: &Options, socket: &Path) -> Result<(), ClientConnectError> {
    let mut stream = match connect_stream(socket) {
        Ok(stream) => stream,
        Err(err) => {
            return Err(ClientConnectError::Connection(format!(
                "can't connect to {}: {err}",
                socket.display()
            )));
        }
    };
    if let Some(timeout) = options.timeout {
        stream.set_read_timeout(Some(timeout)).map_err(|err| {
            ClientConnectError::Other(format!("failed to set socket timeout: {err}"))
        })?;
    }

    let request = build_request(options).map_err(ClientConnectError::Other)?;
    stream.write_all(request.as_bytes()).map_err(|err| {
        ClientConnectError::Other(format!("failed to send request to server: {err}"))
    })?;

    read_responses(&mut stream, options).map_err(ClientConnectError::Other)
}

fn run_tcp_client(options: &Options, server_file: &Path) -> Result<(), ClientConnectError> {
    let server_file = server_file.to_string_lossy();
    let config = match read_tcp_server_config(&server_file) {
        Ok(config) => config,
        Err(err) => return Err(ClientConnectError::Connection(err)),
    };
    let mut stream = match TcpStream::connect((&*config.host, config.port)) {
        Ok(stream) => stream,
        Err(err) => {
            return Err(ClientConnectError::Connection(format!(
                "can't connect to {}:{}: {err}",
                config.host, config.port
            )));
        }
    };
    if let Some(timeout) = options.timeout {
        stream.set_read_timeout(Some(timeout)).map_err(|err| {
            ClientConnectError::Other(format!("failed to set socket timeout: {err}"))
        })?;
    }

    let mut request = String::new();
    push_arg_command(&mut request, "-auth", &config.auth_key);
    request.push_str(&build_request(options).map_err(ClientConnectError::Other)?);
    stream.write_all(request.as_bytes()).map_err(|err| {
        ClientConnectError::Other(format!("failed to send request to server: {err}"))
    })?;

    read_responses(&mut stream, options).map_err(ClientConnectError::Other)
}

struct TcpServerConfig {
    host: String,
    port: u16,
    auth_key: String,
}

fn read_tcp_server_config(server_file: &str) -> Result<TcpServerConfig, String> {
    let path = resolve_tcp_server_file(server_file)
        .ok_or_else(|| format!("can't find server file: {server_file}"))?;
    let contents = fs::read_to_string(&path)
        .map_err(|err| format!("cannot read server file {}: {err}", path.display()))?;
    let mut lines = contents.lines();
    let endpoint = lines
        .next()
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| format!("invalid server file: {}", path.display()))?;
    let (host, port) = endpoint
        .rsplit_once(':')
        .ok_or_else(|| format!("invalid server address in {}", path.display()))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| format!("invalid server port in {}", path.display()))?;
    let auth_key = lines
        .next()
        .ok_or_else(|| format!("cannot read authentication info from {}", path.display()))?
        .trim_end_matches(['\r', '\n'])
        .to_string();
    if auth_key.is_empty() {
        return Err(format!(
            "empty authentication info in server file {}",
            path.display()
        ));
    }

    Ok(TcpServerConfig {
        host: host.to_string(),
        port,
        auth_key,
    })
}

fn resolve_tcp_server_file(server_file: &str) -> Option<PathBuf> {
    let path = Path::new(server_file);
    if path.is_absolute() {
        return path.exists().then(|| path.to_path_buf());
    }

    tcp_server_file_candidates(server_file)
        .into_iter()
        .find(|candidate| candidate.exists())
}

fn tcp_server_file_candidates(name: &str) -> Vec<PathBuf> {
    let home = effective_home();
    let xdg = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    tcp_server_file_candidates_from_paths(name, home.as_deref(), xdg.as_deref())
}

fn tcp_server_file_candidates_from_paths(
    name: &str,
    home: Option<&Path>,
    xdg: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(3);

    if let Some(home) = home {
        candidates.push(home.join(".emacs.d").join("server").join(name));
    }
    if let Some(xdg) = xdg {
        candidates.push(xdg.join("emacs").join("server").join(name));
    }
    if let Some(home) = home {
        candidates.push(home.join(".config").join("emacs").join("server").join(name));
    }

    candidates
}

fn effective_home() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("APPDATA").filter(|value| !value.is_empty()))
        .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
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

fn build_request(options: &Options) -> Result<String, String> {
    let mut request = String::new();
    let cwd = env::current_dir().map_err(|err| format!("cannot get current directory: {err}"))?;
    let mut cwd = cwd.to_string_lossy().into_owned();
    if !cwd.ends_with('/') {
        cwd.push('/');
    }
    let display = effective_display(options);

    push_command(&mut request, "-dir");
    if let Some(prefix) = &options.tramp_prefix {
        request.push_str(&quote_argument(prefix));
    }
    request.push_str(&quote_argument(&cwd));
    request.push(' ');

    if options.nowait {
        push_flag(&mut request, "-nowait");
    }
    if !options.create_frame || options.reuse_frame {
        push_flag(&mut request, "-current-frame");
    }
    if let Some(display) = &display {
        push_arg_command(&mut request, "-display", display);
    }
    if let Some(parent_id) = &options.parent_id {
        push_arg_command(&mut request, "-parent-id", parent_id);
    }
    if let Some(frame_parameters) = &options.frame_parameters {
        push_arg_command(&mut request, "-frame-parameters", frame_parameters);
    }
    if options.create_frame && !options.tty {
        push_flag(&mut request, "-window-system");
    }

    if options.eval {
        if options.args.is_empty() {
            let mut input = String::new();
            io::stdin()
                .read_to_string(&mut input)
                .map_err(|err| format!("failed to read stdin: {err}"))?;
            for line in input.lines() {
                push_arg_command(&mut request, "-eval", line);
            }
        } else {
            for arg in &options.args {
                push_arg_command(&mut request, "-eval", arg);
            }
        }
    } else {
        for arg in &options.args {
            if is_position_arg(arg) {
                push_arg_command(&mut request, "-position", arg);
            } else {
                push_command(&mut request, "-file");
                if let Some(prefix) = &options.tramp_prefix
                    && Path::new(arg).is_absolute()
                {
                    request.push_str(&quote_argument(prefix));
                }
                request.push_str(&quote_argument(arg));
                request.push(' ');
            }
        }
    }

    request.push('\n');
    Ok(request)
}

fn effective_display(options: &Options) -> Option<String> {
    if let Some(display) = options
        .display
        .as_ref()
        .filter(|display| !display.is_empty())
    {
        return Some(display.clone());
    }
    if options.create_frame && !options.tty {
        return env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|display| !display.is_empty())
            .or_else(|| {
                env::var("DISPLAY")
                    .ok()
                    .filter(|display| !display.is_empty())
            })
            .or_else(|| Some("neomacs".to_string()));
    }
    None
}

fn push_flag(request: &mut String, flag: &str) {
    request.push_str(flag);
    request.push(' ');
}

fn push_command(request: &mut String, command: &str) {
    request.push_str(command);
    request.push(' ');
}

fn push_arg_command(request: &mut String, command: &str, arg: &str) {
    push_command(request, command);
    request.push_str(&quote_argument(arg));
    request.push(' ');
}

fn is_position_arg(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('+') else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == ':')
}

fn quote_argument(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() * 2);
    if arg.starts_with('-') {
        quoted.push('&');
    }
    for ch in arg.chars() {
        match ch {
            ' ' => quoted.push_str("&_"),
            '\n' => quoted.push_str("&n"),
            '&' => quoted.push_str("&&"),
            _ => quoted.push(ch),
        }
    }
    quoted
}

fn unquote_argument(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len());
    let mut chars = arg.chars();
    while let Some(ch) = chars.next() {
        if ch == '&' {
            match chars.next() {
                Some('_') => out.push(' '),
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('&'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn read_responses(stream: &mut impl Read, options: &Options) -> Result<(), String> {
    let mut buffer = Vec::new();
    stream
        .read_to_end(&mut buffer)
        .map_err(|err| format!("failed to read server response: {err}"))?;
    let text = String::from_utf8_lossy(&buffer);
    let mut ok = true;

    for line in text.lines() {
        if let Some(value) = line.strip_prefix("-print ") {
            if !options.suppress_output {
                print!("{}", unquote_argument(value));
            }
        } else if let Some(value) = line.strip_prefix("-print-nonl ") {
            if !options.suppress_output {
                print!("{}", unquote_argument(value));
            }
        } else if let Some(value) = line.strip_prefix("-error ") {
            eprintln!("*ERROR*: {}", unquote_argument(value));
            ok = false;
        } else if line.trim_end() == "-window-system-unsupported" {
            return Err("server does not support creating a window-system frame".to_string());
        }
    }

    if ok {
        Ok(())
    } else {
        Err("server reported an error".to_string())
    }
}

fn selected_server_name(options: &Options) -> String {
    if let Some(server_file) = selected_server_file(options) {
        if let Some(name) = Path::new(&server_file)
            .file_name()
            .filter(|name| !name.is_empty())
        {
            return name.to_string_lossy().into_owned();
        }
        return server_file;
    }

    selected_socket_name(options)
}

fn elisp_string_literal(value: &str) -> String {
    use std::fmt::Write as _;

    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => literal.push_str(r"\\"),
            '"' => literal.push_str(r#"\""#),
            '\n' => literal.push_str(r"\n"),
            '\r' => literal.push_str(r"\r"),
            '\t' => literal.push_str(r"\t"),
            '\x08' => literal.push_str(r"\b"),
            '\x0c' => literal.push_str(r"\f"),
            ch if ch.is_control() => {
                write!(literal, r"\u{:04X}", ch as u32).expect("writing to String cannot fail");
            }
            ch => literal.push(ch),
        }
    }
    literal.push('"');
    literal
}

fn daemon_arguments(options: &Options) -> Result<Vec<OsString>, String> {
    daemon_arguments_with(options, stream_supported())
}

fn daemon_arguments_with(
    options: &Options,
    local_supported: bool,
) -> Result<Vec<OsString>, String> {
    let daemon_name = selected_server_name(options);
    let daemon_argument = OsString::from(format!("--daemon={daemon_name}"));

    if let Some(server_file) = selected_server_file(options) {
        let server_file = daemon_server_file(&server_file)?;
        let parent = server_file.parent().unwrap_or_else(|| Path::new("."));
        let config = format!(
            "(setq server-use-tcp t server-auth-dir {})",
            elisp_string_literal(&parent.to_string_lossy())
        );
        return Ok(vec![
            OsString::from("--eval"),
            OsString::from(config),
            daemon_argument,
        ]);
    }

    if cfg!(windows) && !local_supported {
        let server_file = daemon_server_file(&selected_server_name(options))?;
        let parent = server_file.parent().unwrap_or_else(|| Path::new("."));
        let config = format!(
            "(setq server-use-tcp t server-auth-dir {})",
            elisp_string_literal(&parent.to_string_lossy())
        );
        return Ok(vec![
            OsString::from("--eval"),
            OsString::from(config),
            daemon_argument,
        ]);
    }

    Ok(vec![daemon_argument])
}

fn daemon_server_file(server_file: &str) -> Result<PathBuf, String> {
    let path = Path::new(server_file);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    let mut candidates = tcp_server_file_candidates(server_file);
    if let Some(existing) = candidates.iter().find(|candidate| candidate.exists()) {
        return Ok(existing.clone());
    }

    candidates
        .drain(..)
        .next()
        .ok_or_else(|| format!("cannot determine an authentication directory for {server_file}"))
}

fn find_neomacs_executable() -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    let executable_names: &[&str] = if cfg!(windows) {
        &["neomacs.exe", "neomacs"]
    } else {
        &["neomacs"]
    };
    for variable in ["NEOMACS", "EMACS"] {
        if let Some(path) = env::var_os(variable).filter(|path| !path.is_empty()) {
            candidates.push(PathBuf::from(path));
        }
    }

    if let Ok(current_exe) = env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        candidates.extend(executable_names.iter().map(|name| parent.join(name)));
    }

    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).flat_map(|directory| {
            executable_names
                .iter()
                .map(move |name| directory.join(name))
        }));
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| {
            "could not find neomacs executable (checked NEOMACS, EMACS, the sibling executable, and PATH)"
                .to_string()
        })
}

fn start_daemon_and_retry(prog: &str, options: Options) -> Result<(), String> {
    let executable = find_neomacs_executable().map_err(|err| format!("{prog}: {err}"))?;
    start_daemon_and_retry_with_runner(prog, options, &executable, |executable, args| {
        #[cfg(windows)]
        clear_standard_handle_inheritance()?;
        let mut command = Command::new(executable);
        command.args(args);
        #[cfg(windows)]
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = command
            .status()
            .map_err(|err| format!("failed to launch daemon: {err}"))?;
        Ok(status.success())
    })
}

#[cfg(windows)]
fn clear_standard_handle_inheritance() -> Result<(), String> {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { GetStdHandle(kind) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(format!(
                "failed to detach standard handle inheritance: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn start_daemon_and_retry_with_runner<F>(
    prog: &str,
    options: Options,
    executable: &Path,
    run_daemon: F,
) -> Result<(), String>
where
    F: FnOnce(&Path, &[OsString]) -> Result<bool, String>,
{
    let daemon_arguments = daemon_arguments(&options)?;
    let launch_result = run_daemon(executable, &daemon_arguments);

    match try_client(prog, &options) {
        Ok(()) => Ok(()),
        Err(retry_error) => match launch_result {
            Ok(true) => Err(format!("{prog}: {}", retry_error.message())),
            Ok(false) => Err(format!(
                "{prog}: daemon command exited unsuccessfully; {}",
                retry_error.message()
            )),
            Err(launch_error) => Err(format!("{prog}: {launch_error}; {}", retry_error.message())),
        },
    }
}

fn fail_or_alternate(prog: &str, options: &Options, message: &str) -> Result<(), String> {
    let Some(alternate) = &options.alternate_editor else {
        return Err(format!("{prog}: {message}"));
    };
    if alternate.is_empty() {
        return Err(format!("{prog}: {message}"));
    }

    #[cfg(unix)]
    let (shell, shell_arg) = ("sh", "-c");
    #[cfg(windows)]
    let (shell, shell_arg) = ("cmd", "/C");

    let status = Command::new(shell)
        .arg(shell_arg)
        .arg(alternate)
        .args(&options.args)
        .status()
        .map_err(|err| format!("{prog}: failed to run alternate editor: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{prog}: alternate editor exited with {status}"))
    }
}
