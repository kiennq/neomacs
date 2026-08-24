use super::*;
use neovm_host_abi::ChannelId;

#[cfg(windows)]
use std::sync::Mutex;

/// Minimal in-crate TaskScheduler implementation proving the trait
/// contract is implementable and its surface behaves as documented
/// (neovm-worker holds the production implementation).
#[derive(Default)]
struct MockScheduler;

impl TaskScheduler for MockScheduler {
    fn spawn_task(&self, _form: LispValue, _opts: TaskOptions) -> Result<TaskHandle, Signal> {
        Ok(TaskHandle(42))
    }

    fn task_cancel(&self, handle: TaskHandle) -> bool {
        handle.0 == 42
    }

    fn task_status(&self, handle: TaskHandle) -> Option<TaskStatus> {
        if handle.0 == 42 {
            Some(TaskStatus::Completed)
        } else {
            None
        }
    }

    fn task_await(
        &self,
        handle: TaskHandle,
        _timeout: Option<Duration>,
    ) -> Result<LispValue, TaskError> {
        if handle.0 == 42 {
            Ok(LispValue {
                bytes: vec![1, 2, 3],
            })
        } else {
            Err(TaskError::TimedOut)
        }
    }

    fn select(&self, _ops: &[SelectOp], _timeout: Option<Duration>) -> SelectResult {
        SelectResult::Ready {
            op_index: 0,
            value: Some(LispValue { bytes: vec![9] }),
        }
    }
}

#[test]
fn task_scheduler_trait_contract() {
    crate::test_utils::init_test_tracing();
    let sched = MockScheduler;
    let handle = sched
        .spawn_task(LispValue::default(), TaskOptions::default())
        .expect("spawn should succeed");
    assert_eq!(handle, TaskHandle(42));
    assert_eq!(sched.task_status(handle), Some(TaskStatus::Completed));
    assert_eq!(sched.task_status(TaskHandle(7)), None);
    assert!(sched.task_cancel(handle));
    assert!(!sched.task_cancel(TaskHandle(7)));
    assert_eq!(
        sched
            .task_await(handle, Some(Duration::from_millis(10)))
            .expect("await should return result")
            .bytes,
        vec![1, 2, 3]
    );
    assert!(matches!(
        sched.task_await(TaskHandle(7), None).unwrap_err(),
        TaskError::TimedOut
    ));
    assert!(matches!(
        sched.select(&[SelectOp::Recv(ChannelId(1))], None),
        SelectResult::Ready { op_index: 0, .. }
    ));
}

#[test]
fn task_handle_eq_hash() {
    crate::test_utils::init_test_tracing();
    use std::collections::HashSet;
    let h1 = TaskHandle(1);
    let h2 = TaskHandle(1);
    let h3 = TaskHandle(2);
    assert_eq!(h1, h2);
    assert_ne!(h1, h3);
    let mut set = HashSet::new();
    set.insert(h1);
    assert!(set.contains(&h2));
    assert!(!set.contains(&h3));
}

#[test]
fn facade_reexports_name_the_engine_front_door() {
    crate::test_utils::init_test_tracing();
    // The curated lib.rs facade must keep working: Context, Value,
    // ValueKind, EvalError, Flow at the crate root.
    let mut ctx: crate::Context = crate::Context::new();
    let v: crate::Value = ctx
        .eval_str_each("(+ 1 2)")
        .pop()
        .expect("one form")
        .expect("eval succeeds");
    assert!(matches!(v.kind(), crate::ValueKind::Fixnum(3)));
}

#[test]
fn regex_fuzz_support_checks_each_differential_through_one_interface() {
    use crate::fuzz_support::{RegexCase, RegexCheck, RegexDifferential, check_regex_differential};
    use strum::IntoEnumIterator;

    let case = RegexCase::new(
        "prefix\\(alpha\\|beta\\)suffix",
        b"noise prefixbetasuffix tail",
        false,
        0,
        0,
    );

    for differential in RegexDifferential::iter() {
        assert!(
            matches!(
                check_regex_differential(case, differential),
                Ok(RegexCheck::Equivalent { comparisons }) if comparisons > 0
            ),
            "{differential} should compare at least one operation",
        );
    }
}

