//! Delete semantics: tombstone recording, eviction, and resurrection.

#[path = "common.rs"]
mod common;
use common::{doc, sync};

use loro_internal::{cursor::PosType, loro::ExportMode, HandlerTrait, IdLp, ToJson};
use serde_json::json;

/// Calling `MapHandler::delete(key)` on a key holding a mergeable child evicts the side-table
/// entry via tombstoning: the child no longer appears in deep value. The child's underlying KV
/// state IS preserved (delete detaches rather than destroys). A subsequent `get_mergeable_*`
/// call returns a handler to that preserved state, but the child stays hidden from deep-value
/// walks until a new op with IdLp > tombstone arrives.
#[test]
#[cfg(feature = "counter")]
fn delete_on_mergeable_child_key_detaches_and_preserves_state() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(3.0).unwrap();
    doc.commit_then_renew();
    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 3.0 } })
    );

    root.delete("revision").unwrap();
    doc.commit_then_renew();

    // Detached: not in deep value.
    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": {} }),
        "delete must evict the mergeable child from deep value"
    );

    // State is preserved in KV. Re-getting the counter resolves to the same deterministic cid;
    // the handle works; the value is the PRIOR value (3.0), not a reset to 0.0.
    let counter2 = root.get_mergeable_counter("revision").unwrap();
    assert_eq!(counter2.id(), counter.id(), "deterministic cid is stable");
    assert_eq!(
        counter2.get_value().to_json_value(),
        json!(3.0),
        "re-get after delete sees preserved state, not reset state"
    );

    // The counter is still NOT in deep value (no post-tombstone op has
    // re-registered the cid yet).
    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": {} }),
        "re-getting the handle does not resurrect the cid; only a post-tombstone op does"
    );

    // Issuing a new op (any op) re-registers the cid via the
    // resurrection path.
    counter2.increment(10.0).unwrap();
    doc.commit_then_renew();
    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 13.0 } }),
        "post-tombstone increment resurrects the cid; value is preserved + new increment"
    );
}


/// After `delete` on a mergeable key, the child does not appear in
/// deep value. The mergeable cid has been evicted from the side table
/// because its max-op-idlp is <= the tombstone.
#[test]
#[cfg(feature = "counter")]
fn delete_on_mergeable_key_removes_child_from_deep_value() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(3.0).unwrap();
    doc.commit_then_renew();
    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 3.0 } })
    );

    root.delete("revision").unwrap();
    doc.commit_then_renew();

    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": {} }),
        "after delete, mergeable child must not appear in deep value"
    );
}


/// Calling `MapHandler::delete(key)` on a key with a mergeable child
/// records a tombstone in the parent MapState. The tombstone's IdLp
/// matches the delete op's IdLp (peer, lamport).
#[test]
#[cfg(feature = "counter")]
fn delete_on_mergeable_key_records_tombstone() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(1.0).unwrap();
    doc.commit_then_renew();

    // Read parent MapState; tombstone for "revision" should be None.
    let before = root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_tombstone(&"revision".into()))
        })
        .unwrap();
    assert!(
        before.is_none(),
        "no tombstone before delete; got {before:?}"
    );

    root.delete("revision").unwrap();
    doc.commit_then_renew();

    let after = root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_tombstone(&"revision".into()))
        })
        .unwrap()
        .expect("tombstone must exist after delete on mergeable key");
    assert_eq!(after.peer, 1, "tombstone peer must be the local peer");
    assert!(
        after.lamport > 0,
        "tombstone lamport must be > 0 after a delete"
    );
}


/// A local `delete` on a mergeable key must evict the side-table entry
/// immediately, not wait for a remote import cycle. The child stops
/// appearing in deep value right after `commit_then_renew`.
#[test]
#[cfg(feature = "counter")]
fn local_delete_immediately_evicts_mergeable_side_table() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(1.0).unwrap();
    doc.commit_then_renew();
    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 1.0 } })
    );

    root.delete("revision").unwrap();
    doc.commit_then_renew();

    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": {} }),
        "local delete must remove the mergeable child from deep value"
    );
}


