use std::io;
use std::path::Path;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivateDirectoryPurpose {
    LocalSocket,
    ExecutableCache,
}

pub fn prepare_private_directory(path: &Path, purpose: PrivateDirectoryPurpose) -> io::Result<()> {
    #[cfg(windows)]
    {
        return prepare_windows_private_directory(path, purpose);
    }
    #[cfg(unix)]
    {
        return prepare_unix_private_directory(path, purpose);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, purpose);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private directories are unsupported on this platform",
        ))
    }
}

pub fn validate_private_directory(path: &Path, purpose: PrivateDirectoryPurpose) -> io::Result<()> {
    #[cfg(windows)]
    {
        return validate_windows_private_directory(path, purpose);
    }
    #[cfg(unix)]
    {
        return validate_unix_private_directory(path, purpose);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, purpose);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private directories are unsupported on this platform",
        ))
    }
}

#[cfg(unix)]
fn prepare_unix_private_directory(path: &Path, purpose: PrivateDirectoryPurpose) -> io::Result<()> {
    if purpose == PrivateDirectoryPurpose::LocalSocket {
        return Ok(());
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(invalid_path(
                    "executable cache directory is not a directory",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(path)
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("cannot create executable cache directory: {error}"),
                    )
                })?;
        }
        Err(error) => return Err(error),
    }

    validate_unix_private_directory(path, purpose)
}

#[cfg(unix)]
fn validate_unix_private_directory(
    path: &Path,
    purpose: PrivateDirectoryPurpose,
) -> io::Result<()> {
    if purpose == PrivateDirectoryPurpose::LocalSocket {
        return Ok(());
    }

    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(invalid_path(
            "executable cache directory is not a directory",
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(invalid_path(
            "executable cache directory is not owned by the current user",
        ));
    }
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(invalid_path(
            "executable cache directory mode must be 0o700",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn prepare_windows_private_directory(
    path: &Path,
    purpose: PrivateDirectoryPurpose,
) -> io::Result<()> {
    ensure_windows_ntfs(path, purpose)?;
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot create {} directory parent: {error}",
                    directory_name(purpose)
                ),
            )
        })?;
    }
    create_windows_private_directory(path, purpose)?;
    validate_windows_private_directory(path, purpose)?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn inspect_private_directory(
    path: &Path,
    purpose: PrivateDirectoryPurpose,
) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError,
    };

    ensure_windows_ntfs(path, purpose)?;
    match open_windows_private_directory(path, read_control_access(), purpose) {
        Ok(directory) => validate_windows_private_directory_handle(directory.0, purpose),
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
            Err(windows_error(
                &format!("cannot inspect {} directory", directory_name(purpose)),
                code,
            ))
        }
    }
}

