//! End-to-end behaviour tests for `dig-store-cache`.
//!
//! These exercise the crate through its public API against real temp-dir cache roots, with special
//! attention to the DURABILITY paths: atomic admission, disk-authoritative rebuild, orphan recovery,
//! partial-write cleanup, and manifest round-trip.
//!
//! Two kinds of capsule bytes are used:
//! - synthetic arbitrary bytes under a synthetic [`CapsuleIdentity`] — fine for every test that keeps
//!   the manifest (identity comes from the manifest overlay, keyed by the on-disk filename);
//! - the GOLDEN real `.dig` module (byte-identical to `dig-store`'s golden fixture, patched to vary the
//!   non-self-verified `store_id`) — required for the manifest-LESS rebuild tests, where identity must
//!   be recovered by reading the capsule body from disk.

use dig_store::{Bytes32, CapsuleIdentity};
use dig_store_cache::{Cache, CacheConfig, CacheError, PutOptions};

// ---------------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------------

/// A synthetic identity from two seed bytes (store_id = [a; 32], root_hash = [b; 32]).
fn id(a: u8, b: u8) -> CapsuleIdentity {
    CapsuleIdentity {
        store_id: Bytes32::new([a; 32]),
        root_hash: Bytes32::new([b; 32]),
    }
}

/// A cache open on a fresh temp root with the given capacity + default (LRU) policy.
fn open_cache(dir: &std::path::Path, max_bytes: u64) -> Cache {
    Cache::open(
        dir,
        CacheConfig {
            max_bytes,
            ..CacheConfig::default()
        },
    )
    .expect("open cache")
}

const GOLDEN_HEX: &str = include_str!("fixtures/golden_capsule_module.hex");
const GOLDEN_ROOT_HEX: &str = "da2b3372876ec1d3dd9b846f22cb6a9afcb2dadbf579f2930f8e5efe81b0905a";

/// The golden `.dig` module bytes with its (non-self-verified) `store_id` set to `[store_byte; 32]`,
/// yielding a real, parseable capsule with a distinct identity.
fn golden_bytes(store_byte: u8) -> Vec<u8> {
    let mut bytes = hex::decode(GOLDEN_HEX.trim()).expect("decode golden hex");
    // Locate the 32-byte run of 0xAB (the embedded store_id) and overwrite it.
    let run = [0xABu8; 32];
    let at = bytes
        .windows(32)
        .position(|w| w == run)
        .expect("store_id run present");
    for byte in &mut bytes[at..at + 32] {
        *byte = store_byte;
    }
    bytes
}

/// The identity the golden bytes declare for a given store byte.
fn golden_id(store_byte: u8) -> CapsuleIdentity {
    let root: [u8; 32] = hex::decode(GOLDEN_ROOT_HEX).unwrap().try_into().unwrap();
    CapsuleIdentity {
        store_id: Bytes32::new([store_byte; 32]),
        root_hash: Bytes32::new(root),
    }
}

// ---------------------------------------------------------------------------------------------------
// 1. put_file -> get path round-trips identical bytes.
// ---------------------------------------------------------------------------------------------------
#[test]
fn put_file_then_get_round_trips_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 1 << 20);

    let src = dir.path().join("source.bin");
    let payload = vec![7u8; 4096];
    std::fs::write(&src, &payload).unwrap();

    cache
        .put_file(id(1, 2), &src, PutOptions::default())
        .unwrap();

    let hit = cache.get(&id(1, 2)).expect("cached");
    let read_back = std::fs::read(hit.path()).unwrap();
    assert_eq!(read_back, payload);
}

// ---------------------------------------------------------------------------------------------------
// 2. contains + holdings reflect the admitted set.
// ---------------------------------------------------------------------------------------------------
#[test]
fn contains_and_holdings_reflect_admissions() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 1 << 20);

    assert!(!cache.contains(&id(1, 1)));
    cache
        .put_bytes(id(1, 1), b"alpha", PutOptions::default())
        .unwrap();
    cache
        .put_bytes(id(2, 2), b"beta", PutOptions::default())
        .unwrap();

    assert!(cache.contains(&id(1, 1)));
    assert!(cache.contains(&id(2, 2)));
    let mut holdings = cache.holdings();
    holdings.sort_by_key(|c| c.store_id.to_vec());
    assert_eq!(holdings, vec![id(1, 1), id(2, 2)]);
}

