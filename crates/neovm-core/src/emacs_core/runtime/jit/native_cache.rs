//! Process-local model for the persistent native-code cache.
//!
//! This module deliberately stops at configuration, identity, manifest
//! validation, and lookup bookkeeping. Storage, dynamic loading, and host
//! lifecycle integration are implemented by later cache tasks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use super::stats::NativeCacheCounters;
use crate::emacs_core::bytecode::ByteCodeFunction;
use crate::emacs_core::intern::{SymId, resolve_name, symbol_name_id};
use crate::emacs_core::symbol::Obarray;
use crate::emacs_core::value::Value;

#[path = "native_cache/format.rs"]
pub mod format;
#[path = "native_cache/storage.rs"]
pub(crate) mod storage;

pub use format::{
    ContentHash, FunctionPrekey, GenerationId, GenerationManifest, ManifestError, ManifestLeaf,
    ManifestLimits, VariantHash, parse_generation_manifest,
};
#[allow(unused_imports)]
pub(crate) use format::{
    GenerationIndex, IndexedGeneration, IndexedLeaf, validate_manifest_identity,
};

#[cfg(test)]
#[path = "native_cache_test.rs"]
mod tests;

pub(crate) const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_MANIFEST_LEAVES: usize = 128;
pub(crate) const MAX_CANDIDATES: usize = 4;
pub(crate) const MAX_DESCRIPTOR_BYTES: u32 = 4 * 1024 * 1024;
pub(crate) const MAX_RELOC_RECIPE_BYTES: u32 = 1024 * 1024;
pub(crate) const MAX_SPEC_SITES: u32 = 64 * 1024;

/// Result of the one-shot prewarmed lookup. A miss is deliberately not a
/// cache entry: the caller must clear the marker and return to ordinary
/// heat-driven dispatch.
pub(crate) enum NativeCacheLookup {
    Hit(Rc<super::compile::CompiledLeaf>),
    Miss,
}

