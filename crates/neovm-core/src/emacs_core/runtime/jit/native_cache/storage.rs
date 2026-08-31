//! On-disk storage for immutable native-cache generations.
//!
//! The filesystem is an untrusted input boundary.  This module therefore
//! validates names and link metadata before opening any cache file, and keeps
//! publication as a single directory operation.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::private_directory::{PrivateDirectoryPurpose, prepare_private_directory};

use super::{
    ContentHash, GenerationId, GenerationIndex, GenerationManifest, MAX_MANIFEST_BYTES,
    ManifestLimits, NativeCacheAccess, NativeCacheConfig, NativeCacheError, VariantHash, format,
    validate_manifest_identity,
};

#[cfg(test)]
#[path = "storage_test.rs"]
mod tests;

const MAX_CACHE_LEAVES: usize = 4_096;
const MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;
const TEMP_EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);
const NAMESPACE_EXPIRY: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_JOURNAL_IDS: usize = MAX_CACHE_LEAVES;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const RUNTIME_TARGET: &str = "x86_64-unknown-linux-gnu";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const RUNTIME_TARGET: &str = "aarch64-unknown-linux-gnu";
#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
const RUNTIME_TARGET: &str = "x86_64-pc-windows-msvc";
#[cfg(all(target_os = "windows", target_arch = "aarch64", target_env = "msvc"))]
const RUNTIME_TARGET: &str = "aarch64-pc-windows-msvc";
#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"),
    all(target_os = "windows", target_arch = "aarch64", target_env = "msvc"),
)))]
const RUNTIME_TARGET: &str = "unknown-target";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublishOutcome {
    Published,
    AlreadyPublished,
    WritesUnsupported,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MaintenanceReport {
    pub temporary_directories_removed: u64,
    pub journals_merged: u64,
    pub quarantined_generations_removed: u64,
    pub generations_pruned: u64,
    pub namespaces_removed: u64,
    pub trash_removed: u64,
    pub skipped_due_to_stamp: bool,
    pub budget_exhausted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageJournal {
    generation_ids: Vec<GenerationId>,
    #[serde(default)]
    used_unix_secs: Option<u64>,
}

#[cfg(test)]
static TEST_DISAPPEAR_PATH: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn test_disappear_on_next_metadata(path: PathBuf) {
    *TEST_DISAPPEAR_PATH.lock().unwrap() = Some(path);
}

#[cfg(test)]
fn disappear_before_metadata(path: &Path) {
    let mut target = TEST_DISAPPEAR_PATH.lock().unwrap();
    let should_disappear = target.as_ref().is_some_and(|target| target == path);
    if !should_disappear {
        return;
    }
    target.take();
    drop(target);
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

#[cfg(not(test))]
fn disappear_before_metadata(_path: &Path) {}

pub(crate) fn runtime_target() -> &'static str {
    RUNTIME_TARGET
}

pub(crate) fn library_basename() -> &'static str {
    if cfg!(windows) {
        "native-cache.dll"
    } else {
        "native-cache.so"
    }
}

pub(crate) fn runtime_abi_tag() -> u32 {
    super::super::aot::ABI_TAG
}

pub(crate) fn namespace_path(config: &NativeCacheConfig) -> PathBuf {
    config
        .root
        .join(runtime_target())
        .join(build_id_component())
}

fn build_id_component() -> &'static str {
    if super::BUILD_ID.is_empty() {
        "unsupported"
    } else {
        super::BUILD_ID
    }
}

