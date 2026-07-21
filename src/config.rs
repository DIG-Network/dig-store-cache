//! Public configuration + result value types: [`CacheConfig`], [`PutOptions`], [`Admission`],
//! [`CacheStats`].

use crate::policy::{EvictionPolicy, LruPolicy};
use dig_store::CapsuleIdentity;
use std::sync::Arc;

/// The default cache capacity: 1 GiB.
pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;

/// How the cache is bounded and how it decides what to evict.
///
/// Cloning is cheap — the `policy` is shared behind an [`Arc`]. The default is a 1 GiB [`LruPolicy`]
/// cache.
#[derive(Clone)]
pub struct CacheConfig {
    /// The maximum total bytes of admitted capsules the cache holds before evicting (pins may push it
    /// over — see [`crate::Cache::pin`]).
    pub max_bytes: u64,
    /// The eviction strategy. Defaults to [`LruPolicy`].
    pub policy: Arc<dyn EvictionPolicy>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            policy: Arc::new(LruPolicy),
        }
    }
}

impl std::fmt::Debug for CacheConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The policy is a trait object with no Debug bound; report the tunable field + its presence.
        f.debug_struct("CacheConfig")
            .field("max_bytes", &self.max_bytes)
            .field("policy", &"<dyn EvictionPolicy>")
            .finish()
    }
}

/// Options controlling a single `put`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PutOptions {
    /// Pin the admitted capsule so eviction never reclaims it (until unpinned/removed).
    pub pinned: bool,
    /// Before admitting bytes, recover their declared identity and assert it equals the claimed id
    /// (a cheap structural sanity check — NOT a merkle/chain verify, which the caller already did).
    /// Ignored by [`crate::Cache::put_file`] (which never reads the source body). Defaults to `false`.
    pub check_identity: bool,
}

/// The outcome of an admission (or a capacity-lowering reconfigure): which capsules it evicted.
///
/// Feeds the flywheel's retract path — every id here is one the node must stop advertising as a
/// holding (dig-dht provider retract / opcode-222 HoldingsAnnounce retract).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Admission {
    /// The capsules evicted to make room, in eviction order. Empty when nothing was evicted.
    pub evicted: Vec<CapsuleIdentity>,
}

/// A point-in-time snapshot of cache occupancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    /// Total bytes of all admitted capsules currently on disk.
    pub bytes_used: u64,
    /// Number of capsules currently cached.
    pub count: usize,
    /// The configured capacity in bytes.
    pub capacity: u64,
}