const BUILD_ID: &str = match option_env!("NEOMACS_NATIVE_CACHE_BUILD_ID") {
    Some(value) => value,
    None => "",
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCacheAccess {
    Disabled,
    ReadOnly,
    ReadWrite,
}

impl Default for NativeCacheAccess {
    fn default() -> Self {
        Self::Disabled
    }
}

#[derive(Clone, Debug)]
pub struct NativeCacheConfig {
    pub access: NativeCacheAccess,
    pub root: PathBuf,
    pub toolchain_dir: PathBuf,
    pub active_index_budget: Duration,
    pub maintenance_budget: Duration,
    pub emit_budget: Duration,
    pub max_emit_leaves: usize,
    pub max_cached_leaves: usize,
    pub max_cache_bytes: u64,
}

impl NativeCacheConfig {
    pub fn for_paths(root: PathBuf, toolchain_dir: PathBuf, access: NativeCacheAccess) -> Self {
        Self {
            access,
            root,
            toolchain_dir,
            active_index_budget: Duration::from_millis(50),
            maintenance_budget: Duration::from_millis(50),
            emit_budget: Duration::from_secs(2),
            max_emit_leaves: 128,
            max_cached_leaves: 4_096,
            max_cache_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCacheInitReport {
    pub access: NativeCacheAccess,
    pub supported: bool,
    pub root: PathBuf,
    pub namespace: String,
    pub indexed_generations: u64,
    pub indexed_leaves: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeCacheError {
    InvalidManifest(ManifestError),
    Io {
        operation: &'static str,
        kind: std::io::ErrorKind,
        message: String,
    },
    UnsafePath(String),
    InvalidPublication(String),
}

impl std::fmt::Display for NativeCacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidManifest(error) => write!(f, "invalid native-cache manifest: {error}"),
            Self::Io {
                operation, message, ..
            } => write!(f, "native-cache {operation} failed: {message}"),
            Self::UnsafePath(path) => write!(f, "unsafe native-cache path: {path}"),
            Self::InvalidPublication(message) => {
                write!(f, "invalid native-cache publication: {message}")
            }
        }
    }
}

impl std::error::Error for NativeCacheError {}

impl From<ManifestError> for NativeCacheError {
    fn from(error: ManifestError) -> Self {
        Self::InvalidManifest(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCacheStatus {
    pub access: NativeCacheAccess,
    pub root: PathBuf,
    pub namespace: String,
    pub indexed_leaves: u64,
    pub indexed_generations: u64,
    pub loaded_leaves: u64,
    pub loaded_generations: u64,
    pub hits: u64,
    pub misses: u64,
    pub validation_failures: u64,
    pub emitted_leaves: u64,
    pub skipped_leaves: u64,
    pub bytes: u64,
    pub active_index_budget_exhausted: bool,
    pub maintenance_budget_exhausted: bool,
    pub emit_budget_exhausted: bool,
    pub last_error: Option<String>,
}

#[allow(dead_code)] // storage and prewarm tasks consume the retained snapshot.
#[derive(Clone, Debug)]
struct NativeCacheState {
    access: NativeCacheAccess,
    root: PathBuf,
    toolchain_dir: PathBuf,
    namespace: String,
    manifests: Vec<GenerationManifest>,
    index: GenerationIndex,
    prekey_map: HashMap<FunctionPrekey, Vec<IndexedLeaf>>,
    counters: NativeCacheCounters,
    active_index_budget_exhausted: bool,
    maintenance_budget_exhausted: bool,
    emit_budget_exhausted: bool,
    last_error: Option<String>,
}

impl Default for NativeCacheState {
    fn default() -> Self {
        Self {
            access: NativeCacheAccess::Disabled,
            root: PathBuf::new(),
            toolchain_dir: PathBuf::new(),
            namespace: String::new(),
            manifests: Vec::new(),
            index: GenerationIndex::default(),
            prekey_map: HashMap::new(),
            counters: NativeCacheCounters::default(),
            active_index_budget_exhausted: false,
            maintenance_budget_exhausted: false,
            emit_budget_exhausted: false,
            last_error: None,
        }
    }
}

static STATE: LazyLock<RwLock<NativeCacheState>> =
    LazyLock::new(|| RwLock::new(NativeCacheState::default()));

#[cfg(test)]
static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

#[cfg(test)]
thread_local! {
    static LOOKUP_FOR_TEST: std::cell::RefCell<
        Option<Box<dyn Fn(&IndexedLeaf, &ByteCodeFunction, &Obarray) -> NativeCacheLookup>>,
    > = const { std::cell::RefCell::new(None) };
}

pub fn initialize(config: NativeCacheConfig) -> Result<NativeCacheInitReport, NativeCacheError> {
    let supported = build_supported() && !BUILD_ID.is_empty();
    let requested_access = if supported {
        config.access
    } else {
        NativeCacheAccess::Disabled
    };
    let namespace = supported.then(namespace).unwrap_or_default();
    let mut access = requested_access;
    let mut last_error =
        (!supported).then(|| "native-cache build support is unavailable".to_owned());
    let mut index = GenerationIndex::default();

    if access != NativeCacheAccess::Disabled {
        if let Err(error) = storage::maintain(&config, Instant::now() + config.maintenance_budget) {
            last_error = Some(error.to_string());
        }
        match storage::open_namespace(&config) {
            Ok(opened) => index = opened,
            Err(error) => {
                access = NativeCacheAccess::Disabled;
                last_error = Some(error.to_string());
            }
        }
    }

    let mut prekey_map = HashMap::new();
    for leaf in index
        .generations
        .iter()
        .flat_map(|generation| generation.leaves.iter())
    {
        prekey_map
            .entry(leaf.prekey.clone())
            .or_insert_with(Vec::new)
            .push(leaf.clone());
    }
    let indexed_generations = index.generations.len() as u64;
    let indexed_leaves = index
        .generations
        .iter()
        .map(|generation| generation.leaves.len() as u64)
        .sum();

    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *state = NativeCacheState {
        access,
        root: config.root.clone(),
        toolchain_dir: config.toolchain_dir,
        namespace: namespace.clone(),
        manifests: Vec::new(),
        index,
        prekey_map,
        counters: NativeCacheCounters {
            indexed_generations,
            indexed_leaves,
            ..NativeCacheCounters::default()
        },
        active_index_budget_exhausted: false,
        maintenance_budget_exhausted: false,
        emit_budget_exhausted: false,
        last_error: last_error.clone(),
    };

    Ok(NativeCacheInitReport {
        access,
        supported,
        root: config.root,
        namespace,
        indexed_generations,
        indexed_leaves,
        last_error,
    })
}

pub fn status() -> NativeCacheStatus {
    let state = STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    NativeCacheStatus {
        access: state.access,
        root: state.root.clone(),
        namespace: state.namespace.clone(),
        indexed_leaves: state.counters.indexed_leaves,
        indexed_generations: state.counters.indexed_generations,
        loaded_leaves: state.counters.loaded_leaves,
        loaded_generations: state.counters.loaded_generations,
        hits: state.counters.hits,
        misses: state.counters.misses,
        validation_failures: state.counters.validation_failures,
        emitted_leaves: state.counters.emitted_leaves,
        skipped_leaves: state.counters.skipped_leaves,
        bytes: state.counters.bytes,
        active_index_budget_exhausted: state.active_index_budget_exhausted,
        maintenance_budget_exhausted: state.maintenance_budget_exhausted,
        emit_budget_exhausted: state.emit_budget_exhausted,
        last_error: state.last_error.clone(),
    }
}

pub fn reset_for_test() {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *state = NativeCacheState::default();
    #[cfg(test)]
    LOOKUP_FOR_TEST.with(|lookup| lookup.borrow_mut().take());
}

/// Install the test-only loader seam used before the real native library
/// loader is added. The callback receives each exact indexed candidate in
/// newest-first order, up to [`MAX_CANDIDATES`].
#[cfg(test)]
pub(crate) fn install_lookup_for_test(
    lookup: impl Fn(&IndexedLeaf, &ByteCodeFunction, &Obarray) -> NativeCacheLookup + 'static,
) {
    LOOKUP_FOR_TEST.with(|slot| *slot.borrow_mut() = Some(Box::new(lookup)));
}

/// Full content/variant lookup for a function whose cheap prekey matched.
///
/// The content hash is the existing canonical AOT body hash. Task 5 has no
/// loader or speculation emitter yet, so the variant remains the neutral
/// variant used by the injected loader seam; Task 7 supplies the production
/// variant classification and loading implementation without changing this
/// dispatch contract.
pub(crate) fn try_load_prewarmed(func: &ByteCodeFunction, obarray: &Obarray) -> NativeCacheLookup {
    let Some(content) = super::aot::leaf_content_hash(
        func.executable_ops(),
        &func.constants,
        func.params.required.len(),
    ) else {
        record_lookup_miss();
        return NativeCacheLookup::Miss;
    };
    let content = ContentHash(content);
    let variant = VariantHash(0);
    let (attempted, hit) = {
        let state = STATE
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut attempted = false;
        let mut hit = None;
        for candidate in select_generation_candidates(&state.index, content, variant) {
            attempted = true;
            #[cfg(test)]
            let result = LOOKUP_FOR_TEST.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .map(|lookup| lookup(candidate, func, obarray))
                    .unwrap_or(NativeCacheLookup::Miss)
            });
            #[cfg(not(test))]
            let result = NativeCacheLookup::Miss;
            if let NativeCacheLookup::Hit(leaf) = result {
                hit = Some(leaf);
                break;
            }
        }
        (attempted, hit)
    };
    if let Some(leaf) = hit {
        record_lookup_hit();
        return NativeCacheLookup::Hit(leaf);
    }
    if !attempted {
        record_lookup_miss();
        return NativeCacheLookup::Miss;
    }
    record_lookup_miss();
    NativeCacheLookup::Miss
}

/// Mark a newly published named bytecode function when its cheap prekey is
/// present in the active index. No body hashing or loader work occurs here.
pub(crate) fn on_function_published(_obarray: &Obarray, sym: SymId, function: Value) {
    if !function.is_bytecode() {
        return;
    }
    let name = resolve_name(symbol_name_id(sym));
    let state = STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !state.prekey_map.keys().any(|prekey| prekey.name == name) {
        return;
    }
    if function.bytecode_params_required_only_probe() != Some(true) {
        return;
    }
    let Some(func) = function.get_bytecode_data() else {
        return;
    };
    if prekey_matches(&state.prekey_map, name, func.params.required.len(), func) {
        func.jit_runtime().mark_aot_prewarmed();
    }
}

fn prekey_matches(
    prekey_map: &HashMap<FunctionPrekey, Vec<IndexedLeaf>>,
    name: &str,
    arity: usize,
    func: &ByteCodeFunction,
) -> bool {
    if !prekey_map
        .keys()
        .any(|prekey| prekey.name == name && prekey.arity == arity)
    {
        return false;
    }
    let ops_len = func.executable_ops().len();
    prekey_map
        .keys()
        .any(|prekey| prekey.name == name && prekey.arity == arity && prekey.ops_len == ops_len)
}

/// Result of the bounded post-pdump prekey walk.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrewarmReport {
    pub candidates: usize,
    pub marked: usize,
    pub budget_exhausted: bool,
}

/// Mark pdump-resident named bytecode functions without hashing or loading.
pub fn prewarm_after_pdump(ctx: &crate::emacs_core::eval::Context) -> PrewarmReport {
    let state = STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.prekey_map.is_empty() {
        return PrewarmReport::default();
    }

    let deadline = Instant::now() + Duration::from_millis(50);
    let mut report = PrewarmReport::default();
    for (name_id, function) in ctx.obarray.interned_function_cells_with_names() {
        if Instant::now() >= deadline {
            report.budget_exhausted = true;
            break;
        }
        if !function.is_bytecode() {
            continue;
        }
        let name = resolve_name(name_id);
        if !state.prekey_map.keys().any(|prekey| prekey.name == name) {
            continue;
        }
        if function.bytecode_params_required_only_probe() != Some(true) {
            continue;
        }
        let Some(func) = function.get_bytecode_data() else {
            continue;
        };
        if !func.params.optional.is_empty() || func.params.rest.is_some() {
            continue;
        }
        report.candidates += 1;
        if prekey_matches(&state.prekey_map, name, func.params.required.len(), func) {
            func.jit_runtime().mark_aot_prewarmed();
            report.marked += 1;
        }
    }
    drop(state);
    if report.budget_exhausted {
        mark_budget_exhausted(true, false, false);
    }
    report
}

/// Select exact content/variant matches in newest-first order. The small
/// temporary reference vector makes the cap explicit without exposing storage
/// layout or requiring the index to be pre-sorted.
pub(crate) fn select_generation_candidates<'a>(
    index: &'a GenerationIndex,
    content: ContentHash,
    variant: VariantHash,
) -> impl Iterator<Item = &'a IndexedLeaf> {
    let mut candidates = index
        .generations
        .iter()
        .flat_map(|generation| generation.leaves.iter())
        .filter(|leaf| leaf.content_hash == content && leaf.variant_hash == variant)
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .created_unix_secs
            .cmp(&left.created_unix_secs)
            .then_with(|| right.generation_id.cmp(&left.generation_id))
    });
    candidates.into_iter().take(MAX_CANDIDATES)
}