pub(crate) fn open_namespace(
    config: &NativeCacheConfig,
) -> Result<GenerationIndex, NativeCacheError> {
    if config.access == NativeCacheAccess::Disabled {
        return Ok(GenerationIndex::default());
    }

    let namespace = prepare_namespace(config)?;
    let quarantined =
        read_quarantine(&namespace).map_err(|error| io_error("read quarantine", error))?;
    let generations = namespace.join("generations");
    let mut manifests = Vec::new();

    let deadline = Instant::now() + config.active_index_budget;
    for entry in read_dir(&generations, "scan generations")? {
        if !within_deadline(Some(deadline)) {
            break;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(id) = parse_generation_name(&name) else {
            continue;
        };
        let generation = entry.path();
        let Some(metadata) = (match checked_metadata(&generation) {
            Ok(metadata) => metadata,
            Err(NativeCacheError::UnsafePath(_)) => continue,
            Err(error) => return Err(error),
        }) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let Some(mut manifest) = read_valid_generation(&generation, id)? else {
            continue;
        };
        manifest
            .leaves
            .retain(|leaf| !quarantined.contains(&leaf_key(leaf.content_hash, leaf.variant_hash)));
        manifests.push(manifest);
    }

    manifests.sort_by(|left, right| {
        right
            .created_unix_secs
            .cmp(&left.created_unix_secs)
            .then_with(|| right.generation_id.cmp(&left.generation_id))
    });
    Ok(GenerationIndex::from_manifests(manifests))
}

pub(crate) fn publish_generation(
    config: &NativeCacheConfig,
    staged_dir: &Path,
    id: GenerationId,
) -> Result<PublishOutcome, NativeCacheError> {
    if config.access != NativeCacheAccess::ReadWrite {
        return Ok(PublishOutcome::WritesUnsupported);
    }

    let namespace = prepare_namespace(config)?;
    let expected_stage_parent = namespace;
    let stage_parent = staged_dir
        .parent()
        .ok_or_else(|| NativeCacheError::UnsafePath("staging directory has no parent".into()))?;
    if stage_parent != expected_stage_parent {
        return Err(NativeCacheError::UnsafePath(
            "staging directory is outside the active namespace".into(),
        ));
    }
    let stage_name = staged_dir
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| NativeCacheError::UnsafePath("invalid staging directory name".into()))?;
    if !stage_name.starts_with(".tmp-") || !valid_component(stage_name) {
        return Err(NativeCacheError::UnsafePath(
            "staging directory name must begin with .tmp-".into(),
        ));
    }
    let Some(stage_metadata) = checked_metadata(staged_dir)? else {
        return Err(NativeCacheError::InvalidPublication(
            "staging directory disappeared".into(),
        ));
    };
    if !stage_metadata.is_dir() {
        return Err(NativeCacheError::UnsafePath(
            "staging path is not a directory".into(),
        ));
    }

    let destination = expected_stage_parent
        .join("generations")
        .join(id.to_string());
    let manifest = read_valid_generation(staged_dir, id)?.ok_or_else(|| {
        NativeCacheError::InvalidPublication("staging directory is incomplete".into())
    })?;
    sync_generation(staged_dir, &manifest)?;

    let moved = match rename_noreplace(staged_dir, &destination) {
        Ok(result) => result,
        Err(MoveError::AlreadyExists) => MoveResult::AlreadyExists,
        Err(MoveError::Unsupported) => MoveResult::Unsupported,
        Err(MoveError::Io(error)) => return Err(io_error("publish generation", error)),
    };
    match moved {
        MoveResult::Published => {
            sync_directory(&destination.parent().expect("generation has a parent"))
                .map_err(|error| io_error("sync generations directory", error))?;
            Ok(PublishOutcome::Published)
        }
        MoveResult::Unsupported => {
            let _ = remove_tree(staged_dir);
            Ok(PublishOutcome::WritesUnsupported)
        }
        MoveResult::AlreadyExists => {
            let winner = read_valid_generation(&destination, id)?.ok_or_else(|| {
                NativeCacheError::InvalidPublication(
                    "existing generation failed manifest or digest validation".into(),
                )
            })?;
            let _ = winner;
            remove_tree(staged_dir)
                .map_err(|error| io_error("discard staging directory", error))?;
            Ok(PublishOutcome::AlreadyPublished)
        }
    }
}

pub(crate) fn request_clear(config: &NativeCacheConfig) -> io::Result<()> {
    prepare_private_directory(&config.root, PrivateDirectoryPurpose::ExecutableCache)?;
    let marker = config.root.join(".clear-on-start");
    match checked_metadata_io(&marker)? {
        Some(metadata) => {
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "native-cache clear marker is not a regular file",
                ));
            }
            Ok(())
        }
        None => {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)?;
            file.write_all(config.root.to_string_lossy().as_bytes())?;
            file.sync_all()?;
            sync_directory(&config.root)
        }
    }
}

