use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::emacs_core::bytecode::ByteCodeFunction;
use crate::emacs_core::bytecode::opcode::Op;
use crate::emacs_core::intern::SymId;
use crate::emacs_core::value::LambdaParams;

const CONTENT: ContentHash = ContentHash(0x11);
const VARIANT: VariantHash = VariantHash(0x22);

fn state_test_lock() -> std::sync::MutexGuard<'static, ()> {
    super::test_lock()
}

#[test]
fn production_defaults_match_the_spec() {
    let cfg = NativeCacheConfig::for_paths(
        PathBuf::from("cache"),
        PathBuf::from("runtime/share/neomacs/native-cache"),
        NativeCacheAccess::ReadWrite,
    );
    assert_eq!(cfg.active_index_budget, Duration::from_millis(50));
    assert_eq!(cfg.maintenance_budget, Duration::from_millis(50));
    assert_eq!(cfg.emit_budget, Duration::from_secs(2));
    assert_eq!(cfg.max_emit_leaves, 128);
    assert_eq!(cfg.max_cached_leaves, 4_096);
    assert_eq!(cfg.max_cache_bytes, 512 * 1024 * 1024);
}

#[test]
fn unsupported_builds_do_not_enable_the_cache() {
    let _lock = state_test_lock();
    let root = tempfile::tempdir().expect("create native-cache test directory");
    reset_for_test();
    let report = initialize(NativeCacheConfig::for_paths(
        root.path().join("cache"),
        root.path().join("toolchain"),
        NativeCacheAccess::ReadWrite,
    ))
    .expect("initialization should degrade cleanly");
    let current = status();
    assert_eq!(report.access, current.access);
    assert_eq!(
        current.access,
        if report.supported {
            NativeCacheAccess::ReadWrite
        } else {
            NativeCacheAccess::Disabled
        }
    );
    reset_for_test();
}

#[test]
fn exact_variant_lookup_is_newest_first_and_capped_at_four() {
    let index = index_with_duplicate_key_generations(6);
    let ids: Vec<_> = select_generation_candidates(&index, CONTENT, VARIANT)
        .map(|leaf| leaf.generation_id)
        .collect();
    assert_eq!(
        ids,
        vec![
            GenerationId(6),
            GenerationId(5),
            GenerationId(4),
            GenerationId(3)
        ]
    );
}

#[test]
fn candidate_selection_requires_both_hashes() {
    let mut index = index_with_duplicate_key_generations(1);
    index.generations[0].leaves.push(IndexedLeaf {
        generation_id: GenerationId(99),
        created_unix_secs: 99,
        prekey: FunctionPrekey::new("other", 1, 1),
        content_hash: CONTENT,
        variant_hash: VariantHash(0xdead),
        arity: 1,
        entry_symbol: "entry".into(),
        descriptor_symbol: "descriptor".into(),
        descriptor_bytes: 0,
        reloc_recipe_bytes: 0,
        spec_site_count: 0,
    });
    let ids: Vec<_> = select_generation_candidates(&index, CONTENT, VARIANT)
        .map(|leaf| leaf.generation_id)
        .collect();
    assert_eq!(ids, vec![GenerationId(1)]);
}

#[test]
fn prewarmed_lookup_attempts_at_most_four_exact_candidates() {
    let _lock = state_test_lock();
    reset_for_test();

    let mut func = ByteCodeFunction::new(LambdaParams {
        required: vec![SymId(1)],
        optional: Vec::new(),
        rest: None,
    });
    func.lexical = true;
    func.ops = vec![Op::StackRef(0), Op::Return];
    func.max_stack = 4;
    let content = crate::emacs_core::jit::aot::leaf_content_hash(
        &func.ops,
        &func.constants,
        func.params.required.len(),
    )
    .expect("test body has a canonical content hash");

    let index = (1..=6)
        .map(|id| IndexedGeneration {
            generation_id: GenerationId(id),
            created_unix_secs: id as u64,
            leaves: vec![IndexedLeaf {
                generation_id: GenerationId(id),
                created_unix_secs: id as u64,
                prekey: FunctionPrekey::new("demo", 1, func.ops.len()),
                content_hash: ContentHash(content),
                variant_hash: VariantHash(0),
                arity: 1,
                entry_symbol: "entry".into(),
                descriptor_symbol: "descriptor".into(),
                descriptor_bytes: 0,
                reloc_recipe_bytes: 0,
                spec_site_count: 0,
            }],
        })
        .collect();
    install_index(GenerationIndex { generations: index });

    let attempted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = std::rc::Rc::clone(&attempted);
    install_lookup_for_test(move |leaf, _, _| {
        seen.borrow_mut().push(leaf.generation_id);
        NativeCacheLookup::Miss
    });

    let obarray = crate::emacs_core::symbol::Obarray::new();
    assert!(matches!(
        try_load_prewarmed(&func, &obarray),
        NativeCacheLookup::Miss
    ));
    assert_eq!(
        *attempted.borrow(),
        vec![
            GenerationId(6),
            GenerationId(5),
            GenerationId(4),
            GenerationId(3)
        ]
    );
    reset_for_test();
}

