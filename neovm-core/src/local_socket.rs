//! Cross-platform local stream socket support.

use socket2::{Domain, SockAddr, Socket, Type};
use std::env;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
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
    ensure_windows_ntfs(path)?;
    inspect_windows_socket_directory(path).map(|_| ())
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
    ensure_windows_ntfs(path)?;
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot create local socket directory parent: {error}"),
            )
        })?;
    }
    create_windows_socket_directory(path)?;
    validate_windows_socket_directory(path)?;
    Ok(())
}

#[cfg(windows)]
fn inspect_windows_socket_directory(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError,
    };

    match open_windows_socket_directory(path, read_control_access()) {
        Ok(directory) => validate_windows_socket_directory_handle(directory.0),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(code)
                    if code == ERROR_FILE_NOT_FOUND as i32
                        || code == ERROR_PATH_NOT_FOUND as i32
            ) =>
        {
            Ok(())
        }
        Err(error) => {
            let code = error
                .raw_os_error()
                .map(|code| code as u32)
                .unwrap_or_else(|| unsafe { GetLastError() });
            Err(windows_error("cannot inspect local socket directory", code))
        }
    }
}

#[cfg(windows)]
fn ensure_windows_ntfs(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};

    let wide = windows_path(path)?;
    let mut volume_path = vec![0u16; 32_768];
    if unsafe {
        GetVolumePathNameW(
            wide.as_ptr(),
            volume_path.as_mut_ptr(),
            volume_path.len() as u32,
        )
    } == 0
    {
        return Err(windows_error(
            "cannot determine local socket directory volume",
            unsafe { GetLastError() },
        ));
    }

    let mut filesystem_name = [0u16; 32];
    if unsafe {
        GetVolumeInformationW(
            volume_path.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem_name.as_mut_ptr(),
            filesystem_name.len() as u32,
        )
    } == 0
    {
        return Err(windows_error(
            "cannot determine local socket directory filesystem",
            unsafe { GetLastError() },
        ));
    }

    let filesystem_name = String::from_utf16_lossy(
        &filesystem_name[..filesystem_name
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(filesystem_name.len())],
    );
    if !filesystem_name.eq_ignore_ascii_case("NTFS") {
        return Err(invalid_path(
            "local socket directory must reside on an NTFS volume",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn build_windows_socket_directory_acl() -> io::Result<LocalFreeGuard> {
    use windows_sys::Win32::Foundation::GENERIC_ALL;
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
        TRUSTEE_IS_WELL_KNOWN_GROUP,
    };
    use windows_sys::Win32::Security::NO_INHERITANCE;
    use windows_sys::Win32::Security::TOKEN_USER;

    let (_token_guard, token_info) = current_user_sid_storage()?;
    let user_sid = unsafe { (*token_info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut system_sid_storage = system_sid_storage()?;
    let system_sid = system_sid_storage.as_mut_ptr().cast();

    let entries = [
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: windows_sys::Win32::Security::Authorization::TRUSTEE_W {
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: user_sid.cast(),
                ..Default::default()
            },
        },
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: windows_sys::Win32::Security::Authorization::TRUSTEE_W {
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
                ptstrName: system_sid,
                ..Default::default()
            },
        },
    ];

    let mut acl = std::ptr::null_mut();
    let acl_error = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null(),
            &mut acl,
        )
    };
    if acl_error != 0 {
        return Err(windows_error(
            "cannot build local socket directory DACL",
            acl_error,
        ));
    };
    let acl_guard = LocalFreeGuard(acl.cast());

    Ok(acl_guard)
}

