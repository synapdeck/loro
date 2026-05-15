//! Snapshot and update-import round-trips, including LWW recovery.

#[path = "common.rs"]
mod common;
use common::{doc, sync};

use loro_internal::{cursor::PosType, event::Index, loro::ExportMode, HandlerTrait, ToJson};
use serde_json::json;

/// Snapshot round-trip must preserve the parent edges (logical path) and
/// state values for mergeable child containers nested inside other mergeable
/// child containers.
///
/// Source peer creates `state` → mergeable map `profile` → mergeable counter
/// `revision`, mutates each, then exports a snapshot. Importing into a fresh
/// peer must reproduce the same deep value and the same logical path for
/// the counter.
#[test]
#[cfg(feature = "counter")]
fn snapshot_roundtrip_preserves_mergeable_parent_edges_and_values() {
    let source = doc(1);
    let root = source.get_map("state");
    let nested = root.get_mergeable_map("profile").unwrap();
    nested.insert("name", "Ada").unwrap();
    let counter = nested.get_mergeable_counter("revision").unwrap();
    counter.increment(3.0).unwrap();

    let snapshot = source.export(ExportMode::Snapshot).unwrap();
    let imported = doc(2);
    imported.import(&snapshot).unwrap();

    assert_eq!(
        imported.get_deep_value().to_json_value(),
        source.get_deep_value().to_json_value(),
        "deep value of the imported doc must match the source after snapshot round-trip"
    );

    let imported_counter = imported
        .get_map("state")
        .get_mergeable_map("profile")
        .unwrap()
        .get_mergeable_counter("revision")
        .unwrap();
    let path = imported
        .get_path_to_container(&imported_counter.id())
        .expect("mergeable counter must have a logical path after snapshot import");
    let indexes = path
        .iter()
        .map(|(_, index)| index.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        indexes,
        vec![
            Index::Key("state".into()),
            Index::Key("profile".into()),
            Index::Key("revision".into()),
        ],
        "imported counter should walk logical parent edges across two mergeable hops"
    );
}


/// Peer B imports updates that originated from peer A's `get_mergeable_counter`
/// + `increment` calls, but peer B never locally called `get_mergeable_*`.
/// After import, peer B's deep value, container enumeration, and path
/// resolution for the mergeable child must all reflect the imported state.
#[test]
#[cfg(feature = "counter")]
fn update_import_populates_mergeable_side_table_on_receiver() {
    let a = doc(1);
    let b = doc(2);

    let a_counter = a
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    a_counter.increment(5.0).unwrap();
    a.commit_then_renew();

    // Peer B imports A's updates WITHOUT first calling get_mergeable_counter.
    let updates = a.export(ExportMode::updates(&b.oplog_vv())).unwrap();
    b.import(&updates).unwrap();

    assert_eq!(
        b.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 5.0 } }),
        "after update import, peer B's deep value must include the mergeable child"
    );

    // Peer B then locally resolves the mergeable handler — this must return
    // the same cid as the one peer A wrote, and the existing value.
    let b_counter = b
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    assert_eq!(b_counter.id(), a_counter.id());
    assert_eq!(b_counter.get_value().to_json_value(), json!(5.0));

    // Path resolution from peer B's side must walk through the parent map.
    let path = b.get_path_to_container(&b_counter.id()).expect("path");
    let indexes = path.iter().map(|(_, idx)| idx.clone()).collect::<Vec<_>>();
    assert_eq!(
        indexes,
        vec![Index::Key("state".into()), Index::Key("revision".into())]
    );
}


/// Create a mergeable counter but never mutate it, then export a snapshot.
/// An unmutated mergeable child has no KV-backed state to anchor the recovery
/// walk, so it does not round-trip through a snapshot. The receiving peer
/// sees an empty parent map and can re-create the same deterministic cid by
/// calling `get_mergeable_*` locally.
#[test]
#[cfg(feature = "counter")]
fn empty_mergeable_child_after_snapshot_import() {
    let a = doc(1);
    let _counter = a
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    // Deliberately no increment. Commit anyway so any pending state is flushed.
    a.commit_then_renew();

    let snapshot = a.export(ExportMode::Snapshot).unwrap();
    let b = doc(2);
    b.import(&snapshot).unwrap();

    // An unmutated mergeable child does NOT round-trip through a snapshot because it has no
    // KV-backed state to anchor the recovery walk. Peer B's deep value sees an empty `state`
    // map; re-invoking `get_mergeable_counter` on B re-creates the same cid deterministically.
    assert_eq!(
        b.get_deep_value().to_json_value(),
        json!({ "state": {} }),
        "unmutated mergeable child must not appear in deep value after snapshot import",
    );

    let b_counter = b
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    assert_eq!(b_counter.id(), _counter.id(), "cid still deterministic");
    assert_eq!(b_counter.get_value().to_json_value(), json!(0.0));
}


