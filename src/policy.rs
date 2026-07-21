//! The eviction policy trait + the default LRU implementation.
//!
//! Eviction is a PLUGGABLE decision, deliberately factored behind a trait so future rules
//! (min-free-disk, TTL, access-heat, publisher-owned pinning) drop in as new [`EvictionPolicy`]
//! implementations reading new [`EvictionContext`] fields — additively, with no change to the cache's
//! public API. In v0.1.0 the only implementation is [`LruPolicy`] (least-recently-used).
//!
//! The one INVARIANT every policy must uphold: it MUST NEVER select a pinned entry. Pins are the
//! caller's explicit "keep this" override (an owned/republished capsule, content the node is a primary
//! host for); the cache relies on the policy respecting them and does not re-filter the returned set.

use dig_store::CapsuleIdentity;

/// A single cache entry as the policy sees it — enough to rank it for eviction, nothing more.
#[derive(Debug, Clone, Copy)]
pub struct EvictionEntry {
    /// The capsule this entry holds.
    pub id: CapsuleIdentity,
    /// Its on-disk size in bytes.
    pub size: u64,
    /// A monotonically increasing recency stamp — larger means more recently accessed/admitted.
    pub last_access: u64,
    /// Whether the caller has pinned this entry. A policy MUST NOT return a pinned entry.
    pub pinned: bool,
}

/// The read-only view a policy reasons over when choosing what to evict.
pub struct EvictionContext<'a> {
    /// Every current cache entry EXCEPT the one being admitted.
    pub entries: &'a [EvictionEntry],
    /// Bytes currently held by `entries` (excludes the incoming capsule).
    pub current_bytes: u64,
    /// The cache capacity in bytes.
    pub capacity: u64,
    /// The size of the capsule being admitted (0 for a reconcile/reconfigure sweep).
    pub incoming_size: u64,
}

impl EvictionContext<'_> {
    /// How many bytes must be freed so `current_bytes + incoming_size <= capacity`; 0 if it fits.
    pub fn bytes_to_free(&self) -> u64 {
        (self.current_bytes.saturating_add(self.incoming_size)).saturating_sub(self.capacity)
    }
}

/// Chooses which capsules to evict to make room. See the module docs for the pinned invariant.
pub trait EvictionPolicy: Send + Sync {
    /// Return the capsules to evict (in eviction order) to satisfy [`EvictionContext::bytes_to_free`].
    /// MUST NOT include a pinned entry. Returning too few is allowed — the cache then admits over
    /// capacity (only pins can force that), which is the caller's explicit choice.
    fn select_evictions(&self, ctx: &EvictionContext<'_>) -> Vec<CapsuleIdentity>;
}

/// Least-recently-used eviction: evict the coldest unpinned entries first until enough room is freed.
#[derive(Debug, Default, Clone, Copy)]
pub struct LruPolicy;

impl EvictionPolicy for LruPolicy {
    fn select_evictions(&self, ctx: &EvictionContext<'_>) -> Vec<CapsuleIdentity> {
        let need = ctx.bytes_to_free();
        if need == 0 {
            return Vec::new();
        }

        let mut coldest_first: Vec<&EvictionEntry> =
            ctx.entries.iter().filter(|entry| !entry.pinned).collect();
        coldest_first.sort_by_key(|entry| entry.last_access);

        let mut freed = 0u64;
        let mut victims = Vec::new();
        for entry in coldest_first {
            if freed >= need {
                break;
            }
            freed = freed.saturating_add(entry.size);
            victims.push(entry.id);
        }
        victims
    }
}
