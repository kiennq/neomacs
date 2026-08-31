#[path = "../build_support/native_cache.rs"]
mod native_cache;

use std::path::{Path, PathBuf};

use native_cache::{
    IdentityRecord, audit_symbols, hash_identity_records, linker_flavor, linker_wrapper_path,
    parse_lld_override, rust_lld_path, staged_linker_file, support_decision,
};

#[test]
fn non_jit_macos_windows_gnu_and_cross_are_unsupported() {
    assert!(
        !support_decision(
            false,
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            "unix",
            true,
        )
        .supported
    );
    assert!(
        !support_decision(
            true,
            "x86_64-apple-darwin",
            "x86_64-apple-darwin",
            "unix",
            true,
        )
        .supported
    );
    assert!(
        !support_decision(
            true,
            "x86_64-pc-windows-gnu",
            "x86_64-pc-windows-gnu",
            "windows",
            true,
        )
        .supported
    );
    assert!(
        !support_decision(
            true,
            "aarch64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            "unix",
            false,
        )
        .supported
    );
}

#[test]
fn linker_flavor_and_staged_name_match_target() {
    assert_eq!(linker_flavor("x86_64-unknown-linux-gnu"), Some("gnu"));
    assert_eq!(staged_linker_file("gnu"), "ld.lld");
    assert_eq!(linker_flavor("x86_64-pc-windows-msvc"), Some("link"));
    assert_eq!(staged_linker_file("link"), "lld-link.exe");
    assert_eq!(linker_flavor("x86_64-apple-darwin"), None);
}

#[test]
fn linker_paths_use_target_wrapper_and_host_rust_lld() {
    let sysroot = if cfg!(windows) {
        Path::new(r"C:\sysroot")
    } else {
        Path::new("/sysroot")
    };
    let expected_wrapper = if cfg!(windows) {
        PathBuf::from(r"C:\sysroot\lib\rustlib\x86_64-unknown-linux-gnu\bin\gcc-ld\ld.lld")
    } else {
        PathBuf::from("/sysroot/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld/ld.lld")
    };
    assert_eq!(
        linker_wrapper_path(sysroot, "x86_64-unknown-linux-gnu"),
        expected_wrapper,
    );
    let expected_lld = if cfg!(windows) {
        PathBuf::from(r"C:\sysroot\lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe")
    } else {
        PathBuf::from("/sysroot/lib/rustlib/x86_64-pc-windows-msvc/bin/rust-lld.exe")
    };
    assert_eq!(
        rust_lld_path(sysroot, "x86_64-pc-windows-msvc"),
        expected_lld,
    );
}

#[test]
fn lld_override_must_be_absolute() {
    assert!(parse_lld_override(Some(Path::new("relative/lld"))).is_err());
    let absolute = std::env::current_dir().unwrap().join("rust-lld");
    assert_eq!(parse_lld_override(Some(&absolute)).unwrap(), Some(absolute),);
    assert_eq!(parse_lld_override(None).unwrap(), None);
}

#[test]
fn symbol_audit_accepts_only_approved_builtins() {
    assert!(
        audit_symbols(
            &[
                "neomacs_cache_memcpy",
                "neomacs_cache_memmove",
                "neomacs_cache_memset",
            ],
            &[],
        )
        .is_ok()
    );
    assert!(audit_symbols(&["neomacs_cache_memcpy"], &["memcpy"]).is_err());
    assert!(
        audit_symbols(
            &[
                "neomacs_cache_memcpy",
                "neomacs_cache_memmove",
                "neomacs_cache_memset",
                "unexpected",
            ],
            &[],
        )
        .is_err()
    );
}

#[test]
fn identity_is_order_independent_after_name_sort() {
    let a = IdentityRecord {
        name: "a".into(),
        bytes: vec![1],
    };
    let b = IdentityRecord {
        name: "b".into(),
        bytes: vec![2],
    };
    assert_eq!(
        hash_identity_records(&[a.clone(), b.clone()]),
        hash_identity_records(&[b, a]),
    );
}

#[test]
fn identity_changes_when_codegen_input_changes() {
    let before = IdentityRecord {
        name: "jit.rs".into(),
        bytes: vec![1],
    };
    let after = IdentityRecord {
        name: "jit.rs".into(),
        bytes: vec![2],
    };
    assert_ne!(
        hash_identity_records(&[before]),
        hash_identity_records(&[after]),
    );
}

#[test]
fn identity_changes_when_support_state_changes() {
    let supported = IdentityRecord {
        name: "support-state".into(),
        bytes: b"supported".to_vec(),
    };
    let unsupported = IdentityRecord {
        name: "support-state".into(),
        bytes: b"unsupported".to_vec(),
    };
    assert_ne!(
        hash_identity_records(&[supported]),
        hash_identity_records(&[unsupported]),
    );
}