#[allow(dead_code)] // storage will publish validated manifests through this seam.
pub(crate) fn install_manifests(manifests: Vec<GenerationManifest>) {
    let index = GenerationIndex::from_manifests(manifests.clone());
    let mut prekey_map = HashMap::new();
    for leaf in index
        .generations
        .iter()
        .flat_map(|generation| generation.leaves.iter())
    {
        prekey_map
            .entry(leaf.prekey.clone())
            .or_insert_with(Vec::new)
            .push(leaf.clone());
    }
    install_index_parts(manifests, index, prekey_map);
}

pub(crate) fn install_index(index: GenerationIndex) {
    let mut prekey_map = HashMap::new();
    for leaf in index
        .generations
        .iter()
        .flat_map(|generation| generation.leaves.iter())
    {
        prekey_map
            .entry(leaf.prekey.clone())
            .or_insert_with(Vec::new)
            .push(leaf.clone());
    }
    install_index_parts(Vec::new(), index, prekey_map);
}

pub(crate) fn candidates_for_prekey(prekey: &FunctionPrekey) -> Vec<IndexedLeaf> {
    let state = STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.prekey_map.get(prekey).cloned().unwrap_or_default()
}

fn install_index_parts(
    manifests: Vec<GenerationManifest>,
    index: GenerationIndex,
    prekey_map: HashMap<FunctionPrekey, Vec<IndexedLeaf>>,
) {
    let indexed_generations = index.generations.len() as u64;
    let indexed_leaves = index
        .generations
        .iter()
        .map(|generation| generation.leaves.len() as u64)
        .sum();
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.manifests = manifests;
    state.index = index;
    state.prekey_map = prekey_map;
    state.counters.indexed_generations = indexed_generations;
    state.counters.indexed_leaves = indexed_leaves;
}

