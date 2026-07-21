# dig-store-cache

The DIG Node's on-disk cache of already-verified `.dig` capsules — the **cache + reshare** leg of the
content-replication flywheel (`install → connect → discover → read → CACHE → reshare`). Every capsule a
node fetches and verifies is admitted here and becomes a discoverable holding, so every read makes
content more available.

```toml
[dependencies]
dig-store-cache = "0.1"
```

## What it is

A bounded, crash-safe, content-addressed store with pluggable (LRU by default) eviction and pinning:

- **Pure filesystem primitive** — no chain, no network, no keys. The caller verifies content before
  `put` (dig-node runs the NC-9 merkle/chain verify) and chooses where the cache lives.
- **Path-based byte flow** — capsules can be ~1 GiB, so admission stream-copies and reads go through
  `CachedCapsule::path()`; never assume a capsule fits in RAM.
- **Crash-safe** — atomic admission (stage → fsync → rename); disk is authoritative and the cache
  rebuilds from a scan on `open`, so a lost manifest costs only recency ordering + pins, never a capsule.

## Usage

```rust
use dig_store_cache::{Cache, CacheConfig, PutOptions, CapsuleIdentity};
use std::path::Path;

let cache = Cache::open(Path::new("/var/lib/dig/cache"), CacheConfig::default())?;

// Admit an already-verified capsule.
let admission = cache.put_file(id, Path::new("/tmp/verified.dig"), PutOptions::default())?;
// `admission.evicted` = capsules the node must stop advertising.

// Read it back (streams from disk).
if let Some(hit) = cache.get(&id) {
    let path = hit.path();
}

// Advertisable holdings for the DHT provider announce.
let holdings: Vec<CapsuleIdentity> = cache.holdings();
```

## Documentation

- [`SPEC.md`](SPEC.md) — the normative contract (layout, admission, rebuild, eviction, invariants).
- [`runbooks/`](runbooks/) — release (crates.io) + local development.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