// ---------------------------------------------------------------------------------------------------
// 3. LRU eviction: filling past capacity evicts the least-recently-used; Admission.evicted is exact.
// ---------------------------------------------------------------------------------------------------
#[test]
fn lru_eviction_reports_exact_victims() {
    let dir = tempfile::tempdir().unwrap();
    // Capacity holds exactly two 100-byte capsules.
    let cache = open_cache(dir.path(), 200);
    let blob = vec![0u8; 100];

    assert!(cache
        .put_bytes(id(1, 0), &blob, PutOptions::default())
        .unwrap()
        .evicted
        .is_empty());
    assert!(cache
        .put_bytes(id(2, 0), &blob, PutOptions::default())
        .unwrap()
        .evicted
        .is_empty());
    // Third admission overflows -> the oldest (id 1) is evicted.
    let admission = cache
        .put_bytes(id(3, 0), &blob, PutOptions::default())
        .unwrap();

    assert_eq!(admission.evicted, vec![id(1, 0)]);
    assert!(!cache.contains(&id(1, 0)));
    assert!(cache.contains(&id(2, 0)));
    assert!(cache.contains(&id(3, 0)));
}

// ---------------------------------------------------------------------------------------------------
// 4. get updates recency -> a re-read entry survives a later eviction.
// ---------------------------------------------------------------------------------------------------
#[test]
fn get_updates_recency_so_touched_entry_survives() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 200);
    let blob = vec![0u8; 100];

    cache
        .put_bytes(id(1, 0), &blob, PutOptions::default())
        .unwrap();
    cache
        .put_bytes(id(2, 0), &blob, PutOptions::default())
        .unwrap();
    // Touch id 1 so id 2 becomes the least-recently-used.
    cache.get(&id(1, 0)).unwrap();
    let admission = cache
        .put_bytes(id(3, 0), &blob, PutOptions::default())
        .unwrap();

    assert_eq!(admission.evicted, vec![id(2, 0)]);
    assert!(cache.contains(&id(1, 0)));
    assert!(!cache.contains(&id(2, 0)));
}

// ---------------------------------------------------------------------------------------------------
// 5. pin exemption: a pinned entry is never evicted, even when it is the LRU pick.
// ---------------------------------------------------------------------------------------------------
#[test]
fn pinned_entry_is_never_evicted() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 200);
    let blob = vec![0u8; 100];

    // id 1 is the oldest but pinned; id 2 is the eviction candidate.
    cache
        .put_bytes(
            id(1, 0),
            &blob,
            PutOptions {
                pinned: true,
                ..Default::default()
            },
        )
        .unwrap();
    cache
        .put_bytes(id(2, 0), &blob, PutOptions::default())
        .unwrap();
    let admission = cache
        .put_bytes(id(3, 0), &blob, PutOptions::default())
        .unwrap();

    assert_eq!(admission.evicted, vec![id(2, 0)]);
    assert!(cache.contains(&id(1, 0)), "pinned entry must survive");
}

// ---------------------------------------------------------------------------------------------------
// 6. a single capsule larger than capacity is rejected without evicting anything.
// ---------------------------------------------------------------------------------------------------
#[test]
fn over_capacity_single_capsule_is_rejected_without_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 200);

    cache
        .put_bytes(id(1, 0), &[0u8; 100], PutOptions::default())
        .unwrap();
    let err = cache
        .put_bytes(id(9, 0), &[0u8; 300], PutOptions::default())
        .unwrap_err();

    match err {
        CacheError::EntryTooLarge {
            id: got,
            size,
            capacity,
        } => {
            assert_eq!(*got, id(9, 0));
            assert_eq!(size, 300);
            assert_eq!(capacity, 200);
        }
        other => panic!("expected EntryTooLarge, got {other:?}"),
    }
    // The existing capsule is untouched — nothing was evicted for the doomed put.
    assert!(cache.contains(&id(1, 0)));
    assert_eq!(cache.stats().count, 1);
}

// ---------------------------------------------------------------------------------------------------
// 7. remove frees bytes, drops from holdings, and deletes the file.
// ---------------------------------------------------------------------------------------------------
#[test]
fn remove_frees_bytes_and_deletes_file() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 1 << 20);

    cache
        .put_bytes(id(1, 0), &[0u8; 500], PutOptions::default())
        .unwrap();
    let path = cache.get(&id(1, 0)).unwrap().path().to_path_buf();
    assert!(path.exists());

    assert!(cache.remove(&id(1, 0)).unwrap());
    assert!(!cache.contains(&id(1, 0)));
    assert!(cache.holdings().is_empty());
    assert_eq!(cache.stats().bytes_used, 0);
    assert!(!path.exists(), "file must be deleted");
    // Removing a missing id is a no-op returning false.
    assert!(!cache.remove(&id(1, 0)).unwrap());
}

