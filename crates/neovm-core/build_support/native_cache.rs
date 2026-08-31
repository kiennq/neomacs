use sha2::{Digest, Sha256};
#[cfg(not(test))]
use std::env;
#[cfg(not(test))]
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::process::Command;

#[cfg(not(test))]
use object::{Object, ObjectSymbol};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct IdentityRecord {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupportDecision {
    pub supported: bool,
    pub reason: Option<String>,
}

pub fn support_decision(
    jit_enabled: bool,
    target: &str,
    host: &str,
    target_family: &str,
    linker_available: bool,
) -> SupportDecision {
    let unsupported = |reason: &str| SupportDecision {
        supported: false,
        reason: Some(reason.to_owned()),
    };

    if !jit_enabled {
        return unsupported("JIT feature is disabled");
    }
    if target_family != "unix" && target_family != "windows" {
        return unsupported("target family is not supported");
    }
    if target.contains("apple-darwin") {
        return unsupported("macOS native-cache support is not enabled");
    }
    if target_family == "windows" && !target.contains("-windows-msvc") {
        return unsupported("only Windows MSVC targets are supported");
    }
    if target_family == "unix" && !target.contains("-linux-") {
        return unsupported("only Linux targets are supported");
    }
    if target != host && !linker_available {
        return unsupported("cross compilation has no usable native-cache linker");
    }
    if !linker_available {
        return unsupported("native-cache linker is unavailable");
    }
    SupportDecision {
        supported: true,
        reason: None,
    }
}

pub fn linker_flavor(target: &str) -> Option<&'static str> {
    if target.contains("-linux-") {
        Some("gnu")
    } else if target.contains("-windows-msvc") {
        Some("link")
    } else {
        None
    }
}

pub fn staged_linker_file(flavor: &str) -> &'static str {
    match flavor {
        "gnu" => "ld.lld",
        "link" => "lld-link.exe",
        _ => "",
    }
}

pub fn linker_wrapper_path(sysroot: &Path, target: &str) -> PathBuf {
    let driver = if target.contains("-windows-") {
        "lld-link.exe"
    } else {
        "ld.lld"
    };
    sysroot
        .join("lib")
        .join("rustlib")
        .join(target)
        .join("bin")
        .join("gcc-ld")
        .join(driver)
}

pub fn rust_lld_path(sysroot: &Path, host: &str) -> PathBuf {
    let driver = if host.contains("-windows-") {
        "rust-lld.exe"
    } else {
        "rust-lld"
    };
    sysroot
        .join("lib")
        .join("rustlib")
        .join(host)
        .join("bin")
        .join(driver)
}

pub fn parse_lld_override(path: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.is_absolute() {
        return Err("NEOMACS_NATIVE_CACHE_LLD must be an absolute path".into());
    }
    Ok(Some(path.to_path_buf()))
}

pub fn audit_symbols(exports: &[&str], undefined: &[&str]) -> Result<(), String> {
    const APPROVED: [&str; 3] = [
        "neomacs_cache_memcpy",
        "neomacs_cache_memmove",
        "neomacs_cache_memset",
    ];
    if !undefined.is_empty() {
        return Err(format!(
            "native-cache builtins object has undefined symbols: {:?}",
            undefined
        ));
    }
    let mut actual = exports.iter().copied().collect::<Vec<_>>();
    actual.sort_unstable();
    let mut approved = APPROVED.to_vec();
    approved.sort_unstable();
    if actual != approved {
        return Err(format!(
            "native-cache builtins exports {actual:?}, expected {approved:?}"
        ));
    }
    Ok(())
}

pub fn hash_identity_records(records: &[IdentityRecord]) -> String {
    let mut sorted = records.to_vec();
    sorted.sort();

    let mut hasher = Sha256::new();
    for record in sorted {
        mix_record(&mut hasher, &record.name, &record.bytes);
    }
    digest_hex(hasher.finalize())
}

