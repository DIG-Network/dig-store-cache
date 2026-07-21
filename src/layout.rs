//! The on-disk layout + the filesystem primitives the cache is built on.
//!
//! Layout under the caller-chosen `root`:
//! ```text
//! <root>/capsules/<retrieval_key_hex>.dig   the admitted capsules (disk = source of truth)
//! <root>/tmp/<uuid>.part                    in-flight admissions (staged, fsync'd, then renamed)
//! <root>/index.json                         the advisory manifest (recency + pinned overlay)
//! ```
//!
//! Admission is crash-safe: bytes are written to a unique `tmp/*.part`, fsync'd, then atomically
//! RENAMED into `capsules/`. A crash therefore never leaves a half-written file in `capsules/` — only
//! a stray `.part`, which [`clean_tmp`] reclaims on the next [`Cache::open`](crate::Cache::open).

use crate::error::CacheError;
use dig_store::{get_capsule_identity, retrieval_key, CapsuleIdentity};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// The subdirectory holding admitted capsules.
pub const CAPSULES_DIR: &str = "capsules";
/// The subdirectory holding in-flight `.part` admissions.
pub const TMP_DIR: &str = "tmp";
/// The advisory manifest filename at the cache root.
pub const INDEX_FILE: &str = "index.json";
/// The extension every cached capsule file carries.
pub const CAPSULE_EXT: &str = "dig";
/// The extension of an in-flight staged admission.
pub const PART_EXT: &str = "part";

/// The `capsules/` directory under `root`.
pub fn capsules_dir(root: &Path) -> PathBuf {
    root.join(CAPSULES_DIR)
}

/// The `tmp/` directory under `root`.
pub fn tmp_dir(root: &Path) -> PathBuf {
    root.join(TMP_DIR)
}

/// The `index.json` path under `root`.
pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(INDEX_FILE)
}

/// The on-disk path of the capsule with the given retrieval-key hex stem.
pub fn capsule_path(root: &Path, key_hex: &str) -> PathBuf {
    capsules_dir(root).join(format!("{key_hex}.{CAPSULE_EXT}"))
}

/// The retrieval-key hex the capsule with identity `id` is filed under.
///
/// This is the single ecosystem-canonical derivation (`urn:dig:chia:<store_id>:<root_hash>` →
/// `retrieval_key`, from `dig-store`), hex-encoded — so a capsule is addressed byte-identically here
/// and everywhere else in the network.
pub fn retrieval_key_hex(id: &CapsuleIdentity) -> String {
    // `retrieval_key` derives a hash from a well-formed URN; a valid identity always yields a URN, so
    // this cannot fail for any `CapsuleIdentity` the type system lets us hold.
    let key =
        retrieval_key(&id.capsule_urn()).expect("a CapsuleIdentity always yields a valid URN");
    hex::encode(key.as_slice())
}

/// Create `root`, `root/capsules`, and `root/tmp`, failing with [`CacheError::RootNotWritable`] if the
/// root cannot be established.
pub fn ensure_dirs(root: &Path) -> Result<(), CacheError> {
    fs::create_dir_all(capsules_dir(root))
        .and_then(|()| fs::create_dir_all(tmp_dir(root)))
        .map_err(|source| CacheError::RootNotWritable {
            root: root.to_path_buf(),
            source,
        })
}

/// Stage `bytes` into a fresh `tmp/<uuid>.part`, fsync it, and return the staged path.
///
/// Admission is deliberately two-phase (stage, then [`finalize`]) so the caller can evict to make room
/// AFTER the new bytes are safely on disk — a write failure then never costs the evicted entries.
pub fn stage_bytes(root: &Path, bytes: &[u8]) -> Result<PathBuf, CacheError> {
    let staged = stage_path(root);
    let mut file = File::create(&staged).map_err(|e| CacheError::io(&staged, e))?;
    file.write_all(bytes)
        .map_err(|e| CacheError::io(&staged, e))?;
    file.sync_all().map_err(|e| CacheError::io(&staged, e))?;
    Ok(staged)
}

