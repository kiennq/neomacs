use super::*;

fn valid_manifest_with_leaves(leaf_count: usize) -> Vec<u8> {
    let leaves = (0..leaf_count)
        .map(|index| ManifestLeaf {
            prekey: FunctionPrekey::new(format!("demo-{index}"), 1, 1),
            content_hash: ContentHash::from_u128(index as u128 + 1),
            variant_hash: VariantHash::from_u128(index as u128 + 1_000),
            arity: 1,
            entry_symbol: format!("entry_{index}"),
            descriptor_symbol: format!("descriptor_{index}"),
            descriptor_bytes: 0,
            reloc_recipe_bytes: 0,
            spec_site_count: 0,
        })
        .collect();
    serde_json::to_vec(&GenerationManifest {
        format_version: FORMAT_VERSION,
        generation_id: GenerationId::from_u128(1),
        build_id: "a".repeat(64),
        abi_tag: 1,
        target: "x86_64-unknown-linux-gnu".into(),
        library_file: "generation.so".into(),
        library_sha256: "b".repeat(64),
        created_unix_secs: 1,
        leaves,
    })
    .unwrap()
}

#[test]
fn manifest_rejects_excessive_leaf_count() {
    let bytes = valid_manifest_with_leaves(2);
    let error = parse_generation_manifest(&bytes, ManifestLimits { max_leaves: 1 }).unwrap_err();
    assert_eq!(
        error,
        ManifestError::TooManyLeaves {
            actual: 2,
            maximum: 1
        }
    );
}

#[test]
fn manifest_rejects_leaf_count_above_hard_clamp() {
    let bytes = valid_manifest_with_leaves(129);
    let error = parse_generation_manifest(
        &bytes,
        ManifestLimits {
            max_leaves: usize::MAX,
        },
    )
    .unwrap_err();
    assert_eq!(
        error,
        ManifestError::TooManyLeaves {
            actual: 129,
            maximum: MAX_MANIFEST_LEAVES
        }
    );
}

#[test]
fn manifest_rejects_more_than_one_mebibyte() {
    let bytes = vec![b' '; MAX_MANIFEST_BYTES + 1];
    assert!(parse_generation_manifest(&bytes, ManifestLimits::default()).is_err());
}

#[test]
fn manifest_rejects_unknown_fields() {
    let bytes = br#"{
        "format_version":1,
        "generation_id":"00000000000000000000000000000001",
        "build_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "abi_tag":1,
        "target":"x86_64-unknown-linux-gnu",
        "library_file":"generation.so",
        "library_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "created_unix_secs":1,
        "leaves":[],
        "unexpected":true
    }"#;
    assert!(parse_generation_manifest(bytes, ManifestLimits::default()).is_err());
}

#[test]
fn manifest_rejects_invalid_hash_widths() {
    let bytes = br#"{
        "format_version":1,
        "generation_id":"00000000000000000000000000000001",
        "build_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "abi_tag":1,
        "target":"x86_64-unknown-linux-gnu",
        "library_file":"generation.so",
        "library_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "created_unix_secs":1,
        "leaves":[]
    }"#;
    assert!(parse_generation_manifest(bytes, ManifestLimits::default()).is_err());
}

#[test]
fn manifest_round_trips_typed_hashes() {
    let bytes = br#"{
        "format_version":1,
        "generation_id":"00000000000000000000000000000001",
        "build_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "abi_tag":1,
        "target":"x86_64-unknown-linux-gnu",
        "library_file":"generation.so",
        "library_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "created_unix_secs":1,
        "leaves":[{
            "prekey":{"name":"demo","arity":1,"ops_len":1},
            "content_hash":"00000000000000000000000000000001",
            "variant_hash":"00000000000000000000000000000002",
            "arity":1,
            "entry_symbol":"entry",
            "descriptor_symbol":"descriptor",
            "descriptor_bytes":0,
            "reloc_recipe_bytes":0,
            "spec_site_count":0
        }]
    }"#;
    let manifest = parse_generation_manifest(bytes, ManifestLimits::default()).unwrap();
    let encoded = serde_json::to_vec(&manifest).unwrap();
    let decoded = parse_generation_manifest(&encoded, ManifestLimits::default()).unwrap();
    assert_eq!(decoded, manifest);
}