/// After local `delete`, calling `get_mergeable_*` on the same key
/// returns a working handler but does NOT re-register the cid in the
/// side table. Deep value still hides the child until a post-tombstone
/// op arrives.
#[test]
#[cfg(feature = "counter")]
fn get_mergeable_after_delete_does_not_resurrect() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(1.0).unwrap();
    doc.commit_then_renew();

    root.delete("revision").unwrap();
    doc.commit_then_renew();
    assert_eq!(doc.get_deep_value().to_json_value(), json!({ "state": {} }));

    // Re-get returns a working handler. The cid is the same (deterministic).
    let counter2 = root.get_mergeable_counter("revision").unwrap();
    assert_eq!(counter2.id(), counter.id(), "deterministic cid is stable");

    // But the cid is NOT re-registered. Deep value still hides it.
    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": {} }),
        "re-get must not bypass the tombstone gate"
    );

    // The handler still works (reads preserved KV state).
    assert_eq!(
        counter2.get_value().to_json_value(),
        json!(1.0),
        "handler reads preserved KV state"
    );
}


/// After local `delete`, a local mutation on the same mergeable child
/// (via the handler returned from a post-delete `get_mergeable_*`)
/// must re-register the cid in the parent's side table. Deep value
/// shows the counter again, with the preserved prior value plus the
/// new increment.
#[test]
#[cfg(feature = "counter")]
fn local_post_tombstone_mutation_resurrects_cid() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(3.0).unwrap();
    doc.commit_then_renew();

    root.delete("revision").unwrap();
    doc.commit_then_renew();
    assert_eq!(doc.get_deep_value().to_json_value(), json!({ "state": {} }));

    // Re-get returns a handler but does not resurrect the cid on its own.
    let counter2 = root.get_mergeable_counter("revision").unwrap();

    // Local increment: this op's IdLp will be > tombstone (clock advanced
    // by the delete itself plus this commit). Should resurrect.
    counter2.increment(10.0).unwrap();
    doc.commit_then_renew();

    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 13.0 } }),
        "post-tombstone local increment must resurrect cid; value is 3.0 + 10.0 = 13.0 (state preserved)"
    );
}


/// Peer B has a mergeable counter registered and visible. Peer A deletes
/// the same key with a higher IdLp. After sync, B's deep value must hide
/// the counter — the remote delete propagates as a tombstone and the
/// post-loop reconciliation evicts the side-table entry.
#[test]
#[cfg(feature = "counter")]
fn remote_delete_evicts_registered_mergeable_child_on_receiver() {
    let a = doc(1);
    let b = doc(2);

    // B creates and increments the counter, syncs to A.
    let b_root = b.get_map("state");
    let b_counter = b_root.get_mergeable_counter("revision").unwrap();
    b_counter.increment(1.0).unwrap();
    b.commit_then_renew();
    sync(&a, &b);
    assert_eq!(
        b.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 1.0 } })
    );

    // A advances its clock past B's counter op, then deletes.
    let a_root = a.get_map("state");
    for i in 0..5 {
        a_root.insert(&format!("noise_{i}"), i).unwrap();
        a.commit_then_renew();
    }
    a_root.delete("revision").unwrap();
    a.commit_then_renew();

    // Sync A's delete to B. B's side-table entry for "revision" must be
    // evicted because A's tombstone IdLp > B's increment IdLp.
    sync(&a, &b);

    let vb = b.get_deep_value().to_json_value();
    assert!(
        vb["state"].get("revision").is_none(),
        "remote delete must evict B's side-table entry; got {vb}"
    );
}


