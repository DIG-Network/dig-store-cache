//! The [`Cache`] handle + its internal state machine.
//!
//! [`Cache`] is a cheap-to-clone, thread-safe handle (`Arc<Mutex<Inner>>`): every method takes `&self`
//! and briefly locks the shared state. The state machine enforces the crate's invariants — capacity,
//! pin exemption, atomic admission, disk-authoritative rebuild — with the filesystem work delegated to
//! [`crate::layout`] and the eviction decision to the pluggable [`crate::policy::EvictionPolicy`].

use crate::config::{Admission, CacheConfig, CacheStats, PutOptions};
use crate::error::CacheError;
use crate::index::{self, Manifest, ManifestEntry};
use crate::layout;
use crate::policy::{EvictionContext, EvictionEntry};
use dig_store::{get_capsule_identity, CapsuleIdentity};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A thread-safe handle to an on-disk capsule cache. Cloning shares the same underlying cache.
#[derive(Clone)]
pub struct Cache {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("Cache")
            .field("root", &inner.root)
            .field("count", &inner.entries.len())
            .field("bytes_used", &inner.bytes_used)
            .field("capacity", &inner.config.max_bytes)
            .finish()
    }
}

/// A cached capsule located on disk. Byte access is PATH-BASED — capsules can be ~1 GiB, so a consumer
/// streams from [`CachedCapsule::path`] rather than reading the whole file into memory.
#[derive(Debug, Clone)]
pub struct CachedCapsule {
    id: CapsuleIdentity,
    path: PathBuf,
}

impl CachedCapsule {
    /// The capsule's identity.
    pub fn id(&self) -> CapsuleIdentity {
        self.id
    }

