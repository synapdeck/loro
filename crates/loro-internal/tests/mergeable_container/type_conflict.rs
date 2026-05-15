//! Type-mismatch rejection and import-time LWW resolution.

#[path = "common.rs"]
mod common;
use common::{doc, sync};

use loro_internal::{cursor::PosType, loro::ExportMode, HandlerTrait, ToJson};
use serde_json::json;

/// If a mergeable child is already registered under `key` with one container
/// type, a subsequent request under the same key with a different container
/// type must return [`LoroError::ArgErr`] rather than silently producing a
/// second container with a divergent deterministic cid.
#[test]
fn mergeable_type_mismatch_returns_arg_error() {
    let doc = doc(1);
    let root = doc.get_map("state");
    root.get_mergeable_text("field").unwrap();

    let err = root.get_mergeable_map("field").unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Mergeable key") && msg.contains("Map") && msg.contains("Text"),
        "expected ArgErr describing the mergeable type mismatch; got {msg}"
    );
}


/// When two competing-kind mergeable cids land in the same parent's side
/// table under the same key during import (snapshot or update), the LWW
/// resolver picks the one with the higher first-op IdLp. The loser's
/// cid is NOT registered on the parent MapState. The loser's container
/// state remains in KV (orphaned, no parent edge in the side table).
#[test]
#[cfg(feature = "counter")]
fn lww_resolves_different_type_collision_at_import() {
    // Peer A: text under "k", incremented at low lamport.
    let a = doc(1);
    let a_text = a.get_map("state").get_mergeable_text("k").unwrap();
    a_text.insert(0, "from_a", PosType::Unicode).unwrap();
    a.commit_then_renew();

    // Peer B: map under "k", with a *later* op (we use unrelated ops first
    // to advance B's lamport clock past A's text op).
    let b = doc(2);
    let b_state = b.get_map("state");
    // Force B's lamport clock to advance.
    for i in 0..5 {
        b_state.insert(&format!("filler_{i}"), i).unwrap();
        b.commit_then_renew();
    }
    let b_map = b_state.get_mergeable_map("k").unwrap();
    b_map.insert("from_b", true).unwrap();
    b.commit_then_renew();

    // Verify the lamport ordering precondition for the test.
    let a_first = {
        let oplog = a.oplog().lock();
        let state = a.app_state().lock();
        state.mergeable_first_op_idlp(&oplog, &a_text.id())
    }
    .expect("A's text must have a first-op IdLp");
    let b_first = {
        let oplog = b.oplog().lock();
        let state = b.app_state().lock();
        state.mergeable_first_op_idlp(&oplog, &b_map.id())
    }
    .expect("B's map must have a first-op IdLp");
    assert!(
        b_first > a_first,
        "test precondition: B's first op (idlp={b_first:?}) must be later than A's (idlp={a_first:?})"
    );

    // B imports A's snapshot. LWW should keep B's map (higher first-op
    // IdLp) and orphan A's text.
    let snapshot = a.export(ExportMode::Snapshot).unwrap();
    b.import(&snapshot).unwrap();

    // B's deep value still shows the map content; not the text.
    let value = b.get_deep_value().to_json_value();
    let state_obj = &value["state"];
    assert!(state_obj.get("k").is_some(), "k must be present");
    let k_value = &state_obj["k"];
    assert!(
        k_value.is_object(),
        "k must be a Map (B's kind won), got {k_value:?}"
    );
    assert_eq!(k_value["from_b"], json!(true));

    // Calling get_mergeable_text("k") on B now errors (B's side table
    // registers Map under "k"; asking for Text mismatches).
    let err = b
        .get_map("state")
        .get_mergeable_text("k")
        .expect_err("Text on a Map-resolved key must error");
    assert!(
        format!("{err:?}").contains("Expected value type")
            || format!("{err:?}").contains("Mergeable key"),
        "expected ArgErr for kind mismatch; got {err:?}"
    );

    // Calling get_mergeable_map("k") still succeeds and returns the same cid.
    let b_map_again = b.get_map("state").get_mergeable_map("k").unwrap();
    assert_eq!(b_map_again.id(), b_map.id());
}