#[test]
fn regex_fuzz_support_checks_search_optimizations_without_a_prefilter() {
    use crate::fuzz_support::{RegexCase, RegexCheck, RegexDifferential, check_regex_differential};

    let case = RegexCase::new("a", b"zzza", false, 0, 0);
    assert!(matches!(
        check_regex_differential(case, RegexDifferential::SearchOptimizations),
        Ok(RegexCheck::Equivalent { comparisons: 1 })
    ));
}

#[test]
fn local_socket_path_policy_keeps_explicit_names_literal() {
    use std::path::{Path, PathBuf};

    assert_eq!(
        crate::local_socket::socket_path_for_name("/tmp/neomacs.sock").unwrap(),
        PathBuf::from("/tmp/neomacs.sock")
    );
    let expected = if cfg!(windows) {
        PathBuf::from("/tmp/emacs-server/named.sock")
    } else {
        PathBuf::from("/run/user/1000/emacs/named.sock")
    };
    assert_eq!(
        crate::local_socket::socket_path_for_name_with(
            "named.sock",
            Some(Path::new("/run/user/1000")),
            Some(Path::new("/tmp")),
            Some(Path::new("/local")),
            None,
        )
        .unwrap(),
        expected
    );
}

#[cfg(unix)]
#[test]
fn local_socket_unix_directory_policy_prefers_override_then_xdg_then_effective_uid() {
    use std::path::{Path, PathBuf};

    assert_eq!(
        crate::local_socket::socket_directory_for_with(
            Some(Path::new("/run/user/1000")),
            Some(Path::new("/tmp")),
            Some(Path::new("/chosen")),
        )
        .unwrap(),
        PathBuf::from("/chosen")
    );
    assert_eq!(
        crate::local_socket::socket_directory_for_with(
            Some(Path::new("/run/user/1000")),
            Some(Path::new("/tmp")),
            None,
        )
        .unwrap(),
        PathBuf::from("/run/user/1000/emacs")
    );
    assert_eq!(
        crate::local_socket::socket_directory_for_with(None, Some(Path::new("/tmp")), None,)
            .unwrap(),
        PathBuf::from("/tmp").join(format!("emacs{}", unsafe { libc::geteuid() }))
    );
}

#[cfg(unix)]
#[test]
fn local_socket_unix_directory_policy_preserves_non_utf8_override() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    let override_dir = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
    assert_eq!(
        crate::local_socket::socket_directory_for_with(None, None, Some(&override_dir)).unwrap(),
        override_dir
    );
    assert_eq!(
        crate::local_socket::socket_path_for_name_with(
            "server",
            None,
            None,
            None,
            Some(&override_dir),
        )
        .unwrap(),
        override_dir.join("server")
    );
}

#[test]
fn local_socket_path_policy_rejects_invalid_sockaddr_paths() {
    use std::ffi::OsString;
    use std::path::PathBuf;

    let nul = PathBuf::from(OsString::from("bad\0socket"));
    let error = crate::local_socket::sockaddr_for_path(&nul).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("NUL"));

    #[cfg(unix)]
    let too_long = PathBuf::from("x".repeat(crate::local_socket::sockaddr_un_path_capacity()));
    #[cfg(windows)]
    let too_long = PathBuf::from("x".repeat(crate::local_socket::sockaddr_un_path_capacity()));
    let error = crate::local_socket::sockaddr_for_path(&too_long).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("sockaddr_un"));
}

#[cfg(unix)]
#[test]
fn local_socket_accepts_within_capacity_non_utf8_path() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    let path = PathBuf::from(OsString::from_vec(vec![b'x', 0xff]));
    assert!(crate::local_socket::sockaddr_for_path(&path).is_ok());
}