fn mix_record(hasher: &mut Sha256, name: &str, bytes: &[u8]) {
    hasher.update((name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(not(test))]
pub fn emit_native_cache_build_metadata() {
    let manifest_dir = path_env("CARGO_MANIFEST_DIR");
    let project_root = path_env("CARGO_WORKSPACE_DIR");
    let out_dir = path_env("OUT_DIR");
    let target = env::var("TARGET").expect("TARGET");
    let rustc = path_env("RUSTC");
    let host = env::var("HOST").expect("HOST");
    let target_family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let native_cache_dir = out_dir.join("native-cache");
    fs::create_dir_all(&native_cache_dir).expect("create native-cache output directory");
    let builtins_source = manifest_dir.join("native-cache").join("builtins.rs");
    println!("cargo:rerun-if-changed={}", builtins_source.display());
    for name in [
        "RUSTC",
        "TARGET",
        "HOST",
        "CARGO_WORKSPACE_DIR",
        "CARGO_FEATURE_JIT",
        "CARGO_CFG_TARGET_FAMILY",
        "CARGO_CFG_TARGET_FEATURE",
        "CARGO_CFG_TARGET_ARCH",
        "CARGO_CFG_TARGET_ENV",
        "CARGO_CFG_TARGET_OS",
        "CARGO_CFG_TARGET_ABI",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "LTO",
        "CODEGEN_UNITS",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "NEOMACS_NATIVE_CACHE_LLD",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let lld_override = match parse_lld_override(
        env::var_os("NEOMACS_NATIVE_CACHE_LLD")
            .as_deref()
            .map(Path::new),
    ) {
        Ok(path) => path,
        Err(reason) => {
            emit_unsupported_metadata(&native_cache_dir, &target, &host, &reason);
            return;
        }
    };
    let Some(flavor) = linker_flavor(&target) else {
        emit_unsupported_metadata(
            &native_cache_dir,
            &target,
            &host,
            "target is outside the Linux and Windows MSVC support matrix",
        );
        return;
    };
    if !env::var_os("CARGO_FEATURE_JIT").is_some() {
        emit_unsupported_metadata(&native_cache_dir, &target, &host, "JIT feature is disabled");
        return;
    }
    if target_family != "unix" && target_family != "windows" {
        emit_unsupported_metadata(
            &native_cache_dir,
            &target,
            &host,
            "target family is outside the native-cache support matrix",
        );
        return;
    }

    let linker_source = if let Some(path) = lld_override {
        path
    } else {
        if target != host {
            emit_unsupported_metadata(
                &native_cache_dir,
                &target,
                &host,
                "cross compilation requires an absolute NEOMACS_NATIVE_CACHE_LLD override",
            );
            return;
        }
        let sysroot = command_stdout(&rustc, &["--print", "sysroot"]);
        let wrapper = linker_wrapper_path(Path::new(&sysroot), &target);
        println!("cargo:rerun-if-changed={}", wrapper.display());
        if !wrapper.is_file() {
            emit_unsupported_metadata(
                &native_cache_dir,
                &target,
                &host,
                "target-specific Rust gcc-ld linker wrapper is unavailable",
            );
            return;
        }
        rust_lld_path(Path::new(&sysroot), &host)
    };
    println!("cargo:rerun-if-changed={}", linker_source.display());
    let linker_available = linker_source.is_file();
    let decision = support_decision(true, &target, &host, &target_family, linker_available);
    if !decision.supported {
        emit_unsupported_metadata(
            &native_cache_dir,
            &target,
            &host,
            decision
                .reason
                .as_deref()
                .unwrap_or("native-cache is unsupported"),
        );
        return;
    }

    let builtins_name = if target.contains("windows") {
        "native-cache-builtins.obj"
    } else {
        "native-cache-builtins.o"
    };
    let builtins_path = native_cache_dir.join(builtins_name);
    compile_builtins(&rustc, &target, &builtins_source, &builtins_path);
    let builtins_bytes = fs::read(&builtins_path).expect("read native-cache builtins object");
    audit_builtins(&builtins_bytes);

    let linker_bytes = fs::read(&linker_source).expect("read native-cache rust-lld executable");
    let linker_version = command_stdout_path(&linker_source, &["-flavor", flavor, "--version"]);
    let rustc_version = command_stdout(&rustc, &["-Vv"]);

    let mut records = Vec::new();
    add_source_records(&mut records, &project_root, &manifest_dir.join("src"));
    add_source_records(
        &mut records,
        &project_root,
        &manifest_dir.join("build_support"),
    );
    add_source_records(
        &mut records,
        &project_root,
        &manifest_dir.join("native-cache"),
    );
    for (name, path) in [
        (
            "crates/neovm-core/Cargo.toml",
            manifest_dir.join("Cargo.toml"),
        ),
        ("Cargo.toml", project_root.join("Cargo.toml")),
        ("Cargo.lock", project_root.join("Cargo.lock")),
        (
            "rust-toolchain.toml",
            project_root.join("rust-toolchain.toml"),
        ),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
        records.push(IdentityRecord {
            name: name.into(),
            bytes: fs::read(path).expect("read native-cache identity input"),
        });
    }
    records.push(IdentityRecord {
        name: "target".into(),
        bytes: target.as_bytes().to_vec(),
    });
    records.push(IdentityRecord {
        name: "host".into(),
        bytes: host.as_bytes().to_vec(),
    });
    records.push(IdentityRecord {
        name: "target-family".into(),
        bytes: target_family.as_bytes().to_vec(),
    });
    records.push(IdentityRecord {
        name: "native-cache-supported".into(),
        bytes: b"1".to_vec(),
    });
    records.push(IdentityRecord {
        name: "native-cache-linker-flavor".into(),
        bytes: flavor.as_bytes().to_vec(),
    });
    records.push(IdentityRecord {
        name: "enabled-jit-features".into(),
        bytes: enabled_features(),
    });
    records.push(IdentityRecord {
        name: "target-features".into(),
        bytes: env::var("CARGO_CFG_TARGET_FEATURE")
            .unwrap_or_default()
            .into_bytes(),
    });
    records.push(IdentityRecord {
        name: "rustc-Vv".into(),
        bytes: rustc_version.as_bytes().to_vec(),
    });
    records.push(IdentityRecord {
        name: "cranelift-versions".into(),
        bytes: cranelift_versions(&project_root, &manifest_dir),
    });
    records.push(IdentityRecord {
        name: "codegen-settings".into(),
        bytes: codegen_settings().into_bytes(),
    });
    for name in [
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "LTO",
        "CODEGEN_UNITS",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
    ] {
        if let Some(value) = env::var_os(name) {
            records.push(IdentityRecord {
                name: format!("env:{name}"),
                bytes: value.to_string_lossy().into_owned().into_bytes(),
            });
        }
    }
    records.push(IdentityRecord {
        name: "native-cache-builtins".into(),
        bytes: builtins_bytes.clone(),
    });
    records.push(IdentityRecord {
        name: "native-cache-lld".into(),
        bytes: linker_bytes.clone(),
    });
    let build_id = hash_identity_records(&records);
    let builtins_sha256 = digest_hex(Sha256::digest(&builtins_bytes));
    let linker_sha256 = digest_hex(Sha256::digest(&linker_bytes));

    emit_constants(
        true,
        &build_id,
        &linker_version,
        &linker_sha256,
        &builtins_sha256,
    );

    let metadata = format!(
        concat!(
            "{{\n",
            "  \"format_version\": 1,\n",
            "  \"supported\": true,\n",
            "  \"unsupported_reason\": \"\",\n",
            "  \"target\": \"{}\",\n",
            "  \"host\": \"{}\",\n",
            "  \"build_id\": \"{}\",\n",
            "  \"linker_flavor\": \"{}\",\n",
            "  \"linker_source_file\": \"{}\",\n",
            "  \"staged_linker_file\": \"{}\",\n",
            "  \"lld_version\": \"{}\",\n",
            "  \"lld_sha256\": \"{}\",\n",
            "  \"builtins_file\": \"{}\",\n",
            "  \"builtins_sha256\": \"{}\"\n",
            "}}\n"
        ),
        json_escape(&target),
        json_escape(&host),
        build_id,
        flavor,
        json_escape(&absolute_path(&linker_source)),
        staged_linker_file(flavor),
        json_escape(&linker_version),
        linker_sha256,
        json_escape(&absolute_path(&builtins_path)),
        builtins_sha256,
    );
    fs::write(native_cache_dir.join("build-metadata.json"), metadata)
        .expect("write native-cache build metadata");
}

#[cfg(not(test))]
fn emit_constants(
    supported: bool,
    build_id: &str,
    linker_version: &str,
    linker_sha256: &str,
    builtins_sha256: &str,
) {
    println!(
        "cargo:rustc-env=NEOMACS_NATIVE_CACHE_SUPPORTED={}",
        if supported { "1" } else { "0" }
    );
    println!("cargo:rustc-env=NEOMACS_NATIVE_CACHE_BUILD_ID={build_id}");
    println!("cargo:rustc-env=NEOMACS_NATIVE_CACHE_LLD_VERSION={linker_version}");
    println!("cargo:rustc-env=NEOMACS_NATIVE_CACHE_LLD_SHA256={linker_sha256}");
    println!("cargo:rustc-env=NEOMACS_NATIVE_CACHE_BUILTINS_SHA256={builtins_sha256}");
}

#[cfg(not(test))]
fn emit_unsupported_metadata(native_cache_dir: &Path, target: &str, host: &str, reason: &str) {
    emit_constants(false, "", "", "", "");
    println!("cargo:warning=native-cache unavailable: {reason}");
    let metadata = format!(
        concat!(
            "{{\n",
            "  \"format_version\": 1,\n",
            "  \"supported\": false,\n",
            "  \"unsupported_reason\": \"{}\",\n",
            "  \"target\": \"{}\",\n",
            "  \"host\": \"{}\",\n",
            "  \"build_id\": \"\",\n",
            "  \"linker_flavor\": \"\",\n",
            "  \"linker_source_file\": \"\",\n",
            "  \"staged_linker_file\": \"\",\n",
            "  \"lld_version\": \"\",\n",
            "  \"lld_sha256\": \"\",\n",
            "  \"builtins_file\": \"\",\n",
            "  \"builtins_sha256\": \"\"\n",
            "}}\n"
        ),
        json_escape(reason),
        json_escape(target),
        json_escape(host),
    );
    fs::write(native_cache_dir.join("build-metadata.json"), metadata)
        .expect("write unsupported native-cache build metadata");
}

#[cfg(not(test))]
fn compile_builtins(rustc: &Path, target: &str, source: &Path, output: &Path) {
    let result = Command::new(rustc)
        .arg("--crate-name")
        .arg("native_cache_builtins")
        .arg("--crate-type=lib")
        .arg("--emit=obj")
        .arg("-C")
        .arg("panic=abort")
        .arg("-C")
        .arg("opt-level=2")
        .arg("-C")
        .arg("relocation-model=pic")
        .arg("--target")
        .arg(target)
        .arg("--edition=2024")
        .arg("-o")
        .arg(output)
        .arg(source)
        .output()
        .unwrap_or_else(|error| panic!("invoke rustc for native-cache builtins: {error}"));
    if !result.status.success() {
        panic!(
            "rustc failed to compile native-cache builtins:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[cfg(not(test))]
fn audit_builtins(bytes: &[u8]) {
    let file = object::File::parse(bytes).expect("parse native-cache builtins object");
    let mut exports = Vec::new();
    let mut undefined = Vec::new();
    for symbol in file.symbols() {
        let name = symbol.name().unwrap_or("<invalid utf-8>");
        if symbol.is_undefined() {
            if !name.is_empty() {
                undefined.push(name.to_owned());
            }
        } else if symbol.is_global() && symbol.is_definition() && !name.is_empty() {
            exports.push(name.to_owned());
        }
    }
    let export_refs = exports.iter().map(String::as_str).collect::<Vec<_>>();
    let undefined_refs = undefined.iter().map(String::as_str).collect::<Vec<_>>();
    audit_symbols(&export_refs, &undefined_refs).unwrap_or_else(|error| panic!("{error}"));
}

#[cfg(not(test))]
fn add_source_records(records: &mut Vec<IdentityRecord>, root: &Path, directory: &Path) {
    let mut files = Vec::new();
    collect_rs_files(directory, &mut files);
    files.sort();
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = path
            .strip_prefix(root)
            .expect("identity input must be within workspace")
            .to_string_lossy()
            .replace('\\', "/");
        records.push(IdentityRecord {
            name: relative,
            bytes: fs::read(path).expect("read native-cache source identity input"),
        });
    }
}

#[cfg(not(test))]
fn collect_rs_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(directory).unwrap_or_else(|error| {
        panic!(
            "read identity input directory {}: {error}",
            directory.display()
        )
    });
    for entry in entries {
        let path = entry.expect("read identity input directory entry").path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[cfg(not(test))]
fn enabled_features() -> Vec<u8> {
    let mut features = env::vars()
        .filter_map(|(name, _)| {
            name.strip_prefix("CARGO_FEATURE_")
                .map(str::to_ascii_lowercase)
        })
        .collect::<Vec<_>>();
    features.sort();
    features.join(",").into_bytes()
}

#[cfg(not(test))]
fn cranelift_versions(project_root: &Path, manifest_dir: &Path) -> Vec<u8> {
    let mut lines = Vec::new();
    for path in [
        project_root.join("Cargo.toml"),
        manifest_dir.join("Cargo.toml"),
    ] {
        let contents = fs::read_to_string(path).expect("read Cargo.toml for Cranelift identity");
        lines.extend(
            contents
                .lines()
                .filter(|line| line.contains("cranelift-") && line.contains('='))
                .map(str::trim)
                .map(str::to_owned),
        );
    }
    lines.sort();
    lines.join("\n").into_bytes()
}

#[cfg(not(test))]
fn codegen_settings() -> String {
    [
        "--crate-type=lib",
        "--emit=obj",
        "--edition=2024",
        "-C panic=abort",
        "-C opt-level=2",
        "-C relocation-model=pic",
    ]
    .join(" ")
}

#[cfg(not(test))]
fn path_env(name: &str) -> PathBuf {
    PathBuf::from(env::var_os(name).unwrap_or_else(|| panic!("{name}")))
}

#[cfg(not(test))]
fn command_stdout(program: &Path, args: &[&str]) -> String {
    command_stdout_path(program, args)
}

#[cfg(not(test))]
fn command_stdout_path(program: &Path, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("invoke {}: {error}", program.display()));
    if !output.status.success() {
        panic!(
            "{} {:?} failed:\n{}",
            program.display(),
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("{} output is not UTF-8: {error}", program.display()))
        .trim()
        .to_owned()
}

#[cfg(not(test))]
fn absolute_path(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|error| panic!("canonicalize {}: {error}", path.display()))
        .to_string_lossy()
        .into_owned()
}

#[cfg(not(test))]
fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn digest_hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