// ---------------------------------------------------------------------------------------------------
// 8. stats stay accurate across admit / get / evict / remove.
// ---------------------------------------------------------------------------------------------------
#[test]
fn stats_track_occupancy() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 250);
    let blob = vec![0u8; 100];

    cache
        .put_bytes(id(1, 0), &blob, PutOptions::default())
        .unwrap();
    cache
        .put_bytes(id(2, 0), &blob, PutOptions::default())
        .unwrap();
    let s = cache.stats();
    assert_eq!((s.count, s.bytes_used, s.capacity), (2, 200, 250));

    cache.get(&id(1, 0)).unwrap();
    // Overflow evicts one.
    cache
        .put_bytes(id(3, 0), &blob, PutOptions::default())
        .unwrap();
    let s = cache.stats();
    assert_eq!((s.count, s.bytes_used), (2, 200));

    cache.remove(&id(1, 0)).unwrap();
    let s = cache.stats();
    assert_eq!((s.count, s.bytes_used), (1, 100));
}

// ---------------------------------------------------------------------------------------------------
// 9. crash recovery: pre-seeded capsules + NO manifest -> open rebuilds holdings from disk, recency
//    from file mtimes (proven by which entry a capacity squeeze then evicts).
// ---------------------------------------------------------------------------------------------------
#[test]
fn rebuild_from_disk_without_manifest_uses_mtime_recency() {
    let dir = tempfile::tempdir().unwrap();

    // Seed two REAL capsules (distinct store_ids) so their identity is recoverable from disk.
    {
        let cache = open_cache(dir.path(), 1 << 20);
        cache
            .put_bytes(golden_id(0x01), &golden_bytes(0x01), PutOptions::default())
            .unwrap();
        cache
            .put_bytes(golden_id(0x02), &golden_bytes(0x02), PutOptions::default())
            .unwrap();
    }
    let capsule_size = golden_bytes(0x01).len() as u64;

    // Stamp distinct mtimes on the two on-disk capsules: 0x01 older than 0x02. The on-disk path for an
    // identity is found by reopening (its filename is the retrieval-key hash, not the store byte).
    let path_of = |sid: u8| {
        open_cache(dir.path(), 1 << 20)
            .get(&golden_id(sid))
            .unwrap()
            .path()
            .to_path_buf()
    };
    filetime::set_file_mtime(path_of(0x01), filetime::FileTime::from_unix_time(1_000, 0)).unwrap();
    filetime::set_file_mtime(path_of(0x02), filetime::FileTime::from_unix_time(2_000, 0)).unwrap();

    // Drop the manifest to force a disk-only rebuild.
    std::fs::remove_file(dir.path().join("index.json")).unwrap();

    let cache = open_cache(dir.path(), 1 << 20);
    let mut holdings = cache.holdings();
    holdings.sort_by_key(|c| c.store_id.to_vec());
    assert_eq!(
        holdings,
        vec![golden_id(0x01), golden_id(0x02)],
        "holdings rebuilt from disk"
    );

    // Squeeze capacity to one capsule -> the mtime-older (0x01) must be the eviction victim, proving
    // recency was recovered from mtimes.
    let admission = cache
        .set_config(CacheConfig {
            max_bytes: capsule_size,
            ..CacheConfig::default()
        })
        .unwrap();
    assert_eq!(admission.evicted, vec![golden_id(0x01)]);
    assert!(cache.contains(&golden_id(0x02)));
}

// ---------------------------------------------------------------------------------------------------
// 10. partial-write recovery: a leftover tmp/*.part is cleaned on open; complete capsules survive.
// ---------------------------------------------------------------------------------------------------
#[test]
fn open_cleans_leftover_part_files_and_keeps_capsules() {
    let dir = tempfile::tempdir().unwrap();
    {
        let cache = open_cache(dir.path(), 1 << 20);
        cache
            .put_bytes(golden_id(0x05), &golden_bytes(0x05), PutOptions::default())
            .unwrap();
    }
    // Simulate an interrupted admission: a stray .part in tmp/.
    let stray = dir.path().join("tmp").join("deadbeef.part");
    std::fs::write(&stray, b"half-written").unwrap();

    let cache = open_cache(dir.path(), 1 << 20);
    assert!(!stray.exists(), "leftover .part must be reclaimed on open");
    assert_eq!(
        cache.holdings(),
        vec![golden_id(0x05)],
        "complete capsule survives"
    );
}