/// Focused regression for the per-cid reconciliation rule. The side table is
/// seeded with two cids under the same key: an old dominated counter and a
/// newer reachable text. Reconciliation must evict only the dominated cid,
/// not bulk-remove the whole key.
#[test]
#[cfg(feature = "counter")]
fn remote_delete_evicts_only_dominated_cid_under_mixed_reachability() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let key = "revision";

    let counter = root.get_mergeable_counter(key).unwrap();
    counter.increment(1.0).unwrap();
    doc.commit_then_renew();
    let counter_cid = counter.id();

    // Remove the counter side-table entry so we can create a competing text
    // cid under the same key through the public mergeable getter.
    root.with_state(|state| {
        state
            .as_map_state_mut()
            .unwrap()
            .evict_mergeable_child_cid(&counter_cid);
        Ok(())
    })
    .unwrap();

    let text = root.get_mergeable_text(key).unwrap();
    text.insert(0, "reachable", PosType::Unicode).unwrap();
    doc.commit_then_renew();
    let text_cid = text.id();

    let tombstone = IdLp::new(1, 0);
    {
        let oplog = doc.oplog().lock();
        let state = doc.app_state().lock();
        let counter_max = state
            .mergeable_max_op_idlp(&oplog, &counter_cid)
            .expect("counter must have an op");
        let text_max = state
            .mergeable_max_op_idlp(&oplog, &text_cid)
            .expect("text must have an op");
        assert!(
            counter_max <= tombstone,
            "test precondition: counter should be dominated ({counter_max:?} <= {tombstone:?})"
        );
        assert!(
            text_max > tombstone,
            "test precondition: text should remain reachable ({text_max:?} > {tombstone:?})"
        );
    }

    // Seed the mixed side-table state directly: both cids under one key plus a
    // tombstone that dominates only the counter.
    root.with_state(|state| {
        let map = state.as_map_state_mut().unwrap();
        map.register_mergeable_child(key.into(), counter_cid.clone());
        map.register_mergeable_child(key.into(), text_cid.clone());
        map.set_mergeable_tombstone(key.into(), tombstone);
        Ok(())
    })
    .unwrap();

    let before = root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_child_ids_for_key(&key.into()))
        })
        .unwrap();
    assert!(before.contains(&counter_cid));
    assert!(before.contains(&text_cid));

    {
        let oplog_g = doc.oplog().lock();
        let mut app_state = doc.app_state().lock();
        app_state.reconcile_mergeable_tombstones(&oplog_g);
    }

    let after = root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_child_ids_for_key(&key.into()))
        })
        .unwrap();
    assert!(
        !after.contains(&counter_cid),
        "dominated counter cid must be evicted; got {after:?}"
    );
    assert!(
        after.contains(&text_cid),
        "reachable text cid must survive per-cid reconciliation; got {after:?}"
    );
}


/// Three-peer smoke check: a remote delete tombstone should evict stale
/// side-table entries on receivers, allowing a later competing-kind mergeable
/// child with post-tombstone ops to become visible and converge.
#[test]
#[cfg(feature = "counter")]
fn three_peer_delete_resurrect_with_competing_kinds() {
    let a = doc(1);
    let b = doc(2);
    let c = doc(3);

    let b_root = b.get_map("state");
    let b_counter = b_root.get_mergeable_counter("revision").unwrap();
    b_counter.increment(1.0).unwrap();
    b.commit_then_renew();

    sync(&a, &b);
    sync(&b, &c);
    assert_eq!(
        c.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 1.0 } })
    );

    let a_root = a.get_map("state");
    for i in 0..6 {
        a_root.insert(&format!("noise_{i}"), i).unwrap();
        a.commit_then_renew();
    }
    a_root.delete("revision").unwrap();
    a.commit_then_renew();

    sync(&a, &b);
    sync(&a, &c);
    assert!(
        b.get_deep_value().to_json_value()["state"]
            .get("revision")
            .is_none(),
        "B must hide the stale counter after importing A's delete"
    );
    assert!(
        c.get_deep_value().to_json_value()["state"]
            .get("revision")
            .is_none(),
        "C must hide the stale counter after importing A's delete"
    );

    let c_text = c.get_map("state").get_mergeable_text("revision").unwrap();
    c_text.insert(0, "after-delete", PosType::Unicode).unwrap();
    c.commit_then_renew();

    sync(&b, &c);
    sync(&a, &c);
    sync(&a, &b);

    let expected = json!({
        "state": {
            "noise_0": 0,
            "noise_1": 1,
            "noise_2": 2,
            "noise_3": 3,
            "noise_4": 4,
            "noise_5": 5,
            "revision": "after-delete",
        }
    });
    assert_eq!(
        a.get_deep_value().to_json_value(),
        expected,
        "A must converge"
    );
    assert_eq!(
        b.get_deep_value().to_json_value(),
        expected,
        "B must converge"
    );
    assert_eq!(
        c.get_deep_value().to_json_value(),
        expected,
        "C must converge"
    );
}


