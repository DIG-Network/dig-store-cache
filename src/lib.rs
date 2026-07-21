//! # dig-store-cache
//!
//! The DIG Node's on-disk cache of already-verified `.dig` capsules — the **cache + reshare** leg of
//! the content-replication flywheel (`install → connect → discover → read → CACHE → reshare`). When
//! the node fetches and verifies a capsule, it admits the finished bytes here; the cache holds them
//! under a bounded, evicted (pluggable policy, LRU by default), pin-aware, crash-safe store and reports
//! the resulting [`holdings`](Cache::holdings) so the node can announce itself as a provider. Every
//! read therefore makes content MORE available.
//!
//! ## Boundary — a pure filesystem primitive
//!
//! This crate has NO chain, NO network, and NO key dependencies. It **trusts the caller to have
//! verified content** before [`put_file`](Cache::put_file)/[`put_bytes`](Cache::put_bytes) (dig-node
//! runs the NC-9 merkle/chain verify), and it does not choose where it lives — the caller passes the
//! `root`. It re-exports [`dig_store::CapsuleIdentity`] as THE content id so the whole ecosystem speaks
//! one identity type. The optional [`PutOptions::check_identity`] adds a cheap structural sanity check
//! (the bytes' declared identity equals the claim) — not a substitute for the caller's real verify.
//!
//! ## Durability contract
//!
//! Disk is authoritative; the `index.json` manifest is an advisory overlay. Admission is atomic (stage
//! to `tmp/`, fsync, rename into `capsules/`), so a crash never leaves a half file among the capsules.
//! On [`open`](Cache::open) the index is rebuilt by SCANNING `capsules/` and overlaying the manifest
//! for recency + pin state; a lost manifest costs only recency ordering (recovered from mtimes) and
//! pins, never a capsule. See [`SPEC.md`](https://github.com/DIG-Network/dig-store-cache/blob/main/SPEC.md).
//!
//! ## Example
//!
//! ```no_run
//! use dig_store_cache::{Cache, CacheConfig, PutOptions, CapsuleIdentity};
//! use std::path::Path;
//!
//! # fn demo(id: CapsuleIdentity, verified: &Path) -> Result<(), dig_store_cache::CacheError> {
//! let cache = Cache::open(Path::new("/var/lib/dig/cache"), CacheConfig::default())?;
//! let admission = cache.put_file(id, verified, PutOptions::default())?;
//! // `admission.evicted` lists capsules the node must stop advertising.
//! if let Some(hit) = cache.get(&id) {
//!     // stream from `hit.path()` — capsules can be ~1 GiB, never assume they fit in RAM
//!     let _path = hit.path();
//! }
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

mod cache;
mod config;
mod error;
mod index;
mod layout;
mod policy;

pub use cache::{Cache, CachedCapsule};
pub use config::{Admission, CacheConfig, CacheStats, PutOptions, DEFAULT_MAX_BYTES};
pub use error::CacheError;
pub use policy::{EvictionContext, EvictionEntry, EvictionPolicy, LruPolicy};

/// The content id a capsule is addressed by, re-exported from `dig-store` so the ecosystem speaks ONE
/// identity type (`{ store_id, root_hash }`).
pub use dig_store::CapsuleIdentity;
