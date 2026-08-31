//! Cross-platform local stream socket support.

use socket2::{Domain, SockAddr, Socket, Type};
use std::env;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::sync::OnceLock;

#[cfg(windows)]
static STREAM_SUPPORTED: OnceLock<bool> = OnceLock::new();

#[cfg(all(windows, test))]
pub(crate) static TEST_ENV_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();

/// Whether this runtime can create AF_UNIX stream sockets.
pub fn stream_supported() -> bool {
    #[cfg(unix)]
    {
        true
    }
    #[cfg(windows)]
    {
        *STREAM_SUPPORTED.get_or_init(|| Socket::new(Domain::UNIX, Type::STREAM, None).is_ok())
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Resolve the directory used for a local server socket.
pub fn socket_directory() -> io::Result<PathBuf> {
    let (xdg_runtime_dir, tmp_dir, local_app_data, override_dir) = socket_environment();

    #[cfg(windows)]
    {
        let _ = xdg_runtime_dir;
        return socket_directory_for_with(
            tmp_dir.as_deref(),
            local_app_data.as_deref(),
            override_dir.as_deref(),
        );
    }
    #[cfg(unix)]
    {
        return socket_directory_for_with(
            xdg_runtime_dir.as_deref(),
            tmp_dir.as_deref(),
            override_dir.as_deref(),
        );
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (xdg_runtime_dir, tmp_dir, local_app_data, override_dir);
        Err(invalid_path(
            "local sockets are unsupported on this platform",
        ))
    }
}

/// Prepare the local server socket directory for use by the Lisp server.
pub fn prepare_socket_directory() -> io::Result<PathBuf> {
    let directory = socket_directory()?;
    #[cfg(windows)]
    prepare_windows_socket_directory(&directory)?;
    Ok(directory)
}

/// Prepare the selected local server directory immediately before binding.
pub fn prepare_server_path(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let Some(directory) = windows_server_socket_directory_for_path(path)? else {
            return Ok(());
        };
        prepare_windows_socket_directory(&directory)?;
    }
    Ok(())
}

/// Resolve a local socket name according to the platform socket directory
/// policy, preserving names that already look like paths.
pub fn socket_path_for_name(name: &str) -> io::Result<PathBuf> {
    let (xdg_runtime_dir, tmp_dir, local_app_data, override_dir) = socket_environment();

    socket_path_for_name_with(
        name,
        xdg_runtime_dir.as_deref(),
        tmp_dir.as_deref(),
        local_app_data.as_deref(),
        override_dir.as_deref(),
    )
}

fn socket_environment() -> (
    Option<PathBuf>,
    Option<PathBuf>,
    Option<PathBuf>,
    Option<PathBuf>,
) {
    (
        env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        env::var_os(if cfg!(windows) { "TEMP" } else { "TMPDIR" })
            .map(PathBuf::from)
            .or_else(|| Some(env::temp_dir())),
        env::var_os("LOCALAPPDATA").map(PathBuf::from),
        env::var_os("NEOMACS_SERVER_SOCKET_DIR").map(PathBuf::from),
    )
}

pub(crate) fn socket_path_for_name_with(
    name: &str,
    xdg_runtime_dir: Option<&Path>,
    tmp_dir: Option<&Path>,
    _local_app_data: Option<&Path>,
    override_dir: Option<&Path>,
) -> io::Result<PathBuf> {
    #[cfg(windows)]
    let _ = xdg_runtime_dir;

    if name.contains('\0') {
        return Err(invalid_path("local socket name contains NUL"));
    }

    let path = Path::new(name);
    if path.is_absolute() || name.contains('/') || name.contains('\\') {
        return validate_path(path).map(|_| path.to_path_buf());
    }

    #[cfg(windows)]
    {
        let path = socket_directory_for_with(tmp_dir, _local_app_data, override_dir)?.join(name);
        validate_path(&path)?;
        return Ok(path);
    }

    #[cfg(unix)]
    {
        let directory = socket_directory_for_with(xdg_runtime_dir, tmp_dir, override_dir)?;
        let path = directory.join(name);
        validate_path(&path)?;
        return Ok(path);
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (xdg_runtime_dir, tmp_dir, _local_app_data, override_dir);
        Err(invalid_path(
            "local sockets are unsupported on this platform",
        ))
    }
}

#[cfg(unix)]
pub(crate) fn socket_directory_for_with(
    xdg_runtime_dir: Option<&Path>,
    tmp_dir: Option<&Path>,
    override_dir: Option<&Path>,
) -> io::Result<PathBuf> {
    if let Some(directory) = override_dir.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(directory.to_path_buf());
    }
    if let Some(directory) = xdg_runtime_dir.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(directory.join("emacs"));
    }
    tmp_dir
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.join(format!("emacs{}", unsafe { libc::geteuid() })))
        .ok_or_else(|| invalid_path("no usable local socket directory"))
}