/// Snapshot import where both peers registered the same `(key, kind)`:
/// deterministic cids match, recovery walk converges, content from peer A
/// wins through normal CRDT merge.
#[test]
fn snapshot_import_same_type_collision_converges() {
    let a = doc(1);
    let a_text = a.get_map("state").get_mergeable_text("notes").unwrap();
    a_text.insert(0, "A", PosType::Unicode).unwrap();
    a.commit_then_renew();
    let snapshot = a.export(ExportMode::Snapshot).unwrap();

    let b = doc(2);
    let b_text = b.get_map("state").get_mergeable_text("notes").unwrap();
    b_text.insert(0, "B", PosType::Unicode).unwrap();
    b.commit_then_renew();
    assert_eq!(a_text.id(), b_text.id(), "cids must match before import");

    b.import(&snapshot).unwrap();

    // Sync back so A sees both.
    sync(&a, &b);
    let value = a.get_deep_value().to_json_value();
    assert!(
        value == json!({ "state": { "notes": "AB" } })
            || value == json!({ "state": { "notes": "BA" } }),
        "both edits must survive on same-type collision; got {value}"
    );
    assert_eq!(b.get_deep_value().to_json_value(), value);
}


/// Snapshot import where the LOCAL peer has registered a different kind for
/// the same key than the SNAPSHOT peer. The deterministic cids differ (kind
/// is part of the cid hash), so the recovery walk produces two distinct
/// mergeable child cids under the same key in the parent's side table.
///
/// Both cids coexist; the parent's deep value surfaces only one (the side-
/// table iterator order). User code that mixes types under the same key has
/// bigger problems — this test documents the observable behavior so that any
/// future tightening (e.g. promoting it to an error) is a deliberate change.
#[test]
fn snapshot_import_different_type_collision_is_observable() {
    let a = doc(1);
    let a_text = a.get_map("state").get_mergeable_text("k").unwrap();
    a_text.insert(0, "hello", PosType::Unicode).unwrap();
    a.commit_then_renew();
    let snapshot = a.export(ExportMode::Snapshot).unwrap();

    let b = doc(2);
    let b_map = b.get_map("state").get_mergeable_map("k").unwrap();
    b_map.insert("flag", true).unwrap();
    b.commit_then_renew();
    assert_ne!(
        a_text.id(),
        b_map.id(),
        "different kinds under the same key MUST produce different cids"
    );

    let result = b.import(&snapshot);
    assert!(
        result.is_ok(),
        "import itself must not fail; got {result:?}"
    );

    // After LWW at import, exactly one kind survives; the other errors with kind-mismatch.
    let text_result = b.get_map("state").get_mergeable_text("k");
    let map_result = b.get_map("state").get_mergeable_map("k");
    let surviving = match (&text_result, &map_result) {
        (Ok(_), Err(_)) => "Text",
        (Err(_), Ok(_)) => "Map",
        other => panic!("expected exactly one kind to survive, got {other:?}"),
    };
    println!("Different-type LWW resolution: {surviving} won");
}


/// After snapshot import registers `("k", Text)` on the receiver via the
/// recovery walk, a local `get_mergeable_map("k")` on the receiver must fail
/// with type-mismatch — exactly as it would if the text was registered
/// locally first.
#[test]
fn type_mismatch_rejected_after_snapshot_registers_the_key() {
    let a = doc(1);
    let a_text = a.get_map("state").get_mergeable_text("k").unwrap();
    a_text.insert(0, "x", PosType::Unicode).unwrap();
    a.commit_then_renew();
    let snapshot = a.export(ExportMode::Snapshot).unwrap();

    let b = doc(2);
    b.import(&snapshot).unwrap();

    // Mergeable child for "k" is now registered on B as Text via the
    // recovery walk. Asking for a Map under "k" must error.
    let err = b.get_map("state").get_mergeable_map("k").unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Mergeable key") && msg.contains("Map") && msg.contains("Text"),
        "expected ArgErr after snapshot-registered mergeable child blocks a different kind; got {msg}"
    );

    // Asking for Text under "k" still works and resolves the same cid.
    let b_text = b.get_map("state").get_mergeable_text("k").unwrap();
    assert_eq!(b_text.id(), a_text.id());
}


/// Shallow snapshot export should preserve mergeable child state and parent
/// edges on the receiver, the same as a full snapshot.
#[test]
#[cfg(feature = "counter")]
fn shallow_snapshot_roundtrip_preserves_mergeable_child() {
    let a = doc(1);
    let counter = a
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    counter.increment(4.0).unwrap();
    a.commit_then_renew();

    // ShallowSnapshot at current frontiers.
    let frontiers = a.state_frontiers();
    let snapshot = a
        .export(ExportMode::ShallowSnapshot(std::borrow::Cow::Owned(
            frontiers,
        )))
        .unwrap();

    let b = doc(2);
    b.import(&snapshot).unwrap();
    assert_eq!(
        b.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 4.0 } }),
        "shallow snapshot must carry mergeable child state and side-table reconstruction"
    );
}