#[cfg(windows)]
fn ensure_windows_ntfs(path: &Path, purpose: PrivateDirectoryPurpose) -> io::Result<()> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};

    let wide = windows_path(path, purpose)?;
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
            &format!(
                "cannot determine {} directory volume",
                directory_name(purpose)
            ),
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
            &format!(
                "cannot determine {} directory filesystem",
                directory_name(purpose)
            ),
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
        return Err(invalid_path(&format!(
            "{} directory must reside on an NTFS volume",
            directory_name(purpose)
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn build_windows_private_directory_acl(
    purpose: PrivateDirectoryPurpose,
) -> io::Result<LocalFreeGuard> {
    use windows_sys::Win32::Foundation::GENERIC_ALL;
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
        TRUSTEE_IS_WELL_KNOWN_GROUP,
    };
    use windows_sys::Win32::Security::NO_INHERITANCE;
    use windows_sys::Win32::Security::TOKEN_USER;

    let (_token_guard, token_info) = current_user_sid_storage(purpose)?;
    let user_sid = unsafe { (*token_info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut system_sid_storage = system_sid_storage(purpose)?;
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
            &format!("cannot build {} directory DACL", directory_name(purpose)),
            acl_error,
        ));
    };
    let acl_guard = LocalFreeGuard(acl.cast());

    Ok(acl_guard)
}

#[cfg(windows)]
fn create_windows_private_directory(
    path: &Path,
    purpose: PrivateDirectoryPurpose,
) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::Security::{
        InitializeSecurityDescriptor, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    let acl_guard = build_windows_private_directory_acl(purpose)?;
    let mut security_descriptor = SECURITY_DESCRIPTOR::default();
    if unsafe {
        InitializeSecurityDescriptor(
            (&mut security_descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            1,
        )
    } == 0
    {
        return Err(windows_error(
            &format!(
                "cannot initialize {} directory security descriptor",
                directory_name(purpose)
            ),
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
            &format!(
                "cannot set {} directory security descriptor DACL",
                directory_name(purpose)
            ),
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
            &format!(
                "cannot protect {} directory security descriptor DACL",
                directory_name(purpose)
            ),
            unsafe { GetLastError() },
        ));
    }

    let wide = windows_path(path, purpose)?;
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
        Err(windows_error(
            &format!("cannot create {} directory", directory_name(purpose)),
            error,
        ))
    }
}

#[cfg(windows)]
fn validate_windows_private_directory(
    path: &Path,
    purpose: PrivateDirectoryPurpose,
) -> io::Result<()> {
    ensure_windows_ntfs(path, purpose)?;
    let directory =
        open_windows_private_directory(path, read_control_access(), purpose).map_err(|error| {
            windows_error(
                &format!("cannot open {} directory", directory_name(purpose)),
                error.raw_os_error().map(|code| code as u32).unwrap_or(0),
            )
        })?;
    validate_windows_private_directory_handle(directory.0, purpose)
}

#[cfg(windows)]
fn read_control_access() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, READ_CONTROL};

    READ_CONTROL | FILE_READ_ATTRIBUTES
}