pub(crate) fn maintain(
    config: &NativeCacheConfig,
    deadline: Instant,
) -> Result<MaintenanceReport, NativeCacheError> {
    let mut report = MaintenanceReport::default();
    if config.access == NativeCacheAccess::Disabled {
        return Ok(report);
    }
    prepare_private_directory(&config.root, PrivateDirectoryPurpose::ExecutableCache)
        .map_err(|error| io_error("prepare cache root", error))?;
    if !within_deadline(Some(deadline)) {
        report.budget_exhausted = true;
        return Ok(report);
    }

    let marker = config.root.join(".clear-on-start");
    let force = checked_metadata_io(&marker)
        .map_err(|error| io_error("inspect clear marker", error))?
        .is_some();
    if force {
        if !clear_root(config, deadline, &mut report)? {
            report.budget_exhausted = true;
            return Ok(report);
        }
        match fs::remove_file(&marker) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("consume clear marker", error)),
        }
    }
    if !within_deadline(Some(deadline)) {
        report.budget_exhausted = true;
        return Ok(report);
    }

    if !force {
        let active = namespace_path(config);
        let Some(metadata) = checked_metadata_io(&active)
            .map_err(|error| io_error("inspect active namespace", error))?
        else {
            return Ok(report);
        };
        if !metadata.is_dir() {
            return Err(NativeCacheError::UnsafePath(
                "active cache namespace is not a directory".into(),
            ));
        }
    }
    let namespace = prepare_namespace(config)?;
    if !within_deadline(Some(deadline)) {
        report.budget_exhausted = true;
        return Ok(report);
    }
    if !force && maintenance_stamp_is_recent(&namespace)? {
        report.skipped_due_to_stamp = true;
        return Ok(report);
    }

    let namespaces = list_namespaces(&config.root)?;
    for path in &namespaces {
        if !within_deadline(Some(deadline)) {
            report.budget_exhausted = true;
            return Ok(report);
        }
        clean_temporary_entries(path, deadline, &mut report)?;
        if !within_deadline(Some(deadline)) {
            report.budget_exhausted = true;
            return Ok(report);
        }
        merge_usage_journals(path, deadline, &mut report)?;
        if !within_deadline(Some(deadline)) {
            report.budget_exhausted = true;
            return Ok(report);
        }
        remove_quarantined_generations(path, deadline, &mut report)?;
        if !within_deadline(Some(deadline)) {
            report.budget_exhausted = true;
            return Ok(report);
        }
    }
    remove_inactive_namespaces(config, &namespaces, deadline, &mut report)?;
    if !within_deadline(Some(deadline)) {
        report.budget_exhausted = true;
        return Ok(report);
    }
    prune_global(config, deadline, &mut report)?;
    if !report.budget_exhausted {
        write_maintenance_stamp(&namespace)?;
    }
    Ok(report)
}

#[allow(dead_code)] // loader/shutdown tasks publish the process usage journal.
pub(crate) fn record_usage(
    config: &NativeCacheConfig,
    generation_ids: &[GenerationId],
) -> io::Result<()> {
    if generation_ids.is_empty() {
        return Ok(());
    }
    let namespace = prepare_namespace_io(config)?;
    let journals = namespace.join("journals");
    let nonce = unique_nonce();
    let temporary = journals.join(format!(".tmp-journal-{}-{nonce}.json", process_id()));
    let final_name = journals.join(format!("journal-{}-{nonce}.json", process_id()));
    let journal = UsageJournal {
        generation_ids: generation_ids
            .iter()
            .copied()
            .take(MAX_JOURNAL_IDS)
            .collect(),
        used_unix_secs: Some(unix_now()),
    };
    let bytes = serde_json::to_vec(&journal).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("encode usage journal: {error}"),
        )
    })?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    match rename_noreplace(&temporary, &final_name) {
        Ok(MoveResult::Published) => {}
        Ok(MoveResult::Unsupported) => {
            let _ = remove_tree(&temporary);
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "usage-journal publication is unsupported",
            ));
        }
        Ok(MoveResult::AlreadyExists) | Err(MoveError::AlreadyExists) => {
            let _ = remove_tree(&temporary);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "usage-journal destination already exists",
            ));
        }
        Err(MoveError::Unsupported) => {
            let _ = remove_tree(&temporary);
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "usage-journal publication is unsupported",
            ));
        }
        Err(MoveError::Io(error)) => {
            let _ = remove_tree(&temporary);
            return Err(error);
        }
    }
    sync_directory(&journals)
}

#[allow(dead_code)] // loader validation records failed content/variant pairs here.
pub(crate) fn quarantine_leaf(
    config: &NativeCacheConfig,
    content: ContentHash,
    variant: VariantHash,
) -> io::Result<()> {
    let namespace = prepare_namespace_io(config)?;
    let mut entries = read_quarantine(&namespace)?;
    entries.insert(leaf_key(content, variant));
    let bytes = serde_json::to_vec(&entries).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("encode quarantine: {error}"),
        )
    })?;
    atomic_replace(&namespace.join("quarantine.json"), &bytes)
}

fn prepare_namespace(config: &NativeCacheConfig) -> Result<PathBuf, NativeCacheError> {
    prepare_namespace_io(config).map_err(|error| io_error("prepare cache namespace", error))
}

fn prepare_namespace_io(config: &NativeCacheConfig) -> io::Result<PathBuf> {
    prepare_private_directory(&config.root, PrivateDirectoryPurpose::ExecutableCache)?;
    let namespace = namespace_path(config);
    ensure_directory(&config.root.join(runtime_target()))?;
    ensure_directory(&namespace)?;
    ensure_directory(&namespace.join("generations"))?;
    ensure_directory(&namespace.join("journals"))?;
    ensure_directory(&namespace.join("trash"))?;
    ensure_metadata_file(&namespace.join("quarantine.json"))?;
    ensure_metadata_file(&namespace.join("recency.json"))?;
    ensure_metadata_file(&namespace.join("backoff.json"))?;
    sync_directory(&namespace)?;
    Ok(namespace)
}