// ---------------------------------------------------------------------------------------------------
// 11. atomic admission: put_file never leaves a .part inside capsules/.
// ---------------------------------------------------------------------------------------------------
#[test]
fn admission_leaves_no_part_in_capsules_dir() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 1 << 20);

    let src = dir.path().join("src.bin");
    std::fs::write(&src, vec![1u8; 2048]).unwrap();
    cache
        .put_file(id(4, 4), &src, PutOptions::default())
        .unwrap();

    let cap_dir = dir.path().join("capsules");
    for entry in std::fs::read_dir(&cap_dir).unwrap().flatten() {
        let path = entry.path();
        assert_ne!(
            path.extension().and_then(|s| s.to_str()),
            Some("part"),
            "no .part in capsules/"
        );
        assert_eq!(path.extension().and_then(|s| s.to_str()), Some("dig"));
    }
    // tmp/ holds no residue either.
    assert_eq!(
        std::fs::read_dir(dir.path().join("tmp")).unwrap().count(),
        0
    );
}

// ---------------------------------------------------------------------------------------------------
// 12. manifest round-trip: admit + pin, reopen -> recency + pinned preserved.
// ---------------------------------------------------------------------------------------------------
#[test]
fn manifest_round_trip_preserves_pin_and_recency() {
    let dir = tempfile::tempdir().unwrap();
    let blob = vec![0u8; 100];
    {
        let cache = open_cache(dir.path(), 300);
        cache
            .put_bytes(
                id(1, 0),
                &blob,
                PutOptions {
                    pinned: true,
                    ..Default::default()
                },
            )
            .unwrap();
        cache
            .put_bytes(id(2, 0), &blob, PutOptions::default())
            .unwrap();
        cache
            .put_bytes(id(3, 0), &blob, PutOptions::default())
            .unwrap();
    }

    // Reopen with the manifest present (arbitrary bytes are fine — identity comes from the manifest).
    let cache = open_cache(dir.path(), 300);
    assert_eq!(cache.holdings().len(), 3);

    // The pin survived: squeezing to two capsules must NOT evict the pinned id 1; it evicts the LRU
    // unpinned one (id 2).
    let admission = cache
        .set_config(CacheConfig {
            max_bytes: 200,
            ..CacheConfig::default()
        })
        .unwrap();
    assert_eq!(admission.evicted, vec![id(2, 0)]);
    assert!(cache.contains(&id(1, 0)), "pinned survives reopen");
    assert!(cache.contains(&id(3, 0)));
}

// ---------------------------------------------------------------------------------------------------
// 13. reconfigure: set_config lowering max_bytes evicts to fit and returns the evicted set.
// ---------------------------------------------------------------------------------------------------
#[test]
fn set_config_lowering_capacity_evicts_to_fit() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 1000);
    let blob = vec![0u8; 100];
    for n in 0..5u8 {
        cache
            .put_bytes(id(n, 0), &blob, PutOptions::default())
            .unwrap();
    }
    assert_eq!(cache.stats().count, 5);

    // Drop to hold only two -> three oldest evicted.
    let admission = cache
        .set_config(CacheConfig {
            max_bytes: 200,
            ..CacheConfig::default()
        })
        .unwrap();
    assert_eq!(admission.evicted, vec![id(0, 0), id(1, 0), id(2, 0)]);
    assert_eq!(cache.stats().count, 2);
    assert!(cache.stats().bytes_used <= 200);
}