#[test]
fn manifest_rejects_oversized_descriptor_metadata() {
    let bytes = br#"{
        "format_version":1,
        "generation_id":"00000000000000000000000000000001",
        "build_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "abi_tag":1,
        "target":"x86_64-unknown-linux-gnu",
        "library_file":"generation.so",
        "library_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "created_unix_secs":1,
        "leaves":[{
            "prekey":{"name":"demo","arity":1,"ops_len":1},
            "content_hash":"00000000000000000000000000000001",
            "variant_hash":"00000000000000000000000000000002",
            "arity":1,
            "entry_symbol":"entry",
            "descriptor_symbol":"descriptor",
            "descriptor_bytes":4194305,
            "reloc_recipe_bytes":0,
            "spec_site_count":0
        }]
    }"#;
    assert!(parse_generation_manifest(bytes, ManifestLimits::default()).is_err());
}

#[test]
fn manifest_rejects_duplicate_content_variant_pairs() {
    let bytes = br#"{
        "format_version":1,
        "generation_id":"00000000000000000000000000000001",
        "build_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "abi_tag":1,
        "target":"x86_64-unknown-linux-gnu",
        "library_file":"generation.so",
        "library_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "created_unix_secs":1,
        "leaves":[
            {"prekey":{"name":"a","arity":1,"ops_len":1},"content_hash":"00000000000000000000000000000001","variant_hash":"00000000000000000000000000000002","arity":1,"entry_symbol":"entry1","descriptor_symbol":"descriptor1","descriptor_bytes":0,"reloc_recipe_bytes":0,"spec_site_count":0},
            {"prekey":{"name":"b","arity":1,"ops_len":1},"content_hash":"00000000000000000000000000000001","variant_hash":"00000000000000000000000000000002","arity":1,"entry_symbol":"entry2","descriptor_symbol":"descriptor2","descriptor_bytes":0,"reloc_recipe_bytes":0,"spec_site_count":0}
        ]
    }"#;
    assert!(parse_generation_manifest(bytes, ManifestLimits::default()).is_err());
}

#[test]
fn manifest_rejects_non_basename_library_file() {
    let bytes = br#"{
        "format_version":1,
        "generation_id":"00000000000000000000000000000001",
        "build_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "abi_tag":1,
        "target":"x86_64-unknown-linux-gnu",
        "library_file":"nested/generation.so",
        "library_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "created_unix_secs":1,
        "leaves":[]
    }"#;
    assert!(parse_generation_manifest(bytes, ManifestLimits::default()).is_err());
}

#[test]
fn manifest_identity_rejects_wrong_build_target_and_abi() {
    let bytes = br#"{
        "format_version":1,
        "generation_id":"00000000000000000000000000000001",
        "build_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "abi_tag":1,
        "target":"x86_64-unknown-linux-gnu",
        "library_file":"generation.so",
        "library_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "created_unix_secs":1,
        "leaves":[]
    }"#;
    let manifest =
        parse_generation_manifest(bytes, ManifestLimits::default()).expect("valid manifest");
    assert!(
        validate_manifest_identity(
            &manifest,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "x86_64-unknown-linux-gnu",
            1
        )
        .is_err()
    );
    assert!(
        validate_manifest_identity(
            &manifest,
            &manifest.build_id,
            "aarch64-unknown-linux-gnu",
            1
        )
        .is_err()
    );
    assert!(
        validate_manifest_identity(&manifest, &manifest.build_id, &manifest.target, 2).is_err()
    );
}