fn read_valid_generation(
    generation: &Path,
    id: GenerationId,
) -> Result<Option<GenerationManifest>, NativeCacheError> {
    let Some(metadata) = (match checked_metadata(generation) {
        Ok(metadata) => metadata,
        Err(NativeCacheError::UnsafePath(_)) => return Ok(None),
        Err(error) => return Err(error),
    }) else {
        return Ok(None);
    };
    if !metadata.is_dir() {
        return Ok(None);
    }
    let manifest_path = generation.join("manifest.json");
    let metadata = match checked_metadata(&manifest_path) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return Ok(None),
        Err(NativeCacheError::UnsafePath(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let size = metadata.len();
    if size > MAX_MANIFEST_BYTES as u64 {
        return Ok(None);
    }
    let bytes = match fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("read generation manifest", error)),
    };
    let manifest = match format::parse_generation_manifest(&bytes, ManifestLimits::default()) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(None),
    };
    if manifest.generation_id != id || manifest.library_file != library_basename() {
        return Ok(None);
    }
    if !super::BUILD_ID.is_empty() {
        if validate_manifest_identity(
            &manifest,
            super::BUILD_ID,
            runtime_target(),
            runtime_abi_tag(),
        )
        .is_err()
        {
            return Ok(None);
        }
    } else if manifest.target != runtime_target() || manifest.abi_tag != runtime_abi_tag() {
        return Ok(None);
    }

    let library = generation.join(library_basename());
    let library_metadata = match checked_metadata(&library) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return Ok(None),
        Err(NativeCacheError::UnsafePath(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if !library_metadata.is_file() {
        return Ok(None);
    }
    let digest = match sha256_file(&library) {
        Ok(digest) => digest,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("hash native-cache library", error)),
    };
    if digest != manifest.library_sha256 {
        return Ok(None);
    }
    Ok(Some(manifest))
}

fn sync_generation(path: &Path, manifest: &GenerationManifest) -> Result<(), NativeCacheError> {
    let library = path.join(&manifest.library_file);
    #[cfg(not(windows))]
    File::open(&library)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error("sync native-cache library", error))?;
    #[cfg(windows)]
    let _ = library;
    #[cfg(not(windows))]
    File::open(path.join("manifest.json"))
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error("sync generation manifest", error))?;
    #[cfg(windows)]
    let _ = path;
    sync_directory(path).map_err(|error| io_error("sync generation directory", error))
}

fn clear_root(
    config: &NativeCacheConfig,
    deadline: Instant,
    report: &mut MaintenanceReport,
) -> Result<bool, NativeCacheError> {
    for namespace in list_namespaces(&config.root)? {
        if !within_deadline(Some(deadline)) {
            return Ok(false);
        }
        remove_tree(&namespace).map_err(|error| io_error("clear cache root", error))?;
        report.temporary_directories_removed =
            report.temporary_directories_removed.saturating_add(1);
    }
    Ok(true)
}

fn list_namespaces(root: &Path) -> Result<Vec<PathBuf>, NativeCacheError> {
    let mut result = Vec::new();
    let targets = read_dir(root, "scan cache targets")?;
    for target_entry in targets {
        let target_name = target_entry.file_name();
        if target_name == OsStr::new(".clear-on-start") || !valid_os_name(&target_name) {
            continue;
        }
        let target = target_entry.path();
        let Some(target_metadata) = (match checked_metadata(&target) {
            Ok(metadata) => metadata,
            Err(NativeCacheError::UnsafePath(_)) => continue,
            Err(error) => return Err(error),
        }) else {
            continue;
        };
        if !target_metadata.is_dir() {
            continue;
        }
        for build_entry in read_dir(&target, "scan cache builds")? {
            if !valid_build_name(&build_entry.file_name()) {
                continue;
            }
            let namespace = build_entry.path();
            let Some(namespace_metadata) = (match checked_metadata(&namespace) {
                Ok(metadata) => metadata,
                Err(NativeCacheError::UnsafePath(_)) => continue,
                Err(error) => return Err(error),
            }) else {
                continue;
            };
            if namespace_metadata.is_dir() {
                result.push(namespace);
            }
        }
    }
    Ok(result)
}

fn clean_temporary_entries(
    namespace: &Path,
    deadline: Instant,
    report: &mut MaintenanceReport,
) -> Result<(), NativeCacheError> {
    for entry in read_dir(namespace, "scan temporary cache entries")? {
        if !within_deadline(Some(deadline)) {
            return Ok(());
        }
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(".tmp-") {
            continue;
        }
        if is_expired(&entry.path())?.unwrap_or(false) {
            remove_tree(&entry.path())
                .map_err(|error| io_error("remove expired cache temp", error))?;
            report.temporary_directories_removed += 1;
        }
    }
    let journals = namespace.join("journals");
    for entry in read_dir(&journals, "scan temporary journals")? {
        if !within_deadline(Some(deadline)) {
            return Ok(());
        }
        if entry.file_name().to_string_lossy().starts_with(".tmp-")
            && is_expired(&entry.path())?.unwrap_or(false)
        {
            remove_tree(&entry.path())
                .map_err(|error| io_error("remove expired journal temp", error))?;
            report.temporary_directories_removed += 1;
        }
    }
    let trash = namespace.join("trash");
    for entry in read_dir(&trash, "scan cache trash")? {
        if !within_deadline(Some(deadline)) {
            return Ok(());
        }
        match remove_tree(&entry.path()) {
            Ok(()) => report.trash_removed += 1,
            Err(error) if is_sharing_violation(&error) => {}
            Err(_) => {}
        }
    }
    Ok(())
}