// ---------------------------------------------------------------------------------------------------
// 14. check_identity = true with mismatched bytes -> IdentityMismatch, nothing admitted.
// ---------------------------------------------------------------------------------------------------
#[test]
fn check_identity_rejects_mismatched_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 1 << 20);

    // Real golden bytes declaring store_id 0x07, but we CLAIM 0x08.
    let bytes = golden_bytes(0x07);
    let claimed = golden_id(0x08);
    let err = cache
        .put_bytes(
            claimed,
            &bytes,
            PutOptions {
                check_identity: true,
                ..Default::default()
            },
        )
        .unwrap_err();

    match err {
        CacheError::IdentityMismatch {
            claimed: c,
            recovered,
        } => {
            assert_eq!(*c, claimed);
            assert_eq!(*recovered, golden_id(0x07));
        }
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    assert!(!cache.contains(&claimed));

    // The matching claim with check_identity is admitted.
    cache
        .put_bytes(
            golden_id(0x07),
            &bytes,
            PutOptions {
                check_identity: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(cache.contains(&golden_id(0x07)));
}

// ---------------------------------------------------------------------------------------------------
// 15. the Cache handle is Send + Sync + Clone and safe to share across threads.
// ---------------------------------------------------------------------------------------------------
#[test]
fn cache_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync + Clone>() {}
    assert_send_sync::<Cache>();

    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 1 << 20);

    let handles: Vec<_> = (0..8u8)
        .map(|n| {
            let cache = cache.clone();
            std::thread::spawn(move || {
                cache
                    .put_bytes(id(n, 0), &[n; 256], PutOptions::default())
                    .unwrap();
                assert!(cache.get(&id(n, 0)).is_some());
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(cache.stats().count, 8);
}

// ---------------------------------------------------------------------------------------------------
// Extra coverage: convenience reads, pin/unpin semantics, error surfaces, and rebuild edge cases.
// ---------------------------------------------------------------------------------------------------

#[test]
fn get_bytes_reads_back_and_misses_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 1 << 20);

    assert_eq!(cache.get_bytes(&id(1, 0)).unwrap(), None);
    cache
        .put_bytes(id(1, 0), b"payload", PutOptions::default())
        .unwrap();
    assert_eq!(
        cache.get_bytes(&id(1, 0)).unwrap().as_deref(),
        Some(&b"payload"[..])
    );
}

#[test]
fn pin_and_unpin_report_presence_and_toggle_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 200);
    let blob = vec![0u8; 100];

    assert!(
        !cache.pin(&id(1, 0)).unwrap(),
        "pinning a missing id returns false"
    );
    assert!(!cache.unpin(&id(1, 0)).unwrap());

    cache
        .put_bytes(id(1, 0), &blob, PutOptions::default())
        .unwrap();
    cache
        .put_bytes(id(2, 0), &blob, PutOptions::default())
        .unwrap();
    assert!(cache.pin(&id(1, 0)).unwrap());

    // Pinned id 1 (the oldest) is protected; id 2 is evicted instead.
    assert_eq!(
        cache
            .put_bytes(id(3, 0), &blob, PutOptions::default())
            .unwrap()
            .evicted,
        vec![id(2, 0)]
    );

    // Unpin id 1 -> it becomes evictable again and loses to the newer id 3.
    assert!(cache.unpin(&id(1, 0)).unwrap());
    assert_eq!(
        cache
            .put_bytes(id(4, 0), &blob, PutOptions::default())
            .unwrap()
            .evicted,
        vec![id(1, 0)]
    );
}

#[test]
fn set_config_raising_capacity_evicts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(dir.path(), 200);
    cache
        .put_bytes(id(1, 0), &[0u8; 100], PutOptions::default())
        .unwrap();

    let admission = cache
        .set_config(CacheConfig {
            max_bytes: 10_000,
            ..CacheConfig::default()
        })
        .unwrap();
    assert!(admission.evicted.is_empty());
    assert_eq!(cache.stats().capacity, 10_000);
}

#[test]
fn error_display_is_informative() {
    let too_large = CacheError::EntryTooLarge {
        id: Box::new(id(1, 0)),
        size: 500,
        capacity: 200,
    };
    assert!(too_large
        .to_string()
        .contains("larger than the cache capacity"));

    let root = CacheError::RootNotWritable {
        root: std::path::PathBuf::from("/nope"),
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
    };
    assert!(root.to_string().contains("not writable"));
}

#[test]
fn open_on_a_non_directory_root_reports_root_not_writable() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("a-file");
    std::fs::write(&file_path, b"not a dir").unwrap();

    let err = Cache::open(&file_path, CacheConfig::default()).unwrap_err();
    assert!(
        matches!(err, CacheError::RootNotWritable { .. }),
        "got {err:?}"
    );
}

#[test]
fn rebuild_skips_foreign_files_in_capsules_dir() {
    let dir = tempfile::tempdir().unwrap();
    {
        let cache = open_cache(dir.path(), 1 << 20);
        cache
            .put_bytes(golden_id(0x0a), &golden_bytes(0x0a), PutOptions::default())
            .unwrap();
    }
    // Drop a non-capsule file into capsules/ with no manifest entry — it must be ignored, not crash.
    std::fs::write(
        dir.path().join("capsules").join("deadbeef.dig"),
        b"not a real dig module",
    )
    .unwrap();
    std::fs::remove_file(dir.path().join("index.json")).unwrap();

    let cache = open_cache(dir.path(), 1 << 20);
    assert_eq!(
        cache.holdings(),
        vec![golden_id(0x0a)],
        "foreign file excluded from holdings"
    );
}
