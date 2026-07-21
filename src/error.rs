//! The cache's error taxonomy.
//!
//! Every fallible cache operation returns [`CacheError`]. The variants are stable, catalogued, and
//! carry the identity/path context a caller (or an operator reading a log) needs to act — the DIG
//! agent-friendly contract (§6.2): no stringly-typed failures, no scraping prose to learn what broke.

use dig_store::CapsuleIdentity;
use std::path::PathBuf;

/// A failure of a cache operation.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// A filesystem operation failed. Carries the path it was acting on so the fault is locatable.
    #[error("i/o error at {path}: {source}")]
    Io {
        /// The path the failing operation touched.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A single capsule is larger than the whole cache capacity. Admitting it would require evicting
    /// everything and STILL not fit, so it is rejected outright — nothing is evicted for a doomed put.
    #[error("capsule {id:?} is {size} bytes, larger than the cache capacity of {capacity} bytes")]
    EntryTooLarge {
        /// The capsule that could never fit.
        // Boxed to keep `CacheError` (and every `Result<_, CacheError>`) small: a bare
        // `CapsuleIdentity` is 64 bytes, which would otherwise bloat the common `Result` on the hot
        // admit path (clippy::result_large_err).
        id: Box<CapsuleIdentity>,
        /// Its size in bytes.
        size: u64,
        /// The cache's total capacity in bytes.
        capacity: u64,
    },

    /// `PutOptions::check_identity` was set and the bytes' recovered `(store_id, root_hash)` did not
    /// equal the claimed identity. The bytes are NOT admitted.
    #[error("capsule bytes declare {recovered:?} but the caller claimed {claimed:?}")]
    IdentityMismatch {
        /// The identity the caller passed to `put` (boxed — see [`CacheError::EntryTooLarge`]).
        claimed: Box<CapsuleIdentity>,
        /// The identity recovered from the bytes.
        recovered: Box<CapsuleIdentity>,
    },

    /// A cached `.dig` file could not be read back as a capsule (during `open` orphan recovery, or a
    /// `check_identity` read of unparseable bytes). Disk stays authoritative — the entry is skipped.
    #[error("cached capsule at {path} is corrupt: {reason}")]
    CorruptEntry {
        /// The offending file.
        path: PathBuf,
        /// Why it could not be read as a capsule.
        reason: String,
    },

    /// The cache root cannot be created or written to (bad path, permissions, read-only volume).
    #[error("cache root {root} is not writable: {source}")]
    RootNotWritable {
        /// The root directory the caller passed to [`Cache::open`](crate::Cache::open).
        root: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

impl CacheError {
    /// Build an [`CacheError::Io`] bound to the path the operation touched.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