fn merge_usage_journals(
    namespace: &Path,
    deadline: Instant,
    report: &mut MaintenanceReport,
) -> Result<(), NativeCacheError> {
    let journals = namespace.join("journals");
    let mut recency = read_recency(namespace)?;
    let mut consumed = Vec::new();
    for entry in read_dir(&journals, "merge usage journals")? {
        if !within_deadline(Some(deadline)) {
            report.budget_exhausted = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let bytes = match bounded_read(&entry.path(), MAX_MANIFEST_BYTES) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let Some((generation_ids, used_unix_secs)) = parse_usage_journal(&bytes) else {
            continue;
        };
        if generation_ids.len() > MAX_JOURNAL_IDS {
            continue;
        }
        for id in generation_ids {
            let key = id.to_string();
            let value = recency.entry(key).or_insert(0);
            if *value < used_unix_secs {
                *value = used_unix_secs;
            }
        }
        consumed.push(entry.path());
    }
    if consumed.is_empty() {
        return Ok(());
    }

    let bytes = serde_json::to_vec(&recency).map_err(|error| {
        io_error(
            "encode recency metadata",
            io::Error::new(io::ErrorKind::InvalidData, error),
        )
    })?;
    atomic_replace(&namespace.join("recency.json"), &bytes)
        .map_err(|error| io_error("write recency metadata", error))?;

    for journal in consumed {
        match fs::remove_file(journal) {
            Ok(()) => report.journals_merged += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("remove merged usage journal", error)),
        }
    }
    Ok(())
}

fn remove_quarantined_generations(
    namespace: &Path,
    deadline: Instant,
    report: &mut MaintenanceReport,
) -> Result<(), NativeCacheError> {
    let quarantined =
        read_quarantine(namespace).map_err(|error| io_error("read quarantine", error))?;
    if quarantined.is_empty() {
        return Ok(());
    }
    let generations = namespace.join("generations");
    for entry in read_dir(&generations, "scan quarantined generations")? {
        if !within_deadline(Some(deadline)) {
            return Ok(());
        }
        let Some(id) = parse_generation_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let Some(manifest) = read_valid_generation(&entry.path(), id)? else {
            continue;
        };
        if !manifest.leaves.is_empty()
            && manifest
                .leaves
                .iter()
                .all(|leaf| quarantined.contains(&leaf_key(leaf.content_hash, leaf.variant_hash)))
        {
            if move_to_trash(namespace, &entry.path(), id).is_ok() {
                report.quarantined_generations_removed += 1;
            }
        }
    }
    Ok(())
}

fn remove_inactive_namespaces(
    config: &NativeCacheConfig,
    namespaces: &[PathBuf],
    deadline: Instant,
    report: &mut MaintenanceReport,
) -> Result<(), NativeCacheError> {
    let active = namespace_path(config);
    let now = unix_now();
    for namespace in namespaces {
        if !within_deadline(Some(deadline)) {
            return Ok(());
        }
        if *namespace == active {
            continue;
        }
        let last = namespace_last_used(namespace)?;
        if now.saturating_sub(last) >= NAMESPACE_EXPIRY.as_secs() {
            match remove_tree(namespace) {
                Ok(()) => report.namespaces_removed += 1,
                Err(error) if is_sharing_violation(&error) => {}
                Err(_) => {}
            }
        }
    }
    Ok(())
}