/// Concurrent delete + increment with increment-higher-IdLp: the
/// increment's op carries the cid past the tombstone, so the side
/// table re-registers the cid, and the counter remains visible.
#[test]
#[cfg(feature = "counter")]
fn concurrent_increment_resurrects_after_delete() {
    let a = doc(1);
    let b = doc(2);

    let a_root = a.get_map("state");
    let a_counter = a_root.get_mergeable_counter("revision").unwrap();
    a_counter.increment(1.0).unwrap();
    a.commit_then_renew();
    sync(&a, &b);

    // A deletes the counter at some IdLp L_a.
    a_root.delete("revision").unwrap();
    a.commit_then_renew();

    // B has not seen A's delete yet. B advances its own lamport clock by
    // doing some unrelated ops, then increments. B's increment IdLp will
    // be > A's delete IdLp.
    for i in 0..5 {
        b.get_map("state").insert(&format!("noise_{i}"), i).unwrap();
        b.commit_then_renew();
    }
    let b_counter = b.get_map("state").get_mergeable_counter("revision").unwrap();
    b_counter.increment(100.0).unwrap();
    b.commit_then_renew();

    // Sync. Both peers receive each other's ops.
    sync(&a, &b);

    let va = a.get_deep_value().to_json_value();
    let vb = b.get_deep_value().to_json_value();
    assert_eq!(va, vb, "peers must converge");

    // The counter must be visible: B's increment IdLp > A's delete IdLp, so the increment
    // dominates and the cid is re-registered. The visible value is the counter's preserved
    // total state — not "reset to zero then incremented":
    //
    //   pre-delete:        1.0 (A's increment)
    //   after delete:      1.0 (no state change to the counter container itself)
    //   after B's +100.0:  101.0
    let revision = &va["state"]["revision"];
    assert_eq!(
        revision,
        &json!(101.0),
        "increment past tombstone reattaches the cid; state is preserved (not reset)"
    );
}


/// Concurrent increment + delete with delete-higher-IdLp: no op past the
/// tombstone, so the cid stays evicted. Counter is not visible in deep
/// value. KV state is preserved (1.0) but unreachable from the parent.
#[test]
#[cfg(feature = "counter")]
fn concurrent_delete_wins_against_earlier_increment() {
    let a = doc(1);
    let b = doc(2);

    let a_root = a.get_map("state");
    let a_counter = a_root.get_mergeable_counter("revision").unwrap();
    a_counter.increment(1.0).unwrap();
    a.commit_then_renew();
    sync(&a, &b);

    // B increments first (low IdLp).
    let b_root = b.get_map("state");
    let b_counter = b_root.get_mergeable_counter("revision").unwrap();
    b_counter.increment(100.0).unwrap();
    b.commit_then_renew();

    // A advances its clock past B's increment, then deletes.
    for i in 0..5 {
        a_root.insert(&format!("noise_{i}"), i).unwrap();
        a.commit_then_renew();
    }
    a_root.delete("revision").unwrap();
    a.commit_then_renew();

    sync(&a, &b);

    let va = a.get_deep_value().to_json_value();
    let vb = b.get_deep_value().to_json_value();
    assert_eq!(va, vb);

    // Counter not visible. Both peers agree.
    assert!(
        va["state"].get("revision").is_none(),
        "delete with higher IdLp must dominate; got {va}"
    );
}