/// Stream-copy the file at `src` into a fresh `tmp/<uuid>.part`, fsync it, and return the staged path
/// plus the copied byte count.
///
/// Path-based (`std::io::copy` streams) so a multi-hundred-megabyte capsule is never slurped into RAM.
pub fn stage_file(root: &Path, src: &Path) -> Result<(PathBuf, u64), CacheError> {
    let staged = stage_path(root);
    let mut reader = File::open(src).map_err(|e| CacheError::io(src, e))?;
    // Stream through a WRITABLE destination handle: `io::copy` never buffers the whole file, and we
    // must `sync_all` on a write-capable handle (a read-only handle's flush is denied on Windows).
    let mut writer = File::create(&staged).map_err(|e| CacheError::io(&staged, e))?;
    let size = std::io::copy(&mut reader, &mut writer).map_err(|e| CacheError::io(&staged, e))?;
    writer.sync_all().map_err(|e| CacheError::io(&staged, e))?;
    Ok((staged, size))
}

/// Atomically move a staged file into its final `capsules/<key_hex>.dig` slot.
pub fn finalize(root: &Path, key_hex: &str, staged: &Path) -> Result<(), CacheError> {
    let dest = capsule_path(root, key_hex);
    fs::rename(staged, &dest).map_err(|e| CacheError::io(&dest, e))
}

/// Best-effort removal of a staged `.part` file (cleanup after a failed admission).
pub fn discard_staged(staged: &Path) {
    let _ = fs::remove_file(staged);
}

/// A fresh, collision-free staging path in `tmp/`.
fn stage_path(root: &Path) -> PathBuf {
    tmp_dir(root).join(format!("{}.{PART_EXT}", uuid::Uuid::new_v4()))
}

/// Delete the capsule filed under `key_hex`. Missing file is not an error (already gone).
pub fn remove_capsule(root: &Path, key_hex: &str) -> Result<(), CacheError> {
    let path = capsule_path(root, key_hex);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CacheError::io(&path, e)),
    }
}

/// Delete every leftover `*.part` in `tmp/` (crash residue from an interrupted admission). Best-effort
/// per file; a failure to remove one stray does not abort recovery.
pub fn clean_tmp(root: &Path) -> Result<(), CacheError> {
    let dir = tmp_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CacheError::io(&dir, e)),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some(PART_EXT) {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

/// A capsule file discovered on disk during a scan.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    /// The retrieval-key hex stem (the filename without `.dig`).
    pub key_hex: String,
    /// The file's absolute path.
    pub path: PathBuf,
    /// Its size in bytes.
    pub size: u64,
    /// Last-modified time as nanoseconds since the Unix epoch (0 if unavailable) — the recency source
    /// when the manifest is missing (disk is authoritative).
    pub mtime_nanos: u128,
}

/// Scan `capsules/` for every `*.dig` file. Disk is the source of truth for what the cache holds.
pub fn scan_capsules(root: &Path) -> Result<Vec<ScannedFile>, CacheError> {
    let dir = capsules_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(CacheError::io(&dir, e)),
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some(CAPSULE_EXT) {
            continue;
        }
        let Some(key_hex) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let meta = entry.metadata().map_err(|e| CacheError::io(&path, e))?;
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        found.push(ScannedFile {
            key_hex,
            path,
            size: meta.len(),
            mtime_nanos,
        });
    }
    Ok(found)
}

/// Recover the identity of an orphan capsule file (present on disk, absent from the manifest) by
/// MEMORY-MAPPING it and reading its `.dig` header via `dig_store::get_capsule_identity`.
///
/// The mapping is zero-copy — the OS pages the file in on demand — so recovering the identity of a
/// ~1 GiB capsule stays RAM-flat (the crate never slurps a capsule body onto the heap). This path runs
/// ONLY for orphan files during [`Cache::open`](crate::Cache::open); the manifest fast path never maps a capsule.
pub fn recover_identity(path: &Path) -> Result<CapsuleIdentity, CacheError> {
    let file = File::open(path).map_err(|e| CacheError::io(path, e))?;
    // SAFETY: the file is opened read-only and the mapping is dropped at the end of this function; the
    // cache is the sole writer of `capsules/` and never mutates an admitted file in place.
    let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| CacheError::io(path, e))?;
    get_capsule_identity(&mmap).map_err(|e| CacheError::CorruptEntry {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}