pub(crate) fn record_lookup_hit() {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.counters.hits = state.counters.hits.saturating_add(1);
}

pub(crate) fn record_lookup_miss() {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.counters.misses = state.counters.misses.saturating_add(1);
}

pub(crate) fn record_loaded(leaves: u64, generations: u64) {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.counters.loaded_leaves = state.counters.loaded_leaves.saturating_add(leaves);
    state.counters.loaded_generations = state
        .counters
        .loaded_generations
        .saturating_add(generations);
}

pub(crate) fn record_validation_failure(error: impl Into<String>) {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.counters.validation_failures = state.counters.validation_failures.saturating_add(1);
    state.last_error = Some(error.into());
}

pub(crate) fn record_emitted(leaves: u64, bytes: u64) {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.counters.emitted_leaves = state.counters.emitted_leaves.saturating_add(leaves);
    state.counters.bytes = state.counters.bytes.saturating_add(bytes);
}

pub(crate) fn record_skipped(leaves: u64) {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.counters.skipped_leaves = state.counters.skipped_leaves.saturating_add(leaves);
}

pub(crate) fn mark_budget_exhausted(active_index: bool, maintenance: bool, emit: bool) {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.active_index_budget_exhausted |= active_index;
    state.maintenance_budget_exhausted |= maintenance;
    state.emit_budget_exhausted |= emit;
}

fn namespace() -> String {
    let target_env = if cfg!(target_env = "msvc") {
        "msvc"
    } else if cfg!(target_env = "gnu") {
        "gnu"
    } else {
        "unknown"
    };
    format!(
        "{}-{}-{}-v{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        target_env,
        format::FORMAT_VERSION,
        BUILD_ID
    )
}

fn build_supported() -> bool {
    option_env!("NEOMACS_NATIVE_CACHE_SUPPORTED").is_some_and(|value| value == "1")
}
