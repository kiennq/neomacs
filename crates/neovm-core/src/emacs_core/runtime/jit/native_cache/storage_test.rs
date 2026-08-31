use super::*;
use crate::emacs_core::jit::native_cache::storage;
use crate::emacs_core::jit::native_cache::{
    ContentHash, FunctionPrekey, ManifestLeaf, VariantHash,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

fn config(root: &Path) -> NativeCacheConfig {
    let config = NativeCacheConfig::for_paths(
        root.join("cache"),
        PathBuf::from("toolchain"),
        NativeCacheAccess::ReadWrite,
    );
    crate::private_directory::prepare_private_directory(
        &config.root,
        crate::private_directory::PrivateDirectoryPurpose::ExecutableCache,
    )
    .unwrap();
    config
}

fn manifest(id: GenerationId, library: &str, bytes: &[u8]) -> GenerationManifest {
    GenerationManifest {
        format_version: format::FORMAT_VERSION,
        generation_id: id,
        build_id: if super::super::BUILD_ID.is_empty() {
            "a".repeat(64)
        } else {
            super::super::BUILD_ID.to_owned()
        },
        abi_tag: storage::runtime_abi_tag(),
        target: storage::runtime_target().to_owned(),
        library_file: library.to_owned(),
        library_sha256: hex_sha256(bytes),
        created_unix_secs: 1,
        leaves: Vec::new(),
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn stage(namespace: &Path, name: &str, id: GenerationId, bytes: &[u8]) -> PathBuf {
    let stage = namespace.join(name);
    fs::create_dir_all(&stage).unwrap();
    let library = storage::library_basename();
    fs::write(stage.join(library), bytes).unwrap();
    let encoded = serde_json::to_vec(&manifest(id, library, bytes)).unwrap();
    fs::write(stage.join("manifest.json"), encoded).unwrap();
    stage
}

fn stage_with_manifest(
    namespace: &Path,
    name: &str,
    manifest: &GenerationManifest,
    bytes: &[u8],
) -> PathBuf {
    let stage = namespace.join(name);
    fs::create_dir_all(&stage).unwrap();
    fs::write(stage.join(storage::library_basename()), bytes).unwrap();
    fs::write(
        stage.join("manifest.json"),
        serde_json::to_vec(manifest).unwrap(),
    )
    .unwrap();
    stage
}

#[test]
fn final_generation_is_visible_only_after_directory_rename() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());
    let namespace = storage::namespace_path(&cfg);
    fs::create_dir_all(namespace.join("generations")).unwrap();
    let id = GenerationId::from_u128(1);
    let stage = stage(&namespace, ".tmp-test-visible", id, b"library");
    let destination = namespace.join("generations").join(id.to_string());
    assert!(!destination.exists());

    let outcome = storage::publish_generation(&cfg, &stage, id).unwrap();

    assert!(matches!(outcome, storage::PublishOutcome::Published));
    assert!(destination.join("manifest.json").is_file());
    assert!(destination.join(storage::library_basename()).is_file());
}

#[test]
fn concurrent_destination_exists_validates_winner_and_succeeds() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());
    let namespace = storage::namespace_path(&cfg);
    fs::create_dir_all(namespace.join("generations")).unwrap();
    let id = GenerationId::from_u128(2);
    let left = stage(&namespace, ".tmp-test-left", id, b"left");
    let right = stage(&namespace, ".tmp-test-right", id, b"right");

    let left_cfg = cfg.clone();
    let right_cfg = cfg.clone();
    let left_thread =
        std::thread::spawn(move || storage::publish_generation(&left_cfg, &left, id).unwrap());
    let right_thread =
        std::thread::spawn(move || storage::publish_generation(&right_cfg, &right, id).unwrap());
    let outcomes = [left_thread.join().unwrap(), right_thread.join().unwrap()];

    assert!(outcomes.iter().all(|outcome| {
        matches!(
            outcome,
            storage::PublishOutcome::Published | storage::PublishOutcome::AlreadyPublished
        )
    }));
    assert!(
        namespace
            .join("generations")
            .join(id.to_string())
            .join("manifest.json")
            .is_file()
    );
}

#[test]
fn startup_ignores_temp_and_manifestless_generation_directories() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());
    let namespace = storage::namespace_path(&cfg);
    fs::create_dir_all(namespace.join("generations")).unwrap();

    let valid_id = GenerationId::from_u128(3);
    let valid_stage = stage(&namespace, ".tmp-test-valid", valid_id, b"valid");
    storage::publish_generation(&cfg, &valid_stage, valid_id).unwrap();

    fs::create_dir_all(namespace.join("generations").join("4")).unwrap();
    fs::create_dir_all(namespace.join(".tmp-ignored")).unwrap();
    fs::create_dir_all(namespace.join("generations").join("not-a-generation")).unwrap();

    let index = storage::open_namespace(&cfg).unwrap();

    assert_eq!(index.generations.len(), 1);
    assert_eq!(index.generations[0].generation_id, valid_id,);
}

