use super::*;

#[test]
fn executable_cache_directory_is_created_private() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("native-cache");
    prepare_private_directory(&path, PrivateDirectoryPurpose::ExecutableCache).unwrap();
    validate_private_directory(&path, PrivateDirectoryPurpose::ExecutableCache).unwrap();
}

#[cfg(unix)]
#[test]
fn executable_cache_rejects_group_writable_directory() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(
        validate_private_directory(tmp.path(), PrivateDirectoryPurpose::ExecutableCache,).is_err()
    );
}

#[cfg(unix)]
#[test]
fn executable_cache_recursive_creation_is_private_under_permissive_umask() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cache").join("native-cache");
    let previous_umask = unsafe { libc::umask(0) };
    let preparation = prepare_private_directory(&path, PrivateDirectoryPurpose::ExecutableCache);
    unsafe {
        libc::umask(previous_umask);
    }
    preparation.unwrap();

    for directory in [tmp.path().join("cache"), path] {
        let mode = std::fs::symlink_metadata(&directory)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode, 0o700,
            "executable cache directory should be created private, got {mode:o}"
        );
    }
}

#[cfg(windows)]
#[test]
fn private_directory_purposes_are_created_with_protected_security() {
    let tmp = tempfile::tempdir().unwrap();
    for (name, purpose) in [
        ("local-socket", PrivateDirectoryPurpose::LocalSocket),
        ("executable-cache", PrivateDirectoryPurpose::ExecutableCache),
    ] {
        let path = tmp.path().join(name);
        prepare_private_directory(&path, purpose).unwrap();
        validate_private_directory(&path, purpose).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn private_directory_rejects_reparse_points() {
    use std::os::windows::fs::symlink_dir;

    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target");
    let link = tmp.path().join("link");
    std::fs::create_dir_all(&target).unwrap();
    if symlink_dir(&target, &link).is_err() {
        return;
    }

    assert!(validate_private_directory(&link, PrivateDirectoryPurpose::ExecutableCache,).is_err());
}