fn prune_global(
    config: &NativeCacheConfig,
    deadline: Instant,
    report: &mut MaintenanceReport,
) -> Result<(), NativeCacheError> {
    let namespaces = list_namespaces(&config.root)?;
    let mut candidates = Vec::new();
    let mut leaves = 0usize;
    let mut bytes = 0u64;
    for namespace in namespaces {
        let recency = read_recency(&namespace)?;
        let quarantined =
            read_quarantine(&namespace).map_err(|error| io_error("read quarantine", error))?;
        let generations = namespace.join("generations");
        for entry in read_dir(&generations, "scan prune generations")? {
            if !within_deadline(Some(deadline)) {
                report.budget_exhausted = true;
                return Ok(());
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = parse_generation_name(&name) else {
                continue;
            };
            let Some(manifest) = read_manifest_only(&entry.path(), id)? else {
                continue;
            };
            let size = directory_size(&entry.path())?;
            let leaf_count = manifest
                .leaves
                .iter()
                .filter(|leaf| {
                    !quarantined.contains(&leaf_key(leaf.content_hash, leaf.variant_hash))
                })
                .count();
            let last_used = recency
                .get(&id.to_string())
                .copied()
                .unwrap_or(manifest.created_unix_secs);
            leaves = leaves.saturating_add(leaf_count);
            bytes = bytes.saturating_add(size);
            candidates.push(PruneCandidate {
                namespace: namespace.clone(),
                path: entry.path(),
                id,
                leaves: leaf_count,
                bytes: size,
                last_used,
                created: manifest.created_unix_secs,
            });
        }
        bytes = bytes.saturating_add(directory_size(&namespace.join("trash"))?);
    }

    let max_leaves = config.max_cached_leaves.min(MAX_CACHE_LEAVES);
    let max_bytes = config.max_cache_bytes.min(MAX_CACHE_BYTES);
    candidates.sort_by(|left, right| {
        left.last_used
            .cmp(&right.last_used)
            .then_with(|| left.created.cmp(&right.created))
            .then_with(|| left.id.cmp(&right.id))
    });
    for candidate in candidates {
        if leaves <= max_leaves && bytes <= max_bytes {
            break;
        }
        if !within_deadline(Some(deadline)) {
            report.budget_exhausted = true;
            break;
        }
        let Ok(trash) = move_to_trash(&candidate.namespace, &candidate.path, candidate.id) else {
            continue;
        };
        match remove_tree(&trash) {
            Ok(()) => {
                leaves = leaves.saturating_sub(candidate.leaves);
                bytes = bytes.saturating_sub(candidate.bytes);
                report.generations_pruned += 1;
            }
            Err(error) if is_sharing_violation(&error) => {}
            Err(_) => {}
        }
    }
    Ok(())
}

struct PruneCandidate {
    namespace: PathBuf,
    path: PathBuf,
    id: GenerationId,
    leaves: usize,
    bytes: u64,
    last_used: u64,
    created: u64,
}

fn move_to_trash(namespace: &Path, generation: &Path, id: GenerationId) -> io::Result<PathBuf> {
    let trash = namespace
        .join("trash")
        .join(format!(".trash-{}-{id}", unique_nonce()));
    rename_noreplace(generation, &trash).map_err(|error| match error {
        MoveError::Io(error) => error,
        MoveError::AlreadyExists => io::Error::new(io::ErrorKind::AlreadyExists, "trash collision"),
        MoveError::Unsupported => {
            io::Error::new(io::ErrorKind::Unsupported, "trash rename unsupported")
        }
    })?;
    Ok(trash)
}

fn read_manifest_only(
    generation: &Path,
    id: GenerationId,
) -> Result<Option<GenerationManifest>, NativeCacheError> {
    let Some(metadata) = (match checked_metadata(generation) {
        Ok(metadata) => metadata,
        Err(NativeCacheError::UnsafePath(_)) => return Ok(None),
        Err(error) => return Err(error),
    }) else {
        return Ok(None);
    };
    if !metadata.is_dir() {
        return Ok(None);
    }
    let path = generation.join("manifest.json");
    let metadata = match checked_metadata(&path) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return Ok(None),
        Err(NativeCacheError::UnsafePath(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Ok(None);
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("read prune manifest", error)),
    };
    match format::parse_generation_manifest(&bytes, ManifestLimits::default()) {
        Ok(manifest) if manifest.generation_id == id => Ok(Some(manifest)),
        Ok(_) | Err(_) => Ok(None),
    }
}

fn read_quarantine(namespace: &Path) -> io::Result<HashSet<String>> {
    let path = namespace.join("quarantine.json");
    let Some(bytes) = checked_read_io(&path, MAX_MANIFEST_BYTES)? else {
        return Ok(HashSet::new());
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(HashSet::new()),
    };
    let mut result = HashSet::new();
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                if let Some(key) = value.as_str().filter(|key| parse_leaf_key(key).is_some()) {
                    result.insert(key.to_owned());
                }
            }
        }
        serde_json::Value::Object(values) => {
            for (key, _) in values {
                if parse_leaf_key(&key).is_some() {
                    result.insert(key);
                }
            }
        }
        _ => {}
    }
    Ok(result)
}

fn read_recency(namespace: &Path) -> Result<BTreeMap<String, u64>, NativeCacheError> {
    let path = namespace.join("recency.json");
    let Some(bytes) = checked_read(&path, MAX_MANIFEST_BYTES)? else {
        return Ok(BTreeMap::new());
    };
    Ok(serde_json::from_slice(&bytes).unwrap_or_default())
}