#[test]
fn clear_marker_is_consumed_only_for_its_cache_root() {
    let _lock = test_lock();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_config = config(first.path());
    let second_config = config(second.path());

    storage::request_clear(&first_config).unwrap();
    assert!(first.path().join("cache/.clear-on-start").is_file());
    assert!(!second.path().join("cache/.clear-on-start").exists());

    storage::maintain(&second_config, Instant::now() + Duration::from_secs(1)).unwrap();
    assert!(first.path().join("cache/.clear-on-start").is_file());

    storage::maintain(&first_config, Instant::now() + Duration::from_secs(1)).unwrap();
    assert!(!first.path().join("cache/.clear-on-start").exists());
}

#[test]
fn clear_removes_only_recognized_namespaces_and_consumes_marker() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());
    let root = &cfg.root;
    let active_namespace = storage::namespace_path(&cfg);
    let other_build = root.join(storage::runtime_target()).join("b".repeat(64));
    fs::create_dir_all(active_namespace.join("generations")).unwrap();
    fs::create_dir_all(other_build.join("generations")).unwrap();
    fs::write(
        active_namespace.join("generations/active-sentinel"),
        b"cache",
    )
    .unwrap();
    fs::write(other_build.join("generations/other-sentinel"), b"cache").unwrap();

    let unrelated_file = root.join("unrelated-sentinel");
    let unrelated_directory = root.join("unrelated-directory");
    fs::write(&unrelated_file, b"keep").unwrap();
    fs::create_dir(&unrelated_directory).unwrap();
    fs::write(unrelated_directory.join("sentinel"), b"keep").unwrap();
    let unknown_build = root.join("other-target").join("not-a-build-id");
    fs::create_dir_all(&unknown_build).unwrap();
    fs::write(unknown_build.join("sentinel"), b"keep").unwrap();

    storage::request_clear(&cfg).unwrap();
    storage::maintain(&cfg, Instant::now() + Duration::from_secs(1))
        .expect("clear should remain usable");

    assert!(
        !active_namespace
            .join("generations/active-sentinel")
            .exists()
    );
    assert!(!other_build.exists());
    assert!(unrelated_file.exists());
    assert!(unrelated_directory.join("sentinel").exists());
    assert!(unknown_build.join("sentinel").exists());
    assert!(!root.join(".clear-on-start").exists());
}

#[test]
fn disappeared_entries_are_skipped_during_indexing_and_maintenance() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());
    let namespace = storage::namespace_path(&cfg);
    let generation = namespace.join("generations/00000000000000000000000000000004");
    fs::create_dir_all(&generation).unwrap();
    storage::test_disappear_on_next_metadata(generation);
    assert!(storage::open_namespace(&cfg).is_ok());

    let expiring = namespace.join(".tmp-expiring");
    fs::create_dir(&expiring).unwrap();
    storage::test_disappear_on_next_metadata(expiring);
    let mut report = MaintenanceReport::default();
    super::clean_temporary_entries(
        &namespace,
        Instant::now() + Duration::from_secs(1),
        &mut report,
    )
    .unwrap();

    let size_root = tmp.path().join("size-root");
    let size_entry = size_root.join("entry");
    fs::create_dir_all(&size_root).unwrap();
    fs::write(&size_entry, b"gone").unwrap();
    storage::test_disappear_on_next_metadata(size_entry);
    assert_eq!(super::directory_size(&size_root).unwrap(), 0);

    let journal = namespace.join("journals/journal-gone.json");
    fs::write(&journal, br#"[]"#).unwrap();
    storage::test_disappear_on_next_metadata(journal);
    super::merge_usage_journals(
        &namespace,
        Instant::now() + Duration::from_secs(1),
        &mut report,
    )
    .unwrap();

    assert!(storage::maintain(&cfg, Instant::now() + Duration::from_secs(1)).is_ok());
}

#[cfg(any(unix, windows))]
#[test]
fn links_are_skipped_and_unlinked_without_touching_targets() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());
    let namespace = storage::namespace_path(&cfg);
    fs::create_dir_all(namespace.join("generations")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("sentinel");
    fs::write(&outside_file, b"must survive").unwrap();
    let link = namespace
        .join("generations")
        .join("00000000000000000000000000000005");
    if let Err(error) = create_directory_link(outside.path(), &link) {
        if cfg!(windows) {
            eprintln!("skipping reparse-point test: {error}");
            return;
        }
        panic!("failed to create test link: {error}");
    }

    assert!(storage::open_namespace(&cfg).is_ok());
    storage::request_clear(&cfg).unwrap();
    storage::maintain(&cfg, Instant::now() + Duration::from_secs(1))
        .expect("clear should recover from link");
    assert!(outside_file.exists());
    assert!(!link.exists());

    storage::request_clear(&cfg).unwrap();
    storage::maintain(&cfg, Instant::now() + Duration::from_secs(1))
        .expect("repeated clear should remain usable");
    assert!(outside_file.exists());
}

fn create_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)
    }
}