/// Snapshot round-trip preserves the delete semantic. A peer creates and
/// deletes a mergeable counter, exports a snapshot; the receiving peer
/// imports and sees no counter in deep value — the tombstone was
/// recovered from the parent map's existing value table.
#[test]
#[cfg(feature = "counter")]
fn snapshot_roundtrip_preserves_delete_via_tombstone_recovery() {
    let a = doc(1);
    let a_root = a.get_map("state");
    let a_counter = a_root.get_mergeable_counter("revision").unwrap();
    a_counter.increment(5.0).unwrap();
    a_root.delete("revision").unwrap();
    a.commit_then_renew();
    assert_eq!(a.get_deep_value().to_json_value(), json!({ "state": {} }));

    let snapshot = a.export(ExportMode::Snapshot).unwrap();
    let b = doc(2);
    b.import(&snapshot).unwrap();

    assert_eq!(
        b.get_deep_value().to_json_value(),
        json!({ "state": {} }),
        "snapshot import must recover the tombstone via MapState.map's None entry"
    );

    let b_root = b.get_map("state");
    let tombstone = b_root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_tombstone(&"revision".into()))
        })
        .unwrap();
    assert!(
        tombstone.is_some(),
        "tombstone must be recovered on the receiving peer; got None"
    );
}


/// Fresh-receiver case: the snapshot has a tombstone for "k" but NO mergeable
/// cid under "k" — because the deleting peer never created the mergeable
/// child (peer C, who did create it concurrently, hasn't synced yet). When
/// C's update later arrives, the gate must reject the registration because
/// the tombstone was seeded from `MapValue { value: None }` even though no
/// mergeable cid existed at snapshot-import time.
///
/// If recovery only seeded tombstones for keys that already had a mergeable
/// cid, this test fails: B's gate sees no tombstone, allows the registration,
/// and B's deep value shows the counter that the snapshot already declared
/// dead.
#[test]
#[cfg(feature = "counter")]
fn snapshot_recovery_seeds_tombstone_even_without_existing_child() {
    let a = doc(1);
    let a_root = a.get_map("state");
    a_root.insert("k", 42).unwrap();
    a_root.delete("k").unwrap();
    a.commit_then_renew();

    let snapshot = a.export(ExportMode::Snapshot).unwrap();
    let b = doc(2);
    b.import(&snapshot).unwrap();

    let b_root = b.get_map("state");
    let tombstone_b = b_root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_tombstone(&"k".into()))
        })
        .unwrap();
    assert!(
        tombstone_b.is_some(),
        "tombstone for 'k' must be recovered even though no mergeable cid \
         exists under that key in the snapshot; got None"
    );

    let c = doc(3);
    let c_root = c.get_map("state");
    let _c_counter = c_root.get_mergeable_counter("k").unwrap();
    let c_counter = c.get_map("state").get_mergeable_counter("k").unwrap();
    c_counter.increment(7.0).unwrap();
    c.commit_then_renew();

    let c_updates = c
        .export(ExportMode::Updates {
            from: Default::default(),
        })
        .unwrap();
    b.import(&c_updates).unwrap();

    assert_eq!(
        b.get_deep_value().to_json_value(),
        json!({ "state": {} }),
        "tombstone seeded from MapValue None must gate the later remote \
         child registration; without the fix, B shows the resurrected counter"
    );
}


/// The op stream emitted by `delete` on a mergeable key contains exactly
/// one op (a regular MapSet tombstone) by the local peer. No new op
/// types are introduced. Older peers receiving this stream apply it as
/// a regular tombstone (no-op against the value table because they
/// never had a value entry for the mergeable child).
#[test]
#[cfg(feature = "counter")]
fn delete_on_mergeable_key_emits_only_existing_op_types() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(1.0).unwrap();
    doc.commit_then_renew();
    let counter_before = doc.oplog_vv().get(&1).copied().unwrap_or(0);

    root.delete("revision").unwrap();
    doc.commit_then_renew();
    let counter_after = doc.oplog_vv().get(&1).copied().unwrap_or(0);

    let new_ops = counter_after - counter_before;
    assert_eq!(
        new_ops, 1,
        "delete must emit exactly one op (the MapSet tombstone); got {new_ops}"
    );
}