fn namespace_last_used(namespace: &Path) -> Result<u64, NativeCacheError> {
    let recency = read_recency(namespace)?;
    let mut last = recency.values().copied().max().unwrap_or(0);
    let generations = namespace.join("generations");
    for entry in read_dir(&generations, "read namespace recency")? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = parse_generation_name(&name) else {
            continue;
        };
        if let Some(manifest) = read_manifest_only(&entry.path(), id)? {
            last = last.max(manifest.created_unix_secs);
        }
    }
    if last == 0 {
        last = modified_unix(namespace)?.unwrap_or(0);
    }
    Ok(last)
}

fn maintenance_stamp_is_recent(namespace: &Path) -> Result<bool, NativeCacheError> {
    let path = namespace.join(".maintenance-stamp");
    let Some(metadata) =
        checked_metadata_io(&path).map_err(|error| io_error("inspect stamp", error))?
    else {
        return Ok(false);
    };
    if !metadata.is_file() {
        return Err(NativeCacheError::UnsafePath(
            "maintenance stamp is not a regular file".into(),
        ));
    }
    let Some(modified) = modified_unix(&path)? else {
        return Ok(false);
    };
    Ok(unix_now().saturating_sub(modified) < MAINTENANCE_INTERVAL.as_secs())
}

fn write_maintenance_stamp(namespace: &Path) -> Result<(), NativeCacheError> {
    let path = namespace.join(".maintenance-stamp");
    atomic_replace(&path, unix_now().to_string().as_bytes())
        .map_err(|error| io_error("write maintenance stamp", error))
}

fn is_expired(path: &Path) -> Result<Option<bool>, NativeCacheError> {
    let Some(modified) = modified_unix(path)? else {
        return Ok(None);
    };
    Ok(Some(
        unix_now().saturating_sub(modified) >= TEMP_EXPIRY.as_secs(),
    ))
}

fn directory_size(path: &Path) -> Result<u64, NativeCacheError> {
    let Some(metadata) = (match checked_metadata(path) {
        Ok(metadata) => metadata,
        Err(NativeCacheError::UnsafePath(_)) => return Ok(0),
        Err(error) => return Err(error),
    }) else {
        return Ok(0);
    };
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in read_dir(path, "size cache directory")? {
        total = total.saturating_add(directory_size(&entry.path())?);
    }
    Ok(total)
}

fn remove_tree(path: &Path) -> io::Result<()> {
    disappear_before_metadata(path);
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return remove_link(path, &metadata);
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            remove_tree(&entry.path())?;
        }
        match fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    } else {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn remove_link(path: &Path, metadata: &Metadata) -> io::Result<()> {
    #[cfg(windows)]
    if metadata.is_dir() || is_directory_reparse(metadata) {
        return match fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_directory(path: &Path) -> io::Result<()> {
    match checked_metadata_io(path)? {
        Some(metadata) if metadata.is_dir() => Ok(()),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cache path is not a directory: {}", path.display()),
        )),
        None => {
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            match checked_metadata_io(path)? {
                Some(metadata) if metadata.is_dir() => Ok(()),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cache directory changed while creating",
                )),
            }
        }
    }
}

fn ensure_metadata_file(path: &Path) -> io::Result<()> {
    match checked_metadata_io(path)? {
        Some(metadata) if metadata.is_file() => Ok(()),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cache metadata path is not a file: {}", path.display()),
        )),
        None => {
            let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return match checked_metadata_io(path)? {
                        Some(metadata) if metadata.is_file() => Ok(()),
                        _ => Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "cache metadata path changed while creating",
                        )),
                    };
                }
                Err(error) => return Err(error),
            };
            file.write_all(b"{}")?;
            file.sync_all()
        }
    }
}

fn checked_metadata(path: &Path) -> Result<Option<Metadata>, NativeCacheError> {
    disappear_before_metadata(path);
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || is_reparse_point(&metadata) => {
            Err(NativeCacheError::UnsafePath(path.display().to_string()))
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("inspect cache path", error)),
    }
}

fn checked_metadata_io(path: &Path) -> io::Result<Option<Metadata>> {
    disappear_before_metadata(path);
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || is_reparse_point(&metadata) => {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cache path is a link or reparse point: {}", path.display()),
            ))
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &Metadata) -> bool {
    false
}

#[cfg(windows)]
fn is_directory_reparse(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x10 != 0
}