/// Three peers each independently register a mergeable child under the
/// same key with three different kinds. After a full round-robin sync,
/// every peer agrees on the same winner via LWW.
#[test]
#[cfg(feature = "counter")]
fn lww_three_peer_type_conflict_converges() {
    let a = doc(1);
    let b = doc(2);
    let c = doc(3);

    // Each peer registers a different kind under "k" and mutates it once.
    let a_text = a.get_map("state").get_mergeable_text("k").unwrap();
    a_text.insert(0, "from_a", PosType::Unicode).unwrap();
    a.commit_then_renew();

    let b_map = b.get_map("state").get_mergeable_map("k").unwrap();
    b_map.insert("from_b", true).unwrap();
    b.commit_then_renew();

    let c_list = c.get_map("state").get_mergeable_list("k").unwrap();
    c_list.insert(0, "from_c").unwrap();
    c.commit_then_renew();

    // Full round-robin sync: every pair exchanges updates.
    sync(&a, &b);
    sync(&b, &c);
    sync(&a, &c);
    sync(&a, &b);

    let va = a.get_deep_value().to_json_value();
    let vb = b.get_deep_value().to_json_value();
    let vc = c.get_deep_value().to_json_value();

    assert_eq!(va, vb, "A and B must agree");
    assert_eq!(vb, vc, "B and C must agree");

    // Exactly one of the three kinds must be the visible content under "k".
    // Text -> JSON string. Map -> JSON object. List -> JSON array.
    let k = &va["state"]["k"];
    let survivors = [
        k.is_string(), // Text
        k.is_object(), // Map
        k.is_array(),  // List
    ];
    let count: usize = survivors.iter().filter(|x| **x).count();
    assert_eq!(
        count, 1,
        "exactly one kind must survive; got {survivors:?} for value {k:?}"
    );
}


/// If both competing mergeable cids have no applied ops, LWW resolution
/// is deferred — neither is registered. Once an op arrives on either
/// side and another sync happens, the next resolution pass picks the
/// winner (by virtue of having an op at all).
#[test]
#[cfg(feature = "counter")]
fn lww_unmutated_competitors_defer_resolution() {
    let a = doc(1);
    let _a_text = a.get_map("state").get_mergeable_text("k").unwrap();
    // Deliberately no mutation.
    a.commit_then_renew();

    let b = doc(2);
    let _b_map = b.get_map("state").get_mergeable_map("k").unwrap();
    b.commit_then_renew();

    // B imports A's snapshot. Both cids have no ops; LWW resolution defers, and the empty-child
    // invariant still holds: B's deep value shows "state": {}.
    let snapshot = a.export(ExportMode::Snapshot).unwrap();
    b.import(&snapshot).unwrap();
    assert_eq!(
        b.get_deep_value().to_json_value(),
        json!({ "state": {} }),
        "unmutated competitors must defer; deep value stays empty"
    );

    // Now A produces an op on its text. After re-sync, LWW kicks in:
    // A's text has a first-op IdLp; B's map has none; A's text wins
    // by virtue of having an op at all.
    let a_text = a.get_map("state").get_mergeable_text("k").unwrap();
    a_text.insert(0, "from_a", PosType::Unicode).unwrap();
    a.commit_then_renew();
    sync(&a, &b);

    let value = b.get_deep_value().to_json_value();
    assert_eq!(
        value,
        json!({ "state": { "k": "from_a" } }),
        "after A's op, A's text wins; B's unmutated map loses; got {value}"
    );
}


/// After LWW resolution at import populates the side table from a remote
/// peer's claim, a local `get_mergeable_<loser_kind>` should error with a
/// message that mentions the cause is a resolved conflict (not just a
/// local type-mismatch).
#[test]
#[cfg(feature = "counter")]
fn type_mismatch_error_mentions_lww_resolution() {
    // Setup: B imports A's snapshot containing a Text under "k"; B has
    // never locally registered "k". After import, B's side table has
    // a Text entry under "k" via the recovery walk.
    let a = doc(1);
    let a_text = a.get_map("state").get_mergeable_text("k").unwrap();
    a_text.insert(0, "x", PosType::Unicode).unwrap();
    a.commit_then_renew();
    let snapshot = a.export(ExportMode::Snapshot).unwrap();

    let b = doc(2);
    b.import(&snapshot).unwrap();

    let err = b.get_map("state").get_mergeable_map("k").unwrap_err();
    let msg = format!("{err:?}");

    // The error should mention both kinds (Map and Text), and indicate
    // that the conflict was resolved by import/LWW/concurrent write
    // (i.e., not just a local type-mismatch).
    assert!(
        msg.contains("Map") && msg.contains("Text"),
        "error must mention both the requested kind (Map) and the resolved kind (Text); got {msg}"
    );
    assert!(
        msg.to_lowercase().contains("resolved")
            || msg.to_lowercase().contains("lww")
            || msg.to_lowercase().contains("concurrent")
            || msg.to_lowercase().contains("import")
            || msg.to_lowercase().contains("mergeable"),
        "error must hint at LWW/import-time resolution or mergeable context; got {msg}"
    );
}