#[test]
fn complete_usage_journals_merge_into_recency_metadata() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());
    storage::record_usage(&cfg, &[GenerationId::from_u128(9)]).unwrap();

    let report = storage::maintain(&cfg, Instant::now() + Duration::from_secs(1)).unwrap();

    assert_eq!(report.journals_merged, 1);
    assert!(storage::namespace_path(&cfg).join("recency.json").is_file());
    assert!(
        fs::read_dir(storage::namespace_path(&cfg).join("journals"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn usage_journal_survives_recency_persistence_failure() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());
    let namespace = storage::namespace_path(&cfg);
    let journal = {
        storage::record_usage(&cfg, &[GenerationId::from_u128(12)]).unwrap();
        fs::read_dir(namespace.join("journals"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .next()
            .unwrap()
    };
    let journal_bytes = fs::read(&journal).unwrap();
    let recency = namespace.join("recency.json");
    fs::remove_file(&recency).unwrap();
    fs::create_dir(&recency).unwrap();

    let mut report = MaintenanceReport::default();
    assert!(
        super::merge_usage_journals(
            &namespace,
            Instant::now() + Duration::from_secs(1),
            &mut report,
        )
        .is_err()
    );
    assert!(journal.is_file());
    assert_eq!(fs::read(&journal).unwrap(), journal_bytes);

    fs::remove_dir(&recency).unwrap();
    fs::write(&recency, b"{}").unwrap();
    let mut retry_report = MaintenanceReport::default();
    super::merge_usage_journals(
        &namespace,
        Instant::now() + Duration::from_secs(1),
        &mut retry_report,
    )
    .unwrap();

    assert_eq!(retry_report.journals_merged, 1);
    let merged: BTreeMap<String, u64> =
        serde_json::from_slice(&fs::read(&recency).unwrap()).unwrap();
    assert!(merged.contains_key(&GenerationId::from_u128(12).to_string()));
    assert!(!journal.exists());
}

#[test]
fn quarantine_removes_a_generation_only_after_all_leaf_keys_are_quarantined() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());
    let namespace = storage::namespace_path(&cfg);
    fs::create_dir_all(namespace.join("generations")).unwrap();
    let id = GenerationId::from_u128(10);
    let bytes = b"library";
    let mut generation_manifest = manifest(id, storage::library_basename(), bytes);
    generation_manifest.leaves = vec![ManifestLeaf {
        prekey: FunctionPrekey::new("demo", 1, 1),
        content_hash: ContentHash::from_u128(1),
        variant_hash: VariantHash::from_u128(2),
        arity: 1,
        entry_symbol: "entry".into(),
        descriptor_symbol: "descriptor".into(),
        descriptor_bytes: 0,
        reloc_recipe_bytes: 0,
        spec_site_count: 0,
    }];
    let staged = stage_with_manifest(
        &namespace,
        ".tmp-test-quarantine",
        &generation_manifest,
        bytes,
    );
    storage::publish_generation(&cfg, &staged, id).unwrap();
    storage::quarantine_leaf(&cfg, ContentHash::from_u128(1), VariantHash::from_u128(2)).unwrap();

    let index = storage::open_namespace(&cfg).unwrap();
    assert_eq!(index.generations.len(), 1);
    assert!(index.generations[0].leaves.is_empty());

    let report = storage::maintain(&cfg, Instant::now() + Duration::from_secs(1)).unwrap();
    assert_eq!(report.quarantined_generations_removed, 1);
    assert!(!namespace.join("generations").join(id.to_string()).exists());
}

#[test]
fn maintenance_prunes_oldest_generation_when_disk_budget_is_exceeded() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = config(tmp.path());
    cfg.max_cache_bytes = 1;
    let namespace = storage::namespace_path(&cfg);
    fs::create_dir_all(namespace.join("generations")).unwrap();
    let id = GenerationId::from_u128(11);
    let staged = stage(&namespace, ".tmp-test-prune", id, b"library");
    storage::publish_generation(&cfg, &staged, id).unwrap();

    let report = storage::maintain(&cfg, Instant::now() + Duration::from_secs(1)).unwrap();

    assert_eq!(report.generations_pruned, 1);
    assert!(!namespace.join("generations").join(id.to_string()).exists());
}

#[test]
fn maintenance_stamp_defers_repeat_work_for_a_day() {
    let _lock = test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(tmp.path());

    storage::open_namespace(&cfg).unwrap();
    storage::maintain(&cfg, Instant::now() + Duration::from_secs(1)).unwrap();
    let second = storage::maintain(&cfg, Instant::now() + Duration::from_secs(1)).unwrap();

    assert!(second.skipped_due_to_stamp);
}

#[test]
fn generation_directory_names_require_canonical_lowercase_hex() {
    assert_eq!(
        GenerationId::parse_directory_name("00000000000000000000000000000001"),
        Some(GenerationId::from_u128(1))
    );
    assert_eq!(
        GenerationId::parse_directory_name("0000000000000000000000000000000A"),
        None
    );
    assert_eq!(
        GenerationId::parse_directory_name("0000000000000000000000000000001"),
        None
    );
}