#[cfg(windows)]
fn open_windows_private_directory(
    path: &Path,
    access: u32,
    purpose: PrivateDirectoryPurpose,
) -> io::Result<HandleGuard> {
    use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide = windows_path(path, purpose)?;
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
fn validate_windows_private_directory_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
    purpose: PrivateDirectoryPurpose,
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
            &format!(
                "cannot inspect {} directory handle",
                directory_name(purpose)
            ),
            unsafe { GetLastError() },
        ));
    }
    if file_information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_path(&format!(
            "{} directory must not be a reparse point",
            directory_name(purpose)
        )));
    }
    if file_information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(invalid_path(&format!(
            "{} directory handle is not a directory",
            directory_name(purpose)
        )));
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
            &format!("cannot read {} directory DACL", directory_name(purpose)),
            security_error,
        ));
    }
    if security_descriptor.is_null() {
        return Err(invalid_path(&format!(
            "{} directory security descriptor is missing",
            directory_name(purpose)
        )));
    }
    let security_descriptor_guard = LocalFreeGuard(security_descriptor.cast());
    let mut control = 0;
    let mut revision = 0;
    if unsafe { GetSecurityDescriptorControl(security_descriptor, &mut control, &mut revision) }
        == 0
    {
        return Err(windows_error(
            &format!(
                "cannot inspect {} directory DACL protection",
                directory_name(purpose)
            ),
            unsafe { GetLastError() },
        ));
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(invalid_path(&format!(
            "{} directory DACL must be protected",
            directory_name(purpose)
        )));
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
            &format!("cannot inspect {} directory DACL", directory_name(purpose)),
            unsafe { GetLastError() },
        ));
    }
    if dacl_present == 0 || dacl.is_null() {
        return Err(invalid_path(&format!(
            "{} directory DACL is not present",
            directory_name(purpose)
        )));
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
            &format!(
                "cannot inspect {} directory DACL entries",
                directory_name(purpose)
            ),
            unsafe { GetLastError() },
        ));
    }
    let (_token_guard, token_info) = current_user_sid_storage(purpose)?;
    let user_sid = unsafe { (*token_info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let system_sid_storage = system_sid_storage(purpose)?;
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
                &format!(
                    "cannot inspect {} directory DACL ACE",
                    directory_name(purpose)
                ),
                unsafe { GetLastError() },
            ));
        }
        let header = unsafe { &*(ace.cast::<windows_sys::Win32::Security::ACE_HEADER>()) };
        if header.AceType != 0 || header.AceFlags != 0 {
            return Err(invalid_path(&format!(
                "{} directory DACL contains an unsupported ACE",
                directory_name(purpose)
            )));
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        if allowed.Mask & GENERIC_ALL != GENERIC_ALL
            && allowed.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
        {
            return Err(invalid_path(&format!(
                "{} directory DACL ACE lacks full access",
                directory_name(purpose)
            )));
        }
        let sid = (&allowed.SidStart as *const u32)
            .cast::<std::ffi::c_void>()
            .cast_mut();
        if unsafe { windows_sys::Win32::Security::EqualSid(sid, user_sid) } != 0 {
            saw_user = true;
        } else if unsafe { windows_sys::Win32::Security::EqualSid(sid, system_sid) } != 0 {
            saw_system = true;
        } else {
            return Err(invalid_path(&format!(
                "{} directory DACL contains an unexpected trustee",
                directory_name(purpose)
            )));
        }
    }
    drop(security_descriptor_guard);
    if !saw_user || !saw_system {
        return Err(invalid_path(&format!(
            "{} directory DACL must grant current user and SYSTEM",
            directory_name(purpose)
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn current_user_sid_storage(
    purpose: PrivateDirectoryPurpose,
) -> io::Result<(HandleGuard, Vec<u64>)> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(windows_error(
            if purpose == PrivateDirectoryPurpose::LocalSocket {
                "cannot obtain current process token".to_owned()
            } else {
                format!("cannot obtain {} process token", directory_name(purpose))
            },
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
            if purpose == PrivateDirectoryPurpose::LocalSocket {
                "cannot determine current process user SID size".to_owned()
            } else {
                format!(
                    "cannot determine {} process user SID size",
                    directory_name(purpose)
                )
            },
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
            if purpose == PrivateDirectoryPurpose::LocalSocket {
                "cannot obtain current process user SID".to_owned()
            } else {
                format!("cannot obtain {} process user SID", directory_name(purpose))
            },
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
fn system_sid_storage(purpose: PrivateDirectoryPurpose) -> io::Result<[u8; 68]> {
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
        return Err(windows_error(
            if purpose == PrivateDirectoryPurpose::LocalSocket {
                "cannot obtain SYSTEM SID".to_owned()
            } else {
                format!("cannot obtain {} SYSTEM SID", directory_name(purpose))
            },
            unsafe { GetLastError() },
        ));
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
fn windows_path(path: &Path, purpose: PrivateDirectoryPurpose) -> io::Result<Vec<u16>> {
    let path = path.to_str().ok_or_else(|| {
        invalid_path(&format!(
            "{}{}",
            if purpose == PrivateDirectoryPurpose::LocalSocket {
                "local socket path"
            } else {
                "executable cache directory path"
            },
            " must be valid UTF-8"
        ))
    })?;
    if path.contains('\0') {
        return Err(invalid_path(&format!(
            "{} path contains NUL",
            if purpose == PrivateDirectoryPurpose::LocalSocket {
                "local socket"
            } else {
                "executable cache directory"
            }
        )));
    }
    if purpose == PrivateDirectoryPurpose::LocalSocket && path.len() + 1 > 108 {
        return Err(invalid_path(
            "local socket path exceeds sockaddr_un path length",
        ));
    }
    let mut wide = std::ffi::OsStr::new(path).encode_wide().collect::<Vec<_>>();
    wide.push(0);
    Ok(wide)
}

#[cfg(windows)]
fn directory_name(purpose: PrivateDirectoryPurpose) -> &'static str {
    match purpose {
        PrivateDirectoryPurpose::LocalSocket => "local socket",
        PrivateDirectoryPurpose::ExecutableCache => "executable cache",
    }
}

fn invalid_path(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(windows)]
fn windows_error(operation: impl AsRef<str>, code: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} (Windows error {code})", operation.as_ref()),
    )
}

#[cfg(test)]
#[path = "private_directory_test.rs"]
mod tests;