#[cfg(windows)]
#[test]
fn local_socket_windows_path_policy_prefers_override_then_shorter_root() {
    use std::path::{Path, PathBuf};

    assert_eq!(
        crate::local_socket::socket_path_for_name_with(
            "server",
            None,
            Some(Path::new("C:\\temp")),
            Some(Path::new("C:\\localappdata")),
            Some(Path::new("C:\\override")),
        )
        .unwrap(),
        PathBuf::from("C:\\override\\server")
    );
    assert_eq!(
        crate::local_socket::socket_path_for_name_with(
            "server",
            None,
            Some(Path::new("C:\\t")),
            Some(Path::new("C:\\long-localappdata")),
            None,
        )
        .unwrap(),
        PathBuf::from("C:\\t\\emacs-server\\server")
    );
}

#[cfg(windows)]
#[test]
fn local_socket_windows_path_policy_discards_invalid_default_before_selection() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::{Path, PathBuf};

    let invalid_temp = PathBuf::from(OsString::from_wide(&['C' as u16, b':' as u16, 0xD800]));
    let valid_local = Path::new("C:\\this-is-a-long-valid-localappdata-root");
    let expected = valid_local.join("emacs").join("server").join("server");

    assert_eq!(
        crate::local_socket::socket_path_for_name_with(
            "server",
            None,
            Some(&invalid_temp),
            Some(valid_local),
            None,
        )
        .unwrap(),
        expected
    );
}

#[cfg(windows)]
#[test]
fn local_socket_windows_path_policy_keeps_invalid_override_strict() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::Path;

    let invalid_override =
        Path::new(&OsString::from_wide(&['C' as u16, b':' as u16, 0xD800])).to_path_buf();
    let error = crate::local_socket::socket_path_for_name_with(
        "server",
        None,
        Some(Path::new("C:\\temp")),
        Some(Path::new("C:\\localappdata")),
        Some(&invalid_override),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("UTF-8"));
}

#[cfg(windows)]
#[test]
fn local_socket_windows_default_policy_skips_reparse_candidate() {
    use std::os::windows::fs::symlink_dir;
    use tempfile::tempdir;

    let root = tempdir().unwrap();
    let target = root.path().join("target");
    let reparse_candidate = root.path().join("emacs-server");
    let local_app_data = root.path().join("local-app-data");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&local_app_data).unwrap();
    if symlink_dir(&target, &reparse_candidate).is_err() {
        return;
    }

    assert_eq!(
        crate::local_socket::socket_directory_for_with(
            Some(root.path()),
            Some(&local_app_data),
            None,
        )
        .unwrap(),
        local_app_data.join("emacs").join("server")
    );
    assert!(
        crate::local_socket::socket_directory_for_with(
            Some(root.path()),
            Some(&local_app_data),
            Some(&reparse_candidate),
        )
        .is_err()
    );
}

#[cfg(windows)]
#[test]
fn local_socket_windows_preparation_allows_reparse_ancestors() {
    use std::os::windows::fs::symlink_dir;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    let _guard = crate::local_socket::TEST_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let root = tempdir().unwrap();
    let target = root.path().join("target");
    let junction = root.path().join("junction");
    std::fs::create_dir_all(&target).unwrap();
    if symlink_dir(&target, &junction).is_err() {
        return;
    }
    let selected = junction.join("socket-dir");
    let old_override = std::env::var_os("NEOMACS_SERVER_SOCKET_DIR");
    unsafe {
        std::env::set_var("NEOMACS_SERVER_SOCKET_DIR", &selected);
    }

    crate::local_socket::prepare_socket_directory().unwrap();

    match old_override {
        Some(value) => unsafe { std::env::set_var("NEOMACS_SERVER_SOCKET_DIR", value) },
        None => unsafe { std::env::remove_var("NEOMACS_SERVER_SOCKET_DIR") },
    }
}