#[cfg(windows)]
pub(crate) fn socket_directory_for_with(
    tmp_dir: Option<&Path>,
    local_app_data: Option<&Path>,
    override_dir: Option<&Path>,
) -> io::Result<PathBuf> {
    windows_socket_directory_candidates(tmp_dir, local_app_data, override_dir)?
        .into_iter()
        .min_by_key(|path| path_len(path))
        .ok_or_else(|| invalid_path("no usable local socket directory"))
}

#[cfg(windows)]
fn windows_server_socket_directory_for_path(path: &Path) -> io::Result<Option<PathBuf>> {
    let parent = path.parent();
    if let Some(explicit) = env::var_os("NEOMACS_SERVER_SOCKET_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return Ok((parent == Some(explicit.as_path())).then_some(explicit));
    }

    let Ok(directory) = socket_directory() else {
        return Ok(None);
    };
    Ok((parent == Some(directory.as_path())).then_some(directory))
}

#[cfg(windows)]
fn windows_socket_directory_candidates(
    tmp_dir: Option<&Path>,
    local_app_data: Option<&Path>,
    override_dir: Option<&Path>,
) -> io::Result<Vec<PathBuf>> {
    if let Some(directory) = override_dir.filter(|path| !path.as_os_str().is_empty()) {
        validate_path(directory)?;
        windows_socket_directory_candidate_is_usable(directory)?;
        return Ok(vec![directory.to_path_buf()]);
    }

    let candidates = [
        tmp_dir
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.join("emacs-server")),
        local_app_data
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.join("emacs").join("server")),
    ];
    let candidates = candidates
        .into_iter()
        .flatten()
        .filter_map(|path| {
            validate_path(&path).ok()?;
            windows_socket_directory_candidate_is_usable(&path)
                .ok()
                .map(|_| path)
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(invalid_path("no usable local socket directory"));
    }
    Ok(candidates)
}

#[cfg(windows)]
fn windows_socket_directory_candidate_is_usable(path: &Path) -> io::Result<()> {
    crate::private_directory::inspect_private_directory(
        path,
        crate::private_directory::PrivateDirectoryPurpose::LocalSocket,
    )
}

/// Build a validated `sockaddr_un` for a local socket path.
pub fn sockaddr_for_path(path: &Path) -> io::Result<SockAddr> {
    validate_path(path)?;
    SockAddr::unix(path).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid sockaddr_un local socket path: {error}"),
        )
    })
}

/// Maximum number of bytes available in `sockaddr_un.sun_path`, including its
/// terminating NUL byte.
pub fn sockaddr_un_path_capacity() -> usize {
    #[cfg(unix)]
    {
        // `sun_path` is the final field; using its offset accounts for BSD's
        // leading `sun_len` instead of assuming the family is first.
        std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path)
    }
    #[cfg(windows)]
    {
        108
    }
    #[cfg(not(any(unix, windows)))]
    {
        0
    }
}

/// Create, bind, and listen on a local stream socket.
pub fn bind_stream_listener(path: &Path, backlog: i32) -> io::Result<Socket> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    let address = sockaddr_for_path(path)?;
    socket.bind(&address)?;
    socket.listen(backlog)?;
    Ok(socket)
}

/// Connect a local stream socket.
pub fn connect_stream(path: &Path) -> io::Result<Socket> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    let address = sockaddr_for_path(path)?;
    socket.connect(&address)?;
    Ok(socket)
}

/// Accept one local stream connection.
pub fn accept_stream(listener: &Socket) -> io::Result<(Socket, SockAddr)> {
    listener.accept()
}

fn validate_path(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let path_bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes()
    };
    #[cfg(windows)]
    let path_bytes = path
        .to_str()
        .ok_or_else(|| invalid_path("local socket path must be valid UTF-8"))?
        .as_bytes();
    #[cfg(not(any(unix, windows)))]
    let path_bytes = path
        .to_str()
        .ok_or_else(|| invalid_path("local socket path must be valid UTF-8"))?
        .as_bytes();

    if path_bytes.contains(&0) {
        return Err(invalid_path("local socket path contains NUL"));
    }
    if path_bytes.len() + 1 > sockaddr_un_path_capacity() {
        return Err(invalid_path(
            "local socket path exceeds sockaddr_un path length",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn path_len(path: &Path) -> usize {
    path.to_str()
        .expect("path length is only computed after validation")
        .len()
}

fn invalid_path(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(windows)]
fn prepare_windows_socket_directory(path: &Path) -> io::Result<()> {
    crate::private_directory::prepare_private_directory(
        path,
        crate::private_directory::PrivateDirectoryPurpose::LocalSocket,
    )
}