#[test]
fn unrelated_publication_does_not_decode_lazy_bytecode() {
    let _lock = state_test_lock();
    reset_for_test();

    let mut func = ByteCodeFunction::new(LambdaParams::simple(vec![]));
    func.ops = vec![Op::Return];
    func.gnu_bytecode_bytes = Some(crate::tagged::header::LispByteVec::owned(vec![135]));
    func.defer_gnu_decode();
    let function = crate::emacs_core::value::Value::make_bytecode(func);
    let sym = crate::emacs_core::intern::intern("unrelated-publication");
    install_index(GenerationIndex {
        generations: vec![IndexedGeneration {
            generation_id: GenerationId(1),
            created_unix_secs: 1,
            leaves: vec![IndexedLeaf {
                generation_id: GenerationId(1),
                created_unix_secs: 1,
                prekey: FunctionPrekey::new("different-name", 0, 1),
                content_hash: ContentHash(1),
                variant_hash: VariantHash(0),
                arity: 0,
                entry_symbol: "entry".into(),
                descriptor_symbol: "descriptor".into(),
                descriptor_bytes: 0,
                reloc_recipe_bytes: 0,
                spec_site_count: 0,
            }],
        }],
    });

    let obarray = crate::emacs_core::symbol::Obarray::new();
    on_function_published(&obarray, sym, function);
    assert!(
        function
            .get_bytecode_data()
            .expect("bytecode")
            .resident_ops()
            .is_empty(),
        "an unrelated prekey must not force lazy GNU decoding"
    );
    reset_for_test();
}

#[test]
fn patched_prefix_clears_marker_without_persistent_lookup() {
    let _lock = state_test_lock();
    reset_for_test();
    crate::emacs_core::jit::cache::clear();

    let mut ev = crate::emacs_core::eval::Context::new();
    let mut func = ByteCodeFunction::new(LambdaParams {
        required: vec![SymId(1)],
        optional: Vec::new(),
        rest: None,
    });
    func.lexical = true;
    func.ops = vec![Op::StackRef(0), Op::Return];
    func.max_stack = 4;
    let content = crate::emacs_core::jit::aot::leaf_content_hash(
        &func.ops,
        &func.constants,
        func.params.required.len(),
    )
    .expect("test body has a canonical content hash");
    install_index(GenerationIndex {
        generations: vec![IndexedGeneration {
            generation_id: GenerationId(1),
            created_unix_secs: 1,
            leaves: vec![IndexedLeaf {
                generation_id: GenerationId(1),
                created_unix_secs: 1,
                prekey: FunctionPrekey::new("patched", 1, 2),
                content_hash: ContentHash(content),
                variant_hash: VariantHash(0),
                arity: 1,
                entry_symbol: "entry".into(),
                descriptor_symbol: "descriptor".into(),
                descriptor_bytes: 0,
                reloc_recipe_bytes: 0,
                spec_site_count: 0,
            }],
        }],
    });
    let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let seen = std::rc::Rc::clone(&calls);
    install_lookup_for_test(move |_, _, _| {
        seen.set(seen.get() + 1);
        NativeCacheLookup::Miss
    });
    func.jit_runtime().note_patched_prefix(1);
    func.jit_runtime().mark_native_cache_prewarmed();
    let id = func.jit_runtime().compiled_id_or_assign();
    assert_eq!(
        crate::emacs_core::jit::cache::try_run_compiled(
            &mut ev as *mut crate::emacs_core::eval::Context,
            &func,
            Value::NIL,
            &[Value::make_int(7)],
        )
        .unwrap(),
        None
    );
    assert!(!func.jit_runtime().is_aot_prewarmed());
    assert_eq!(calls.get(), 0);
    assert!(!crate::emacs_core::jit::cache::is_compiled_for_test(id));

    reset_for_test();
    crate::emacs_core::jit::cache::clear();
}

#[test]
fn status_reports_stable_cache_counters() {
    let _lock = state_test_lock();
    reset_for_test();
    install_index(index_with_duplicate_key_generations(2));
    assert_eq!(
        candidates_for_prekey(&FunctionPrekey::new("demo", 1, 2)).len(),
        2
    );
    record_lookup_hit();
    record_lookup_miss();
    record_loaded(1, 1);
    record_validation_failure("bad descriptor");
    record_emitted(2, 3);
    record_skipped(4);
    mark_budget_exhausted(true, false, true);

    let current = status();
    assert_eq!(current.indexed_generations, 2);
    assert_eq!(current.indexed_leaves, 2);
    assert_eq!(current.loaded_leaves, 1);
    assert_eq!(current.loaded_generations, 1);
    assert_eq!(current.hits, 1);
    assert_eq!(current.misses, 1);
    assert_eq!(current.validation_failures, 1);
    assert_eq!(current.emitted_leaves, 2);
    assert_eq!(current.skipped_leaves, 4);
    assert_eq!(current.bytes, 3);
    assert!(current.active_index_budget_exhausted);
    assert!(!current.maintenance_budget_exhausted);
    assert!(current.emit_budget_exhausted);
    assert_eq!(current.last_error.as_deref(), Some("bad descriptor"));
}

fn index_with_duplicate_key_generations(count: u128) -> GenerationIndex {
    let generations = (1..=count)
        .map(|id| IndexedGeneration {
            generation_id: GenerationId(id),
            created_unix_secs: id as u64,
            leaves: vec![IndexedLeaf {
                generation_id: GenerationId(id),
                created_unix_secs: id as u64,
                prekey: FunctionPrekey::new("demo", 1, 2),
                content_hash: CONTENT,
                variant_hash: VARIANT,
                arity: 1,
                entry_symbol: "entry".into(),
                descriptor_symbol: "descriptor".into(),
                descriptor_bytes: 0,
                reloc_recipe_bytes: 0,
                spec_site_count: 0,
            }],
        })
        .collect();
    GenerationIndex { generations }
}