fn read_dir(path: &Path, operation: &'static str) -> Result<Vec<fs::DirEntry>, NativeCacheError> {
    let Some(metadata) = (match checked_metadata(path) {
        Ok(metadata) => metadata,
        Err(NativeCacheError::UnsafePath(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }) else {
        return Ok(Vec::new());
    };
    if !metadata.is_dir() {
        return Err(NativeCacheError::UnsafePath(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(|error| io_error(operation, error))? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(operation, error)),
        };
        match checked_metadata(&entry.path()) {
            Ok(Some(_)) => entries.push(entry),
            Ok(None) | Err(NativeCacheError::UnsafePath(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(entries)
}

fn checked_read(path: &Path, maximum: usize) -> Result<Option<Vec<u8>>, NativeCacheError> {
    checked_read_io(path, maximum).map_err(|error| io_error("read cache metadata", error))
}

fn checked_read_io(path: &Path, maximum: usize) -> io::Result<Option<Vec<u8>>> {
    let metadata = match checked_metadata_io(path) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > maximum as u64 {
        return Ok(None);
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn bounded_read(path: &Path, maximum: usize) -> io::Result<Vec<u8>> {
    checked_read_io(path, maximum)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid bounded cache file"))
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no parent")
    })?;
    if let Some(metadata) = checked_metadata_io(path)? {
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache metadata path is not a regular file",
            ));
        }
    }
    let temporary = parent.join(format!(".tmp-meta-{}-{}", process_id(), unique_nonce()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    rename_replace(&temporary, path)?;
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory synchronization unsupported",
        ))
    }
}

fn rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let from = wide_path(from);
        let to = wide_path(to);
        if unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

enum MoveResult {
    Published,
    AlreadyExists,
    Unsupported,
}

#[allow(dead_code)]
enum MoveError {
    Io(io::Error),
    AlreadyExists,
    Unsupported,
}

fn rename_noreplace(from: &Path, to: &Path) -> Result<MoveResult, MoveError> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let from = CString::new(from.as_os_str().as_bytes()).map_err(|_| {
            MoveError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL in cache path",
            ))
        })?;
        let to = CString::new(to.as_os_str().as_bytes()).map_err(|_| {
            MoveError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL in cache path",
            ))
        })?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                1u32,
            )
        };
        if result == 0 {
            return Ok(MoveResult::Published);
        }
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code) if code == libc::EEXIST => Err(MoveError::AlreadyExists),
            Some(code)
                if code == libc::ENOSYS
                    || code == libc::EINVAL
                    || code == libc::EOPNOTSUPP
                    || code == libc::ENOTSUP =>
            {
                Ok(MoveResult::Unsupported)
            }
            _ => Err(MoveError::Io(error)),
        };
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, GetLastError,
        };
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let from = wide_path(from);
        let to = wide_path(to);
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } != 0 {
            return Ok(MoveResult::Published);
        }
        let code = unsafe { GetLastError() };
        if code == ERROR_ALREADY_EXISTS || code == ERROR_FILE_EXISTS {
            return Err(MoveError::AlreadyExists);
        }
        return Err(MoveError::Io(io::Error::from_raw_os_error(code as i32)));
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (from, to);
        Ok(MoveResult::Unsupported)
    }
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', ':'])
        && !value.chars().any(char::is_control)
}

fn valid_os_name(value: &OsStr) -> bool {
    value.to_str().is_some_and(valid_component)
}

fn valid_build_name(value: &OsStr) -> bool {
    value.to_str().is_some_and(|value| {
        value == "unsupported"
            || (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    })
}

fn parse_generation_name(value: &str) -> Option<GenerationId> {
    GenerationId::parse_directory_name(value)
}

fn parse_leaf_key(value: &str) -> Option<(ContentHash, VariantHash)> {
    let (content, variant) = value.split_once('/')?;
    Some((content.parse().ok()?, variant.parse().ok()?))
}

fn leaf_key(content: ContentHash, variant: VariantHash) -> String {
    format!("{content}/{variant}")
}

fn modified_unix(path: &Path) -> Result<Option<u64>, NativeCacheError> {
    let Some(metadata) = (match checked_metadata(path) {
        Ok(metadata) => metadata,
        Err(NativeCacheError::UnsafePath(_)) => return Ok(None),
        Err(error) => return Err(error),
    }) else {
        return Ok(None);
    };
    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("read cache timestamp", error)),
    };
    Ok(Some(
        modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unique_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn process_id() -> u32 {
    std::process::id()
}

fn within_deadline(deadline: Option<Instant>) -> bool {
    deadline.is_none_or(|deadline| Instant::now() < deadline)
}

fn io_error(operation: &'static str, error: io::Error) -> NativeCacheError {
    NativeCacheError::Io {
        operation,
        kind: error.kind(),
        message: error.to_string(),
    }
}

fn parse_usage_journal(bytes: &[u8]) -> Option<(Vec<GenerationId>, u64)> {
    if let Ok(journal) = serde_json::from_slice::<UsageJournal>(bytes) {
        return Some((
            journal.generation_ids,
            journal.used_unix_secs.unwrap_or_else(unix_now),
        ));
    }
    serde_json::from_slice::<Vec<GenerationId>>(bytes)
        .ok()
        .map(|generation_ids| (generation_ids, unix_now()))
}

fn is_sharing_violation(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(windows)]
    {
        return matches!(error.raw_os_error(), Some(32 | 33));
    }
    #[cfg(not(windows))]
    {
        false
    }
}
