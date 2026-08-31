use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{MAX_DESCRIPTOR_BYTES, MAX_MANIFEST_BYTES, MAX_MANIFEST_LEAVES};

pub(crate) const FORMAT_VERSION: u32 = 1;

#[cfg(test)]
#[path = "format_test.rs"]
mod tests;

macro_rules! hex_key {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(pub u128);

        impl $name {
            pub const fn from_u128(value: u128) -> Self {
                Self(value)
            }

            pub fn parse_hex(value: &str) -> Result<Self, String> {
                if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(format!(
                        "{} must be exactly 32 hexadecimal characters",
                        stringify!($name)
                    ));
                }
                u128::from_str_radix(value, 16)
                    .map(Self)
                    .map_err(|_| format!("invalid {} hexadecimal value", stringify!($name)))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:032x}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse_hex(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse_hex(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}

hex_key!(ContentHash);
hex_key!(VariantHash);
hex_key!(GenerationId);

impl GenerationId {
    /// Parse the canonical directory spelling used by immutable generations.
    pub(crate) fn parse_directory_name(value: &str) -> Option<Self> {
        (value.len() == 32
            && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            && value == value.to_ascii_lowercase())
        .then(|| Self::parse_hex(value).ok())
        .flatten()
    }
}

/// The inexpensive identity available when a named bytecode function is
/// published. It deliberately contains no bytecode or heap pointers.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionPrekey {
    pub name: String,
    pub arity: usize,
    pub ops_len: usize,
}

impl FunctionPrekey {
    pub fn new(name: impl Into<String>, arity: usize, ops_len: usize) -> Self {
        Self {
            name: name.into(),
            arity,
            ops_len,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationManifest {
    pub format_version: u32,
    pub generation_id: GenerationId,
    pub build_id: String,
    pub abi_tag: u32,
    pub target: String,
    pub library_file: String,
    pub library_sha256: String,
    pub created_unix_secs: u64,
    pub leaves: Vec<ManifestLeaf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestLeaf {
    pub prekey: FunctionPrekey,
    pub content_hash: ContentHash,
    pub variant_hash: VariantHash,
    pub arity: usize,
    pub entry_symbol: String,
    pub descriptor_symbol: String,
    pub descriptor_bytes: u32,
    pub reloc_recipe_bytes: u32,
    pub spec_site_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManifestLimits {
    pub max_leaves: usize,
}

impl Default for ManifestLimits {
    fn default() -> Self {
        Self {
            max_leaves: MAX_MANIFEST_LEAVES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestError {
    TooLarge { actual: usize, maximum: usize },
    InvalidJson(String),
    UnsupportedFormat(u32),
    TooManyLeaves { actual: usize, maximum: usize },
    DuplicateLeaf(ContentHash, VariantHash),
    InvalidLibraryFile(String),
    InvalidHex { field: &'static str },
    DescriptorTooLarge(u32),
    RelocationRecipeTooLarge(u32),
    SpeculationSitesTooMany(u32),
    IdentityMismatch(&'static str),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { actual, maximum } => {
                write!(f, "manifest is {actual} bytes, maximum is {maximum}")
            }
            Self::InvalidJson(error) => write!(f, "invalid generation manifest JSON: {error}"),
            Self::UnsupportedFormat(version) => {
                write!(f, "unsupported generation manifest format {version}")
            }
            Self::TooManyLeaves { actual, maximum } => {
                write!(f, "manifest has {actual} leaves, maximum is {maximum}")
            }
            Self::DuplicateLeaf(content, variant) => {
                write!(f, "duplicate content/variant pair {content}/{variant}")
            }
            Self::InvalidLibraryFile(file) => {
                write!(f, "library file is not a basename: {file:?}")
            }
            Self::InvalidHex { field } => write!(f, "{field} is not a 64-character hex string"),
            Self::DescriptorTooLarge(bytes) => {
                write!(
                    f,
                    "descriptor is {bytes} bytes, maximum is {MAX_DESCRIPTOR_BYTES}"
                )
            }
            Self::RelocationRecipeTooLarge(bytes) => write!(
                f,
                "relocation recipe is {bytes} bytes, maximum is {}",
                super::MAX_RELOC_RECIPE_BYTES
            ),
            Self::SpeculationSitesTooMany(sites) => write!(
                f,
                "speculation site count is {sites}, maximum is {}",
                super::MAX_SPEC_SITES
            ),
            Self::IdentityMismatch(field) => write!(f, "manifest {field} does not match runtime"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Decode and validate the untrusted JSON envelope before any library is
/// opened. The fixed byte and leaf limits are applied independently.
pub fn parse_generation_manifest(
    bytes: &[u8],
    limits: ManifestLimits,
) -> Result<GenerationManifest, ManifestError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::TooLarge {
            actual: bytes.len(),
            maximum: MAX_MANIFEST_BYTES,
        });
    }

    let manifest: GenerationManifest = serde_json::from_slice(bytes)
        .map_err(|error| ManifestError::InvalidJson(error.to_string()))?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(ManifestError::UnsupportedFormat(manifest.format_version));
    }
    let maximum = limits.max_leaves.min(MAX_MANIFEST_LEAVES);
    if manifest.leaves.len() > maximum {
        return Err(ManifestError::TooManyLeaves {
            actual: manifest.leaves.len(),
            maximum,
        });
    }
    if !is_basename(&manifest.library_file) {
        return Err(ManifestError::InvalidLibraryFile(manifest.library_file));
    }
    if !is_hex(&manifest.build_id, 64) {
        return Err(ManifestError::InvalidHex { field: "build_id" });
    }
    if !is_hex(&manifest.library_sha256, 64) {
        return Err(ManifestError::InvalidHex {
            field: "library_sha256",
        });
    }

    let mut pairs = std::collections::HashSet::with_capacity(manifest.leaves.len());
    for leaf in &manifest.leaves {
        if leaf.descriptor_bytes > MAX_DESCRIPTOR_BYTES {
            return Err(ManifestError::DescriptorTooLarge(leaf.descriptor_bytes));
        }
        if leaf.reloc_recipe_bytes > super::MAX_RELOC_RECIPE_BYTES {
            return Err(ManifestError::RelocationRecipeTooLarge(
                leaf.reloc_recipe_bytes,
            ));
        }
        if leaf.spec_site_count > super::MAX_SPEC_SITES {
            return Err(ManifestError::SpeculationSitesTooMany(leaf.spec_site_count));
        }
        if leaf.arity != leaf.prekey.arity {
            return Err(ManifestError::InvalidJson(
                "leaf arity does not match its prekey".into(),
            ));
        }
        if !pairs.insert((leaf.content_hash, leaf.variant_hash)) {
            return Err(ManifestError::DuplicateLeaf(
                leaf.content_hash,
                leaf.variant_hash,
            ));
        }
    }
    Ok(manifest)
}

pub(crate) fn validate_manifest_identity(
    manifest: &GenerationManifest,
    expected_build_id: &str,
    expected_target: &str,
    expected_abi_tag: u32,
) -> Result<(), ManifestError> {
    if manifest.build_id != expected_build_id {
        return Err(ManifestError::IdentityMismatch("build_id"));
    }
    if manifest.target != expected_target {
        return Err(ManifestError::IdentityMismatch("target"));
    }
    if manifest.abi_tag != expected_abi_tag {
        return Err(ManifestError::IdentityMismatch("abi_tag"));
    }
    Ok(())
}

fn is_basename(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', ':'])
        && !value.chars().any(char::is_control)
        && Path::new(value)
            .file_name()
            .is_some_and(|name| name == value)
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GenerationIndex {
    pub generations: Vec<IndexedGeneration>,
}

#[allow(dead_code)] // generation metadata is consumed by later loader tasks.
#[derive(Clone, Debug)]
pub(crate) struct IndexedGeneration {
    pub generation_id: GenerationId,
    pub created_unix_secs: u64,
    pub leaves: Vec<IndexedLeaf>,
}

#[allow(dead_code)] // leaf metadata is consumed by later loader/emitter tasks.
#[derive(Clone, Debug)]
pub(crate) struct IndexedLeaf {
    pub generation_id: GenerationId,
    pub created_unix_secs: u64,
    pub prekey: FunctionPrekey,
    pub content_hash: ContentHash,
    pub variant_hash: VariantHash,
    pub arity: usize,
    pub entry_symbol: String,
    pub descriptor_symbol: String,
    pub descriptor_bytes: u32,
    pub reloc_recipe_bytes: u32,
    pub spec_site_count: u32,
}

impl GenerationIndex {
    #[allow(dead_code)] // used by the storage/indexing task.
    pub(crate) fn from_manifests(manifests: impl IntoIterator<Item = GenerationManifest>) -> Self {
        let generations = manifests
            .into_iter()
            .map(|manifest| {
                let generation_id = manifest.generation_id;
                let created_unix_secs = manifest.created_unix_secs;
                let leaves = manifest
                    .leaves
                    .into_iter()
                    .map(|leaf| IndexedLeaf {
                        generation_id,
                        created_unix_secs,
                        prekey: leaf.prekey,
                        content_hash: leaf.content_hash,
                        variant_hash: leaf.variant_hash,
                        arity: leaf.arity,
                        entry_symbol: leaf.entry_symbol,
                        descriptor_symbol: leaf.descriptor_symbol,
                        descriptor_bytes: leaf.descriptor_bytes,
                        reloc_recipe_bytes: leaf.reloc_recipe_bytes,
                        spec_site_count: leaf.spec_site_count,
                    })
                    .collect();
                IndexedGeneration {
                    generation_id,
                    created_unix_secs,
                    leaves,
                }
            })
            .collect();
        Self { generations }
    }
}
