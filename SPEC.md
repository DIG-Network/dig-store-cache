# dig-store-cache — SPEC

Normative specification for the DIG Node's on-disk capsule cache. An independent reimplementation MUST
satisfy every MUST/MUST NOT below.

## 1. Purpose and boundary

`dig-store-cache` is a bounded, crash-safe, content-addressed store of already-verified `.dig` capsules.
It is the **cache + reshare** leg of the content-replication flywheel: a verified capsule admitted here
becomes a discoverable holding the node re-advertises.

The crate is a **pure filesystem primitive**:

- It MUST NOT depend on chain, network, or key material. Its only DIG dependency is `dig-store`
  (identity + retrieval-key derivation).
- It is **caller-verifies**: `put` MUST assume the bytes were already integrity-verified by the caller
  (dig-node runs the NC-9 merkle/chain verify). The cache performs NO merkle or on-chain verification.
- It MUST NOT choose its own location — the caller passes `root`.
- It MUST NOT encrypt capsules. Capsules are public, re-fetchable artifacts; at-rest confidentiality
  is not this crate's concern.

## 2. Content identity

The content id is `dig_store::CapsuleIdentity { store_id: Bytes32, root_hash: Bytes32 }`, re-exported
unchanged. A capsule's on-disk filename stem is the **retrieval key**:
`retrieval_key(urn:dig:chia:<store_id>:<root_hash>)`, hex-encoded, derived via `dig-store`. The
retrieval key is a one-way hash; identity is NOT recoverable from the filename alone (see §5).

## 3. Disk layout

Under the caller-supplied `root`:

```
<root>/capsules/<retrieval_key_hex>.dig   admitted capsules — the AUTHORITATIVE holdings set
<root>/tmp/<uuid>.part                    in-flight admissions (staged, fsync'd, then renamed)
<root>/index.json                         the ADVISORY manifest (recency + pin + identity overlay)
```

`index.json` schema (v1):

```json
{ "version": 1, "next_seq": 42,
  "entries": [ { "store_id": "<hex>", "root_hash": "<hex>", "retrieval_key": "<hex>",
                 "size": 123, "seq": 10, "pinned": true } ] }
```

## 4. Admission protocol (atomic + durable)

`put_file`/`put_bytes` MUST admit atomically:

1. Reject with `EntryTooLarge` if `size > max_bytes` (a capsule that can never fit) — WITHOUT evicting.
2. Stage the bytes to `tmp/<uuid>.part` and `fsync` them.
3. Ask the policy which capsules to evict to make room (§6), delete them, then atomically `rename` the
   staged file into `capsules/`. A capsule file in `capsules/` is therefore ALWAYS complete — a crash
   at any point leaves at most a stray `.part`, never a half capsule.

`put_file` MUST stream-copy the source (never read the whole capsule into memory). `check_identity` is
IGNORED by `put_file` (the body is not read). `put_bytes` with `check_identity = true` MUST recover the
bytes' declared identity via `dig_store::get_capsule_identity` and return `IdentityMismatch` unless it
equals the claimed id.

## 5. Index rebuild contract (disk authoritative, manifest advisory)

On `open` the cache MUST:

1. Create `capsules/` + `tmp/` (else `RootNotWritable`).
2. Delete every leftover `tmp/*.part`.
3. Scan `capsules/` — the authoritative set of held capsules; sizes are read from disk.
4. Overlay `index.json`:
   - a scanned file WITH a manifest entry takes its identity, `seq` (recency), and `pinned` from the
     manifest; its size from disk;
   - a scanned file WITHOUT a manifest entry (orphan) has its identity recovered by memory-mapping the
     file and reading its `.dig` header; recency = file mtime; `pinned = false`. A file that cannot be
     read as a capsule, or is filed under the wrong retrieval key, MUST be left on disk and excluded
     from holdings (foreign/corrupt);
   - a manifest entry WITHOUT a file MUST be dropped.
5. Persist the reconciled manifest.

A lost or corrupt manifest MUST cost only recency ordering (recovered from mtimes) and pin state — never
a capsule. The manifest MUST be written on every structural change (admit, evict, remove, pin, unpin)
and on `open`'s reconcile, and MUST NOT be written on `get` (recency is bumped in memory only).

## 6. Eviction contract

Eviction is pluggable via `EvictionPolicy::select_evictions(&EvictionContext) -> Vec<CapsuleIdentity>`.
A policy MUST NOT return a pinned entry. The default `LruPolicy` evicts the coldest unpinned entries
first until `current_bytes + incoming_size <= max_bytes`. `get` updates recency so a touched entry is
protected. Pinned entries are exempt from eviction and MAY push `bytes_used` over `max_bytes` (the
caller's explicit override). `Admission.evicted` MUST list exactly the evicted ids (the flywheel
retract signal). `set_config` lowering `max_bytes` MUST evict to fit and return the evicted set.

## 7. Public API

`open`, `put_file`, `put_bytes`, `get` (recency-updating; `CachedCapsule::path` for streaming),
`get_bytes` (convenience), `contains`, `holdings`, `remove`, `pin`, `unpin`, `stats`, `set_config`. The
`Cache` handle is `Clone + Send + Sync`.

## 8. Error taxonomy

`CacheError`: `Io { path, source }`, `EntryTooLarge { id, size, capacity }`,
`IdentityMismatch { claimed, recovered }`, `CorruptEntry { path, reason }`,
`RootNotWritable { root, source }`.

## 9. Configuration defaults

`CacheConfig::default()` = `max_bytes = 1 GiB` (`1 << 30`), `policy = LruPolicy`.

## 10. Invariants

- INV-1 A file in `capsules/` is always a complete capsule (atomic rename).
- INV-2 Holdings == the set of readable, correctly-named `.dig` files in `capsules/`.
- INV-3 A pinned entry is never evicted by any policy.
- INV-4 `stats.bytes_used` == the sum of held capsule sizes.
- INV-5 No chain/network/key dependency; no capsule body read except optional `check_identity` and
  orphan recovery.

## 11. Backwards compatibility (§5.1)

The cache stores `.dig` bytes VERBATIM and never rewrites them; `.dig` format back-compat is inherited
from `dig-store` (this crate adds none of its own format). The `index.json` manifest is advisory and
versioned; it MUST be evolved additively (new optional fields do not bump `version`), and a manifest it
cannot parse MUST be treated as absent (rebuild from disk), never a hard failure.