#[cfg(windows)]
#[test]
fn local_socket_windows_rejects_existing_unprotected_directory_unchanged() {
    use std::os::windows::ffi::OsStrExt;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, SE_DACL_PROTECTED,
    };

    fn dacl_is_protected(path: &std::path::Path) -> bool {
        let wide_path = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut dacl = std::ptr::null_mut();
        let mut security_descriptor = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                GetNamedSecurityInfoW(
                    wide_path.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut security_descriptor,
                )
            },
            0
        );
        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            unsafe {
                GetSecurityDescriptorControl(security_descriptor, &mut control, &mut revision)
            },
            0
        );
        unsafe {
            LocalFree(security_descriptor);
        }
        control & SE_DACL_PROTECTED != 0
    }

    let _guard = crate::local_socket::TEST_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let root = tempdir().unwrap();
    let selected = root.path().join("existing");
    std::fs::create_dir_all(&selected).unwrap();
    assert!(!dacl_is_protected(&selected));
    let old_override = std::env::var_os("NEOMACS_SERVER_SOCKET_DIR");
    unsafe {
        std::env::set_var("NEOMACS_SERVER_SOCKET_DIR", &selected);
    }

    assert!(crate::local_socket::prepare_socket_directory().is_err());
    assert!(!dacl_is_protected(&selected));

    match old_override {
        Some(value) => unsafe { std::env::set_var("NEOMACS_SERVER_SOCKET_DIR", value) },
        None => unsafe { std::env::remove_var("NEOMACS_SERVER_SOCKET_DIR") },
    }
}

#[test]
fn local_socket_runtime_capability_is_stable() {
    let first = crate::local_socket::stream_supported();
    let second = crate::local_socket::stream_supported();
    assert_eq!(first, second);
    if cfg!(unix) {
        assert!(first);
    }
}

#[test]
fn local_socket_stream_round_trip() {
    if !crate::local_socket::stream_supported() {
        return;
    }

    use std::io::{Read, Write};
    use tempfile::tempdir;

    let directory = tempdir().unwrap();
    let path = directory.path().join("roundtrip.sock");
    let listener = crate::local_socket::bind_stream_listener(&path, 1).unwrap();
    let mut client = crate::local_socket::connect_stream(&path).unwrap();
    let (mut server, _) = crate::local_socket::accept_stream(&listener).unwrap();

    client.write_all(b"ping").unwrap();
    let mut request = [0; 4];
    server.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"ping");

    server.write_all(b"pong").unwrap();
    let mut response = [0; 4];
    client.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"pong");

    server.shutdown(std::net::Shutdown::Write).unwrap();
    client.shutdown(std::net::Shutdown::Both).unwrap();
}

#[cfg(windows)]
#[test]
fn local_socket_windows_directory_policy_selects_override_or_shortest_root() {
    use std::path::{Path, PathBuf};

    assert_eq!(
        crate::local_socket::socket_directory_for_with(
            Some(Path::new("C:\\temp")),
            Some(Path::new("C:\\localappdata")),
            Some(Path::new("C:\\override")),
        )
        .unwrap(),
        PathBuf::from("C:\\override")
    );
    assert_eq!(
        crate::local_socket::socket_directory_for_with(
            Some(Path::new("C:\\very-long-temp")),
            Some(Path::new("C:\\local")),
            None,
        )
        .unwrap(),
        PathBuf::from("C:\\local").join("emacs").join("server")
    );
}