#[cfg(windows)]
fn create_windows_socket_directory(path: &Path) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::Security::{
        InitializeSecurityDescriptor, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    let acl_guard = build_windows_socket_directory_acl()?;
    let mut security_descriptor = SECURITY_DESCRIPTOR::default();
    if unsafe {
        InitializeSecurityDescriptor(
            (&mut security_descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            1,
        )
    } == 0
    {
        return Err(windows_error(
            "cannot initialize local socket directory security descriptor",
            unsafe { GetLastError() },
        ));
    }
    if unsafe {
        SetSecurityDescriptorDacl(
            (&mut security_descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            1,
            acl_guard.0.cast(),
            0,
        )
    } == 0
    {
        return Err(windows_error(
            "cannot set local socket directory security descriptor DACL",
            unsafe { GetLastError() },
        ));
    }
    if unsafe {
        SetSecurityDescriptorControl(
            (&mut security_descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        )
    } == 0
    {
        return Err(windows_error(
            "cannot protect local socket directory security descriptor DACL",
            unsafe { GetLastError() },
        ));
    }

    let wide = windows_path(path)?;
    let mut security_attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut security_descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(wide.as_ptr(), &mut security_attributes) } != 0 {
        return Ok(true);
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_ALREADY_EXISTS {
        Ok(false)
    } else {
        Err(windows_error("cannot create local socket directory", error))
    }
}

#[cfg(windows)]
fn validate_windows_socket_directory(path: &Path) -> io::Result<()> {
    ensure_windows_ntfs(path)?;
    let directory =
        open_windows_socket_directory(path, read_control_access()).map_err(|error| {
            windows_error(
                "cannot open local socket directory",
                error.raw_os_error().map(|code| code as u32).unwrap_or(0),
            )
        })?;
    validate_windows_socket_directory_handle(directory.0)
}

#[cfg(windows)]
fn read_control_access() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, READ_CONTROL};

    READ_CONTROL | FILE_READ_ATTRIBUTES
}

#[cfg(windows)]
fn open_windows_socket_directory(path: &Path, access: u32) -> io::Result<HandleGuard> {
    use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide = windows_path(path)?;
    let directory = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if directory == INVALID_HANDLE_VALUE {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ));
    }
    Ok(HandleGuard(directory))
}