    /// The path to the capsule's `.dig` file on disk. Stream from here; do not assume it fits in RAM.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One capsule's in-memory bookkeeping. The identity is the map key; recency + pin live here.
struct Entry {
    key_hex: String,
    size: u64,
    seq: u64,
    pinned: bool,
}

/// The locked cache state.
struct Inner {
    root: PathBuf,
    config: CacheConfig,
    entries: HashMap<CapsuleIdentity, Entry>,
    bytes_used: u64,
    next_seq: u64,
}

impl Cache {
    /// Open (or rebuild) a cache rooted at `root` with `config`.
    ///
    /// Establishes `root/{capsules,tmp}`, reclaims any leftover `tmp/*.part` from an interrupted
    /// admission, then rebuilds the in-memory index by SCANNING `capsules/` (disk is authoritative)
    /// and overlaying `index.json` for recency + pin state. A capsule file with no manifest entry is
    /// admitted with its identity recovered from the file and mtime-based recency; a manifest entry
    /// with no file is dropped.
    pub fn open(root: &Path, config: CacheConfig) -> Result<Cache, CacheError> {
        layout::ensure_dirs(root)?;
        layout::clean_tmp(root)?;
        let scanned = layout::scan_capsules(root)?;
        let manifest = index::load(root);

        let inner = Inner::rebuild(root.to_path_buf(), config, scanned, manifest)?;
        // Persist the reconciled view (dropped orphans, admitted disk-only files, fresh recency).
        inner.save_manifest()?;
        Ok(Cache {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Admit the file at `src` under identity `id`. The file is stream-copied (never slurped), so this
    /// is the path for real (large) capsules. `check_identity` is IGNORED here (the body is not read).
    pub fn put_file(
        &self,
        id: CapsuleIdentity,
        src: &Path,
        opts: PutOptions,
    ) -> Result<Admission, CacheError> {
        let size = std::fs::metadata(src)
            .map_err(|e| CacheError::io(src, e))?
            .len();
        let mut inner = self.lock();
        inner.reject_if_too_large(id, size)?;
        let (staged, _) = layout::stage_file(&inner.root, src)?;
        inner.admit_staged(id, staged, size, opts.pinned)
    }

    /// Admit `bytes` under identity `id`. A convenience for small/test capsules — real capsules use
    /// [`Cache::put_file`]. With `opts.check_identity`, the bytes' declared identity must equal `id`.
    pub fn put_bytes(
        &self,
        id: CapsuleIdentity,
        bytes: &[u8],
        opts: PutOptions,
    ) -> Result<Admission, CacheError> {
        if opts.check_identity {
            verify_claimed_identity(id, bytes)?;
        }
        let size = bytes.len() as u64;
        let mut inner = self.lock();
        inner.reject_if_too_large(id, size)?;
        let staged = layout::stage_bytes(&inner.root, bytes)?;
        inner.admit_staged(id, staged, size, opts.pinned)
    }

    /// Fetch a cached capsule, marking it most-recently-used. `None` if not cached.
    pub fn get(&self, id: &CapsuleIdentity) -> Option<CachedCapsule> {
        let mut inner = self.lock();
        let seq = inner.next_seq;
        let key_hex = {
            let entry = inner.entries.get_mut(id)?;
            entry.seq = seq; // recency bump (in memory only; not persisted until a structural change)
            entry.key_hex.clone()
        };
        inner.next_seq += 1;
        let path = layout::capsule_path(&inner.root, &key_hex);
        Some(CachedCapsule { id: *id, path })
    }

    /// Read a cached capsule's bytes into memory (convenience for small/test capsules; real capsules
    /// stream from [`CachedCapsule::path`]). Updates recency. `Ok(None)` if not cached.
    pub fn get_bytes(&self, id: &CapsuleIdentity) -> Result<Option<Vec<u8>>, CacheError> {
        let Some(cached) = self.get(id) else {
            return Ok(None);
        };
        let bytes = std::fs::read(cached.path()).map_err(|e| CacheError::io(cached.path(), e))?;
        Ok(Some(bytes))
    }

    /// Whether `id` is currently cached.
    pub fn contains(&self, id: &CapsuleIdentity) -> bool {
        self.lock().entries.contains_key(id)
    }

    /// Every capsule the cache currently holds — the node's advertisable holdings.
    pub fn holdings(&self) -> Vec<CapsuleIdentity> {
        self.lock().entries.keys().copied().collect()
    }

    /// Remove a capsule (deleting its file + freeing its bytes). Returns whether it was present.
    pub fn remove(&self, id: &CapsuleIdentity) -> Result<bool, CacheError> {
        let mut inner = self.lock();
        if !inner.entries.contains_key(id) {
            return Ok(false);
        }
        inner.drop_entry(id)?;
        inner.save_manifest()?;
        Ok(true)
    }

    /// Pin a capsule so eviction never reclaims it. Returns whether it was present (and is now pinned).
    pub fn pin(&self, id: &CapsuleIdentity) -> Result<bool, CacheError> {
        self.set_pinned(id, true)
    }

    /// Unpin a capsule, making it eligible for eviction again. Returns whether it was present.
    pub fn unpin(&self, id: &CapsuleIdentity) -> Result<bool, CacheError> {
        self.set_pinned(id, false)
    }

    /// A snapshot of occupancy.
    pub fn stats(&self) -> CacheStats {
        let inner = self.lock();
        CacheStats {
            bytes_used: inner.bytes_used,
            count: inner.entries.len(),
            capacity: inner.config.max_bytes,
        }
    }

    /// Replace the configuration. Lowering `max_bytes` evicts (per the policy) until the cache fits the
    /// new capacity and returns the evicted set (the flywheel-retract signal).
    pub fn set_config(&self, config: CacheConfig) -> Result<Admission, CacheError> {
        let mut inner = self.lock();
        inner.config = config;
        let evicted = inner.evict_to_fit();
        inner.save_manifest()?;
        Ok(Admission { evicted })
    }

    fn set_pinned(&self, id: &CapsuleIdentity, pinned: bool) -> Result<bool, CacheError> {
        let mut inner = self.lock();
        match inner.entries.get_mut(id) {
            Some(entry) => {
                entry.pinned = pinned;
                inner.save_manifest()?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means a prior holder panicked mid-mutation; recover the guard rather than
        // cascading the panic — the next structural write re-persists a consistent manifest.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    /// Rebuild the in-memory index from a disk scan overlaid with the manifest (see [`Cache::open`]).
    fn rebuild(
        root: PathBuf,
        config: CacheConfig,
        scanned: Vec<layout::ScannedFile>,
        manifest: Option<Manifest>,
    ) -> Result<Inner, CacheError> {
        let by_key: HashMap<String, ManifestEntry> = manifest
            .map(|m| {
                m.entries
                    .into_iter()
                    .map(|e| (e.retrieval_key.clone(), e))
                    .collect()
            })
            .unwrap_or_default();

        // Each surviving file becomes a pending entry with a recency sort key: the manifest's seq when
        // known, else the file mtime (disk-authoritative recency after a manifest loss).
        struct Pending {
            id: CapsuleIdentity,
            key_hex: String,
            size: u64,
            pinned: bool,
            sort_key: u128,
        }

        let mut pending = Vec::new();
        for file in scanned {
            let resolved = by_key.get(&file.key_hex).and_then(|entry| {
                entry
                    .identity()
                    .map(|id| (id, entry.seq as u128, entry.pinned))
            });

            let (id, sort_key, pinned) = match resolved {
                Some((id, seq, pinned)) => (id, seq, pinned),
                None => {
                    // Orphan file: recover its identity from disk. A file that cannot be read as a
                    // capsule, or is filed under the wrong key, is foreign/corrupt — leave it on disk
                    // and do not treat it as a holding.
                    match layout::recover_identity(&file.path) {
                        Ok(id) if layout::retrieval_key_hex(&id) == file.key_hex => {
                            (id, file.mtime_nanos, false)
                        }
                        _ => continue,
                    }
                }
            };
            pending.push(Pending {
                id,
                key_hex: file.key_hex,
                size: file.size,
                pinned,
                sort_key,
            });
        }

        // Normalize recency into a dense 0..n sequence in time order (mixing manifest-seq and mtime
        // sources cleanly, since only relative order matters).
        pending.sort_by_key(|p| p.sort_key);
        let mut entries = HashMap::new();
        let mut bytes_used = 0u64;
        for (seq, p) in pending.into_iter().enumerate() {
            bytes_used = bytes_used.saturating_add(p.size);
            entries.insert(
                p.id,
                Entry {
                    key_hex: p.key_hex,
                    size: p.size,
                    seq: seq as u64,
                    pinned: p.pinned,
                },
            );
        }
        let next_seq = entries.len() as u64;

        Ok(Inner {
            root,
            config,
            entries,
            bytes_used,
            next_seq,
        })
    }

    /// Reject an admission whose single capsule can never fit (larger than the whole capacity), before
    /// any eviction or staging happens.
    fn reject_if_too_large(&self, id: CapsuleIdentity, size: u64) -> Result<(), CacheError> {
        if size > self.config.max_bytes {
            return Err(CacheError::EntryTooLarge {
                id: Box::new(id),
                size,
                capacity: self.config.max_bytes,
            });
        }
        Ok(())
    }

    /// Finish an admission whose bytes are already staged in `tmp/`: evict to make room, atomically
    /// move the staged file into place, and update the index + manifest.
    fn admit_staged(
        &mut self,
        id: CapsuleIdentity,
        staged: PathBuf,
        size: u64,
        pinned: bool,
    ) -> Result<Admission, CacheError> {
        let key_hex = layout::retrieval_key_hex(&id);

        let evicted = self.select_evictions(&id, size);
        if let Err(e) = self.apply_evictions(&evicted) {
            layout::discard_staged(&staged);
            return Err(e);
        }

        if let Err(e) = layout::finalize(&self.root, &key_hex, &staged) {
            layout::discard_staged(&staged);
            return Err(e);
        }

        if let Some(previous) = self.entries.get(&id) {
            self.bytes_used = self.bytes_used.saturating_sub(previous.size);
        }
        self.bytes_used = self.bytes_used.saturating_add(size);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.insert(
            id,
            Entry {
                key_hex,
                size,
                seq,
                pinned,
            },
        );

        self.save_manifest()?;
        Ok(Admission { evicted })
    }

    /// Ask the policy which capsules to evict to fit `incoming` bytes, excluding the incoming id
    /// itself (a re-admission of an already-cached capsule reuses its slot rather than evicting it).
    fn select_evictions(
        &self,
        incoming_id: &CapsuleIdentity,
        incoming: u64,
    ) -> Vec<CapsuleIdentity> {
        let existing = self.entries.get(incoming_id).map(|e| e.size).unwrap_or(0);
        let current_bytes = self.bytes_used.saturating_sub(existing);
        let entries: Vec<EvictionEntry> = self
            .entries
            .iter()
            .filter(|(id, _)| *id != incoming_id)
            .map(|(id, e)| EvictionEntry {
                id: *id,
                size: e.size,
                last_access: e.seq,
                pinned: e.pinned,
            })
            .collect();
        let ctx = EvictionContext {
            entries: &entries,
            current_bytes,
            capacity: self.config.max_bytes,
            incoming_size: incoming,
        };
        self.config.policy.select_evictions(&ctx)
    }

    /// Evict everything the policy selects to bring the cache within its (possibly newly-lowered)
    /// capacity. Returns the evicted ids; a filesystem error during a delete is swallowed so a stuck
    /// file never wedges reconfiguration (the entry is still dropped from the index).
    fn evict_to_fit(&mut self) -> Vec<CapsuleIdentity> {
        let entries: Vec<EvictionEntry> = self
            .entries
            .iter()
            .map(|(id, e)| EvictionEntry {
                id: *id,
                size: e.size,
                last_access: e.seq,
                pinned: e.pinned,
            })
            .collect();
        let ctx = EvictionContext {
            entries: &entries,
            current_bytes: self.bytes_used,
            capacity: self.config.max_bytes,
            incoming_size: 0,
        };
        let evicted = self.config.policy.select_evictions(&ctx);
        for id in &evicted {
            let _ = self.drop_entry(id);
        }
        evicted
    }

    /// Delete each selected capsule's file and drop it from the index.
    fn apply_evictions(&mut self, evicted: &[CapsuleIdentity]) -> Result<(), CacheError> {
        for id in evicted {
            self.drop_entry(id)?;
        }
        Ok(())
    }

    /// Remove a single entry: delete its file, subtract its bytes, drop it from the index. A no-op if
    /// the id is not present.
    fn drop_entry(&mut self, id: &CapsuleIdentity) -> Result<(), CacheError> {
        if let Some(entry) = self.entries.remove(id) {
            layout::remove_capsule(&self.root, &entry.key_hex)?;
            self.bytes_used = self.bytes_used.saturating_sub(entry.size);
        }
        Ok(())
    }

    /// Serialize the current index to `index.json`.
    fn save_manifest(&self) -> Result<(), CacheError> {
        let entries = self
            .entries
            .iter()
            .map(|(id, e)| ManifestEntry {
                store_id: index::bytes32_hex(&id.store_id),
                root_hash: index::bytes32_hex(&id.root_hash),
                retrieval_key: e.key_hex.clone(),
                size: e.size,
                seq: e.seq,
                pinned: e.pinned,
            })
            .collect();
        let manifest = Manifest {
            version: index::MANIFEST_VERSION,
            next_seq: self.next_seq,
            entries,
        };
        index::save(&self.root, &manifest)
    }
}

/// Recover the declared identity from `bytes` and assert it equals the caller's claim.
fn verify_claimed_identity(claimed: CapsuleIdentity, bytes: &[u8]) -> Result<(), CacheError> {
    let recovered = get_capsule_identity(bytes).map_err(|e| CacheError::CorruptEntry {
        path: PathBuf::from("<in-memory bytes>"),
        reason: e.to_string(),
    })?;
    if recovered != claimed {
        return Err(CacheError::IdentityMismatch {
            claimed: Box::new(claimed),
            recovered: Box::new(recovered),
        });
    }
    Ok(())
}