#[cfg(windows)]
#[test]
fn local_socket_windows_prepares_directory_with_protected_user_and_system_dacl() {
    use std::os::windows::ffi::OsStrExt;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, GENERIC_ALL, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, GetTokenInformation, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
        TokenUser, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileAttributesW, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING, READ_CONTROL,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    let _guard = crate::local_socket::TEST_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let root = tempdir().unwrap();
    let override_dir = root.path().join("prepared-socket-dir");
    let old_override = std::env::var_os("NEOMACS_SERVER_SOCKET_DIR");
    unsafe {
        std::env::set_var(
            "NEOMACS_SERVER_SOCKET_DIR",
            override_dir.as_os_str().to_os_string(),
        );
    }

    let prepared = crate::local_socket::prepare_socket_directory().unwrap();
    assert_eq!(prepared, override_dir);
    crate::local_socket::prepare_socket_directory().unwrap();

    let wide_path = prepared
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let attributes = unsafe { GetFileAttributesW(wide_path.as_ptr()) };
    assert_ne!(attributes, INVALID_FILE_ATTRIBUTES);
    assert_eq!(attributes & FILE_ATTRIBUTE_REPARSE_POINT, 0);

    let directory_handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(directory_handle, INVALID_HANDLE_VALUE);

    let mut token = std::ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    assert_eq!(unsafe { GetLastError() }, ERROR_INSUFFICIENT_BUFFER);
    let mut token_info = vec![0u8; needed as usize];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_info.as_mut_ptr().cast(),
                token_info.len() as u32,
                &mut needed,
            )
        },
        0
    );
    let user_sid = unsafe { (*(token_info.as_ptr().cast::<TOKEN_USER>())).User.Sid };

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut security_descriptor = std::ptr::null_mut();
    assert_eq!(
        unsafe {
            GetSecurityInfo(
                directory_handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut security_descriptor,
            )
        },
        0
    );
    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(security_descriptor, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0);
    let mut dacl_present = 0;
    let mut dacl_defaulted = 0;
    assert_ne!(
        unsafe {
            GetSecurityDescriptorDacl(
                security_descriptor,
                &mut dacl_present,
                &mut dacl,
                &mut dacl_defaulted,
            )
        },
        0
    );
    assert_ne!(dacl_present, 0);
    let mut acl_size = ACL_SIZE_INFORMATION::default();
    assert_ne!(
        unsafe {
            GetAclInformation(
                dacl,
                (&mut acl_size as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        },
        0
    );
    let mut saw_user = false;
    let mut saw_system = false;
    for index in 0..acl_size.AceCount {
        let mut ace = std::ptr::null_mut();
        assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
        let header = unsafe { &*(ace.cast::<windows_sys::Win32::Security::ACE_HEADER>()) };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            continue;
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        if allowed.Mask & GENERIC_ALL != GENERIC_ALL
            && allowed.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
        {
            continue;
        }
        let sid = (&allowed.SidStart as *const u32)
            .cast::<std::ffi::c_void>()
            .cast_mut();
        if unsafe { windows_sys::Win32::Security::EqualSid(sid, user_sid) } != 0 {
            saw_user = true;
        }
        let mut system_sid = [0u8; 68];
        let mut system_sid_len = system_sid.len() as u32;
        assert_ne!(
            unsafe {
                windows_sys::Win32::Security::CreateWellKnownSid(
                    WinLocalSystemSid,
                    std::ptr::null_mut(),
                    system_sid.as_mut_ptr().cast(),
                    &mut system_sid_len,
                )
            },
            0
        );
        if unsafe {
            windows_sys::Win32::Security::EqualSid(
                sid,
                system_sid.as_mut_ptr().cast::<std::ffi::c_void>(),
            )
        } != 0
        {
            saw_system = true;
        }
    }
    assert!(saw_user);
    assert!(saw_system);

    unsafe {
        CloseHandle(directory_handle);
        CloseHandle(token);
        windows_sys::Win32::Foundation::LocalFree(security_descriptor);
    }
    match old_override {
        Some(value) => unsafe { std::env::set_var("NEOMACS_SERVER_SOCKET_DIR", value) },
        None => unsafe { std::env::remove_var("NEOMACS_SERVER_SOCKET_DIR") },
    }
}