#[cfg(windows)]
fn validate_windows_socket_directory_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{GENERIC_ALL, GetLastError};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, SE_DACL_PROTECTED, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
    };

    let mut file_information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle, &mut file_information) } == 0 {
        return Err(windows_error(
            "cannot inspect local socket directory handle",
            unsafe { GetLastError() },
        ));
    }
    if file_information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_path(
            "local socket directory must not be a reparse point",
        ));
    }
    if file_information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(invalid_path(
            "local socket directory handle is not a directory",
        ));
    }

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut security_descriptor = std::ptr::null_mut();
    let security_error = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        )
    };
    if security_error != 0 {
        return Err(windows_error(
            "cannot read local socket directory DACL",
            security_error,
        ));
    }
    if security_descriptor.is_null() {
        return Err(invalid_path(
            "local socket directory security descriptor is missing",
        ));
    }
    let security_descriptor_guard = LocalFreeGuard(security_descriptor.cast());
    let mut control = 0;
    let mut revision = 0;
    if unsafe { GetSecurityDescriptorControl(security_descriptor, &mut control, &mut revision) }
        == 0
    {
        return Err(windows_error(
            "cannot inspect local socket directory DACL protection",
            unsafe { GetLastError() },
        ));
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(invalid_path(
            "local socket directory DACL must be protected",
        ));
    }

    let mut dacl_present = 0;
    let mut dacl_defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(
            security_descriptor,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
    {
        return Err(windows_error(
            "cannot inspect local socket directory DACL",
            unsafe { GetLastError() },
        ));
    }
    if dacl_present == 0 || dacl.is_null() {
        return Err(invalid_path("local socket directory DACL is not present"));
    }

    let mut acl_size = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut acl_size as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(windows_error(
            "cannot inspect local socket directory DACL entries",
            unsafe { GetLastError() },
        ));
    }
    let (_token_guard, token_info) = current_user_sid_storage()?;
    let user_sid = unsafe { (*token_info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let system_sid_storage = system_sid_storage()?;
    let system_sid = system_sid_storage
        .as_ptr()
        .cast::<std::ffi::c_void>()
        .cast_mut();
    let mut saw_user = false;
    let mut saw_system = false;
    for index in 0..acl_size.AceCount {
        let mut ace = std::ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(windows_error(
                "cannot inspect local socket directory DACL ACE",
                unsafe { GetLastError() },
            ));
        }
        let header = unsafe { &*(ace.cast::<windows_sys::Win32::Security::ACE_HEADER>()) };
        if header.AceType != 0 || header.AceFlags != 0 {
            return Err(invalid_path(
                "local socket directory DACL contains an unsupported ACE",
            ));
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        if allowed.Mask & GENERIC_ALL != GENERIC_ALL
            && allowed.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
        {
            return Err(invalid_path(
                "local socket directory DACL ACE lacks full access",
            ));
        }
        let sid = (&allowed.SidStart as *const u32)
            .cast::<std::ffi::c_void>()
            .cast_mut();
        if unsafe { windows_sys::Win32::Security::EqualSid(sid, user_sid) } != 0 {
            saw_user = true;
        } else if unsafe { windows_sys::Win32::Security::EqualSid(sid, system_sid) } != 0 {
            saw_system = true;
        } else {
            return Err(invalid_path(
                "local socket directory DACL contains an unexpected trustee",
            ));
        }
    }
    drop(security_descriptor_guard);
    if !saw_user || !saw_system {
        return Err(invalid_path(
            "local socket directory DACL must grant current user and SYSTEM",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn current_user_sid_storage() -> io::Result<(HandleGuard, Vec<u64>)> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(windows_error(
            "cannot obtain current process token",
            unsafe { GetLastError() },
        ));
    }
    let token_guard = HandleGuard(token);

    let mut token_info_size = 0u32;
    unsafe {
        GetTokenInformation(
            token_guard.0,
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut token_info_size,
        );
    }
    if token_info_size == 0 {
        return Err(windows_error(
            "cannot determine current process user SID size",
            unsafe { GetLastError() },
        ));
    }
    let token_word_count = (token_info_size as usize)
        .checked_add(std::mem::size_of::<u64>() - 1)
        .ok_or_else(|| invalid_path("current process user SID is too large"))?
        / std::mem::size_of::<u64>();
    let mut token_info = vec![0u64; token_word_count];
    if unsafe {
        GetTokenInformation(
            token_guard.0,
            TokenUser,
            token_info.as_mut_ptr().cast(),
            (token_info.len() * std::mem::size_of::<u64>()) as u32,
            &mut token_info_size,
        )
    } == 0
    {
        return Err(windows_error(
            "cannot obtain current process user SID",
            unsafe { GetLastError() },
        ));
    }
    let user_sid = unsafe { (*token_info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    if user_sid.is_null() {
        return Err(invalid_path("current process user SID is unavailable"));
    }
    Ok((token_guard, token_info))
}

#[cfg(windows)]
fn system_sid_storage() -> io::Result<[u8; 68]> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Security::{CreateWellKnownSid, WinLocalSystemSid};

    let mut system_sid_storage = [0u8; 68];
    let mut system_sid_size = system_sid_storage.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            std::ptr::null_mut(),
            system_sid_storage.as_mut_ptr().cast(),
            &mut system_sid_size,
        )
    } == 0
    {
        return Err(windows_error("cannot obtain SYSTEM SID", unsafe {
            GetLastError()
        }));
    }
    Ok(system_sid_storage)
}

#[cfg(windows)]
struct HandleGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
struct LocalFreeGuard(*mut std::ffi::c_void);

#[cfg(windows)]
impl Drop for LocalFreeGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.0);
            }
        }
    }
}

#[cfg(windows)]
fn windows_path(path: &Path) -> io::Result<Vec<u16>> {
    validate_path(path)?;
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    Ok(wide)
}

#[cfg(windows)]
fn windows_error(operation: &str, code: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{operation} (Windows error {code})"),
    )
}
