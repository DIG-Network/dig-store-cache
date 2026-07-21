//! The advisory manifest (`index.json`).
//!
//! The manifest is an ADVISORY overlay, never the authority: disk decides what the cache HOLDS (the
//! files in `capsules/`), while the manifest supplies the two bits that cannot be recovered from a
//! filename cheaply — the recency ordering and the pinned flag — plus the `(store_id, root_hash)`
//! identity for fast rebuild. A crash that loses the manifest costs only recency ordering (recovered
//! from file mtimes) and pin state; it never loses a capsule.
//!
//! It is (re)written on every STRUCTURAL change (admit / evict / remove / pin / unpin) and on
//! `open`'s reconcile — NOT on every `get`, so a read is a pure in-memory recency bump.

use crate::error::CacheError;
use dig_store::{Bytes32, CapsuleIdentity};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// The manifest schema version. Bumped only if the on-disk shape changes incompatibly (additive
/// fields do not bump it).
pub const MANIFEST_VERSION: u32 = 1;

/// The serialized cache manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// The schema version ([`MANIFEST_VERSION`]).
    pub version: u32,
    /// The next recency stamp to hand out (advisory; rebuild recomputes a fresh sequence).
    pub next_seq: u64,
    /// One record per cached capsule.
    pub entries: Vec<ManifestEntry>,
}

/// A single capsule's manifest record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// The store's launcher id, hex.
    pub store_id: String,
    /// The capsule generation's merkle root, hex.
    pub root_hash: String,
    /// The retrieval-key hex (the capsule's filename stem) — the join key against a disk scan.
    pub retrieval_key: String,
    /// The capsule's size in bytes (advisory; the scan re-reads the authoritative size from disk).
    pub size: u64,
    /// The recency stamp — larger is more recently used.
    pub seq: u64,
    /// Whether the caller pinned this capsule.
    pub pinned: bool,
}

impl ManifestEntry {
    /// Reconstruct the capsule identity from the hex fields, or `None` if either hex is malformed.
    pub fn identity(&self) -> Option<CapsuleIdentity> {
        Some(CapsuleIdentity {
            store_id: hex_to_bytes32(&self.store_id)?,
            root_hash: hex_to_bytes32(&self.root_hash)?,
        })
    }
}

/// Load the manifest at `root/index.json`, or `None` if it is absent or unparseable (disk stays
/// authoritative — a corrupt manifest is simply ignored and rebuilt from the scan).
pub fn load(root: &Path) -> Option<Manifest> {
    let path = crate::layout::manifest_path(root);
    let raw = fs::read(&path).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Atomically write `manifest` to `root/index.json` (write to a sibling temp file, fsync, rename) so a
/// crash mid-write never leaves a truncated manifest.
pub fn save(root: &Path, manifest: &Manifest) -> Result<(), CacheError> {
    let path = crate::layout::manifest_path(root);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(manifest).expect("manifest serializes");
    {
        use std::io::Write;
        let mut file = fs::File::create(&tmp).map_err(|e| CacheError::io(&tmp, e))?;
        file.write_all(&json).map_err(|e| CacheError::io(&tmp, e))?;
        file.sync_all().map_err(|e| CacheError::io(&tmp, e))?;
    }
    fs::rename(&tmp, &path).map_err(|e| CacheError::io(&path, e))
}

/// Hex-encode a 32-byte id for the manifest.
pub fn bytes32_hex(value: &Bytes32) -> String {
    hex::encode(value.as_slice())
}

/// Decode a 32-byte hex string into a [`Bytes32`], or `None` if it is not exactly 32 bytes of hex.
fn hex_to_bytes32(s: &str) -> Option<Bytes32> {
    let bytes = hex::decode(s).ok()?;
    let array: [u8; 32] = bytes.try_into().ok()?;
    Some(Bytes32::new(array))
}
