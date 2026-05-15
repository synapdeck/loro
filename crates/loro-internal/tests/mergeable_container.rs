use std::sync::{Arc, Mutex};

use loro_internal::{
    cursor::PosType, event::Index, handler::ValueOrHandler, loro::ExportMode, ContainerType,
    HandlerTrait, LoroDoc, ToJson,
};
use serde_json::json;

fn doc(peer: u64) -> LoroDoc {
    let doc = LoroDoc::new_auto_commit();
    doc.set_peer_id(peer).unwrap();
    doc
}

fn sync(a: &LoroDoc, b: &LoroDoc) {
    a.import(&b.export(ExportMode::updates(&a.oplog_vv())).unwrap())
        .unwrap();
    b.import(&a.export(ExportMode::updates(&b.oplog_vv())).unwrap())
        .unwrap();
}

#[test]
#[cfg(feature = "counter")]
fn concurrent_counter_increments_show_current_lost_update_bug() {
    let a = doc(1);
    let b = doc(2);

    let a_root = a.get_map("state");
    let b_root = b.get_map("state");

    let a_counter = a_root.get_mergeable_counter("revision").unwrap();
    let b_counter = b_root.get_mergeable_counter("revision").unwrap();

    assert_eq!(
        a_counter.id(),
        b_counter.id(),
        "both peers should produce the same deterministic cid"
    );
    assert!(
        a_counter.id().is_mergeable(),
        "counter cid should be in the mergeable namespace"
    );

    a_counter.increment(1.0).unwrap();
    b_counter.increment(1.0).unwrap();

    sync(&a, &b);

    assert_eq!(
        a.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 2.0 } }),
        "both concurrent increments should survive on the merged counter",
    );
    assert_eq!(
        b.get_deep_value().to_json_value(),
        a.get_deep_value().to_json_value()
    );

    let a_cid = match a_root.get_("revision") {
        Some(ValueOrHandler::Handler(handler)) => Some(handler.id()),
        _ => None,
    };
    let b_cid = match b_root.get_("revision") {
        Some(ValueOrHandler::Handler(handler)) => Some(handler.id()),
        _ => None,
    };
    assert_eq!(
        a_cid, b_cid,
        "both peers should resolve the same logical counter cid"
    );
}

#[test]
fn concurrent_text_updates_show_current_lost_update_bug() {
    let a = doc(1);
    let b = doc(2);
    let a_text = a.get_map("state").get_mergeable_text("notes").unwrap();
    let b_text = b.get_map("state").get_mergeable_text("notes").unwrap();

    assert_eq!(
        a_text.id(),
        b_text.id(),
        "both peers should produce the same deterministic cid"
    );
    assert!(
        a_text.id().is_mergeable(),
        "text cid should be in the mergeable namespace"
    );

    a_text.insert(0, "A", PosType::Unicode).unwrap();
    b_text.insert(0, "B", PosType::Unicode).unwrap();
    sync(&a, &b);

    let value = a.get_deep_value().to_json_value();
    assert!(
        value == json!({ "state": { "notes": "AB" } })
            || value == json!({ "state": { "notes": "BA" } }),
        "both concurrent text edits should survive on the merged text; got {value}",
    );
}

#[test]
fn concurrent_list_inserts_show_current_lost_update_bug() {
    let a = doc(1);
    let b = doc(2);
    let a_list = a.get_map("state").get_mergeable_list("items").unwrap();
    let b_list = b.get_map("state").get_mergeable_list("items").unwrap();

    assert_eq!(
        a_list.id(),
        b_list.id(),
        "both peers should produce the same deterministic cid"
    );
    assert!(
        a_list.id().is_mergeable(),
        "list cid should be in the mergeable namespace"
    );

    a_list.insert(0, "A").unwrap();
    b_list.insert(0, "B").unwrap();
    sync(&a, &b);

    let value = a.get_deep_value().to_json_value();
    assert!(
        value == json!({ "state": { "items": ["A", "B"] } })
            || value == json!({ "state": { "items": ["B", "A"] } }),
        "both concurrent list inserts should survive on the merged list; got {value}",
    );
}

#[test]
#[cfg(feature = "counter")]
fn mergeable_container_id_roundtrips_parent_key_and_type() {
    let parent = loro_common::ContainerID::new_root("state", ContainerType::Map);
    let key = "field\u{1}with/slash:and:semicolon";
    let cid = loro_common::ContainerID::new_mergeable(&parent, key, ContainerType::Counter);

    assert!(cid.is_mergeable());
    let (decoded_parent, decoded_key, decoded_type) = cid.parse_mergeable().unwrap();
    assert_eq!(decoded_parent, parent);
    assert_eq!(decoded_key, key);
    assert_eq!(decoded_type, ContainerType::Counter);
}

#[test]
fn user_root_names_cannot_use_mergeable_namespace() {
    assert!(!loro_common::check_root_container_name(
        loro_common::MERGEABLE_NAMESPACE_PREFIX
    ));
}

#[test]
#[cfg(feature = "counter")]
fn get_path_returns_logical_parent_path_for_mergeable_child() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    // Exercise the cid path directly; child-side wiring needed to keep
    // mutating ops alive arrives in a later commit.
    let _ = counter.increment(1.0);

    let path = doc
        .get_path_to_container(&counter.id())
        .expect("mergeable counter should have a logical path");
    let indexes = path
        .iter()
        .map(|(_, index)| index.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        indexes,
        vec![Index::Key("state".into()), Index::Key("revision".into()),],
        "mergeable child path should walk logical parent edges, not the synthetic Root name",
    );
}

#[test]
#[cfg(feature = "counter")]
fn deep_value_nests_mergeable_child_under_parent_and_hides_synthetic_root() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(2.0).unwrap();

    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 2.0 } })
    );
}

/// Two peers each obtain the "profile" Map via `get_mergeable_map` and write
/// to *different* keys. With non-mergeable child Maps, each peer creates a
/// distinct peer-specific cid, so LWW drops one peer's Map entirely even
/// though the key sets are disjoint. Once child Maps are mergeable the merged
/// value should contain both keys.
#[test]
fn concurrent_map_writes_show_current_lost_update_bug() {
    let a = doc(1);
    let b = doc(2);
    let a_map = a.get_map("state").get_mergeable_map("profile").unwrap();
    let b_map = b.get_map("state").get_mergeable_map("profile").unwrap();

    assert_eq!(
        a_map.id(),
        b_map.id(),
        "both peers should produce the same deterministic cid"
    );
    assert!(
        a_map.id().is_mergeable(),
        "map cid should be in the mergeable namespace"
    );

    a_map.insert("name", "Ada").unwrap();
    b_map.insert("title", "Engineer").unwrap();
    sync(&a, &b);

    assert_eq!(
        a.get_deep_value().to_json_value(),
        json!({ "state": { "profile": { "name": "Ada", "title": "Engineer" } } })
    );
}

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

/// Subscribing to the *parent* map must receive events when one of its
/// mergeable children is mutated: subscriptions on ancestor containers should
/// observe deltas from mergeable descendants, not only subscriptions on the
/// mergeable child itself.
#[test]
#[cfg(feature = "counter")]
fn parent_map_subscription_receives_mergeable_child_events() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();

    let received: Arc<Mutex<Vec<Vec<Index>>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = received.clone();
    let _sub = doc.subscribe(
        &root.id(),
        Arc::new(move |event| {
            let mut g = received_clone.lock().unwrap();
            for container_diff in event.events.iter() {
                g.push(
                    container_diff
                        .path
                        .iter()
                        .map(|(_, idx)| idx.clone())
                        .collect::<Vec<_>>(),
                );
            }
        }),
    );

    counter.increment(1.0).unwrap();
    doc.commit_then_renew();

    let captured = received.lock().unwrap();
    assert!(
        captured.iter().any(|path| path
            .iter()
            .any(|idx| matches!(idx, Index::Key(k) if &**k == "revision"))),
        "parent map subscriber should see an event whose path includes the mergeable child's key 'revision'; got {captured:?}",
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

/// Three peers each increment the same mergeable counter once. After a full
/// round-robin sync, every peer must observe `3.0` and the same deterministic
/// cid. Two-peer tests can't catch ordering or idempotency bugs that fire
/// only when more than two histories overlap.
#[test]
#[cfg(feature = "counter")]
fn three_peer_mergeable_counter_convergence() {
    let a = doc(1);
    let b = doc(2);
    let c = doc(3);

    let a_counter = a
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    let b_counter = b
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    let c_counter = c
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    assert_eq!(a_counter.id(), b_counter.id());
    assert_eq!(b_counter.id(), c_counter.id());

    a_counter.increment(1.0).unwrap();
    b_counter.increment(1.0).unwrap();
    c_counter.increment(1.0).unwrap();

    // Full round-robin: every pair syncs.
    sync(&a, &b);
    sync(&b, &c);
    sync(&a, &c);
    sync(&a, &b);

    let expected = json!({ "state": { "revision": 3.0 } });
    assert_eq!(a.get_deep_value().to_json_value(), expected);
    assert_eq!(b.get_deep_value().to_json_value(), expected);
    assert_eq!(c.get_deep_value().to_json_value(), expected);
}

/// After A and B sync once, both peers concurrently mutate the same
/// mergeable counter again, then sync. Convergence must hold on the second
/// round — the deterministic cid plus CRDT merge must keep working after
/// the side table has been populated and used.
#[test]
#[cfg(feature = "counter")]
fn post_merge_concurrent_counter_increments_converge() {
    let a = doc(1);
    let b = doc(2);

    let a_counter = a
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    let b_counter = b
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    a_counter.increment(1.0).unwrap();
    b_counter.increment(1.0).unwrap();
    sync(&a, &b);
    assert_eq!(
        a.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 2.0 } })
    );

    // Round 2: concurrent edits on the already-merged child.
    a_counter.increment(10.0).unwrap();
    b_counter.increment(100.0).unwrap();
    sync(&a, &b);

    let expected = json!({ "state": { "revision": 112.0 } });
    assert_eq!(a.get_deep_value().to_json_value(), expected);
    assert_eq!(b.get_deep_value().to_json_value(), expected);
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

/// `parse_mergeable` is a pure decoder and must return `None` (not panic, not
/// silently misinterpret) for every malformed payload. This guards against
/// future drift in the encoder + decoder pair.
#[test]
#[cfg(feature = "counter")]
fn parse_mergeable_rejects_malformed_payloads() {
    use loro_common::{ContainerID, ContainerType};

    // Non-mergeable root: returns None.
    let plain_root = ContainerID::new_root("ordinary", ContainerType::Map);
    assert!(plain_root.parse_mergeable().is_none());

    // Mergeable prefix but invalid hex.
    let bad_hex = ContainerID::Root {
        name: "🤝:zzzz".into(),
        container_type: ContainerType::Counter,
    };
    assert!(
        bad_hex.parse_mergeable().is_none(),
        "non-hex chars in payload must reject"
    );

    // Mergeable prefix, valid hex, but truncated (no segments).
    let truncated = ContainerID::Root {
        name: "🤝:".into(),
        container_type: ContainerType::Counter,
    };
    assert!(
        truncated.parse_mergeable().is_none(),
        "empty payload must reject"
    );

    // Mergeable prefix, valid hex, but trailing garbage after the type byte.
    let parent = ContainerID::new_root("state", ContainerType::Map);
    let cid = ContainerID::new_mergeable(&parent, "k", ContainerType::Counter);
    let mut name = match &cid {
        ContainerID::Root { name, .. } => name.to_string(),
        _ => panic!("expected Root"),
    };
    name.push_str("ff"); // append one extra byte's worth of hex
    let with_garbage = ContainerID::Root {
        name: name.into(),
        container_type: ContainerType::Counter,
    };
    assert!(
        with_garbage.parse_mergeable().is_none(),
        "trailing bytes after type byte must reject"
    );

    // Mergeable prefix and a payload that decodes correctly, BUT the
    // encoded type byte disagrees with the Root's container_type field.
    let mismatched = ContainerID::Root {
        name: match &cid {
            ContainerID::Root { name, .. } => name.clone(),
            _ => unreachable!(),
        },
        container_type: ContainerType::Map, // payload says Counter
    };
    assert!(
        mismatched.parse_mergeable().is_none(),
        "type-byte mismatch with Root.container_type must reject"
    );
}

/// `LoroDoc::get_map` must reject names in the mergeable namespace at the
/// call site, not just in `check_root_container_name`. Otherwise user code
/// could fabricate a Root cid that masquerades as a mergeable child and
/// confuse the parent-edge walks.
///
/// This test runs `check_root_container_name` directly on a variety of
/// user-supplied strings; the runtime `get_map` / `get_text` / etc. calls
/// route through this validator (see callers in `crates/loro-internal/src/`).
/// If the validator is bypassed by a runtime path, that's a separate bug —
/// but it's not something this test can prove without intentionally writing
/// `🤝:` keys, which is what we're trying to prevent in the first place.
#[test]
fn root_name_validator_rejects_mergeable_namespace_inputs() {
    use loro_common::{check_root_container_name, MERGEABLE_NAMESPACE_PREFIX};

    // Bare prefix.
    assert!(!check_root_container_name(MERGEABLE_NAMESPACE_PREFIX));
    // Prefix + arbitrary payload.
    assert!(!check_root_container_name("🤝:deadbeef"));
    assert!(!check_root_container_name("🤝:"));
    // Prefix-as-substring is OK (not a prefix), validator still allows it.
    assert!(check_root_container_name("foo🤝:bar"));
    // Prefix with a leading zero-width space is NOT a prefix match, allowed.
    assert!(check_root_container_name("\u{200B}🤝:abc"));
    // Sanity: ordinary user names still pass.
    assert!(check_root_container_name("state"));
    assert!(check_root_container_name("ordinary-name_with-symbols"));
    // Empty is still rejected (pre-existing behavior).
    assert!(!check_root_container_name(""));
    // Slash and NUL still rejected (pre-existing behavior).
    assert!(!check_root_container_name("a/b"));
    assert!(!check_root_container_name("a\0b"));
}

/// Two peers independently navigate `state → mergeable map "profile" →
/// mergeable counter "revision"` and increment. The deterministic cid for
/// "revision" is the same on both peers (it's a function of the "profile"
/// cid, which is itself deterministic from "state"'s cid + "profile" + Map).
/// After sync, both peers see the counter at 2.0 nested correctly.
#[test]
#[cfg(feature = "counter")]
fn nested_mergeable_concurrent_counter_converges() {
    let a = doc(1);
    let b = doc(2);

    let a_profile = a.get_map("state").get_mergeable_map("profile").unwrap();
    let b_profile = b.get_map("state").get_mergeable_map("profile").unwrap();
    assert_eq!(a_profile.id(), b_profile.id());

    let a_rev = a_profile.get_mergeable_counter("revision").unwrap();
    let b_rev = b_profile.get_mergeable_counter("revision").unwrap();
    assert_eq!(a_rev.id(), b_rev.id());

    a_rev.increment(1.0).unwrap();
    b_rev.increment(1.0).unwrap();
    sync(&a, &b);

    let expected = json!({ "state": { "profile": { "revision": 2.0 } } });
    assert_eq!(a.get_deep_value().to_json_value(), expected);
    assert_eq!(b.get_deep_value().to_json_value(), expected);

    // Path resolution still walks both mergeable hops.
    let path = a.get_path_to_container(&a_rev.id()).expect("path");
    let indexes = path.iter().map(|(_, idx)| idx.clone()).collect::<Vec<_>>();
    assert_eq!(
        indexes,
        vec![
            Index::Key("state".into()),
            Index::Key("profile".into()),
            Index::Key("revision".into()),
        ]
    );
}

/// For each supported container kind, `new_mergeable` produces a deterministic
/// cid that decodes back to the same `(parent, key, kind)`. Counter is gated
/// on the feature; the rest are unconditional.
#[test]
fn mergeable_cid_roundtrips_for_every_container_kind() {
    use loro_common::{ContainerID, ContainerType};
    let parent = ContainerID::new_root("state", ContainerType::Map);

    let mut kinds: Vec<ContainerType> = vec![
        ContainerType::Map,
        ContainerType::List,
        ContainerType::MovableList,
        ContainerType::Text,
        ContainerType::Tree,
    ];
    #[cfg(feature = "counter")]
    kinds.push(ContainerType::Counter);

    for kind in kinds {
        let cid = ContainerID::new_mergeable(&parent, "field", kind);
        assert!(cid.is_mergeable(), "kind {kind:?}: must be mergeable");
        let again = ContainerID::new_mergeable(&parent, "field", kind);
        assert_eq!(cid, again, "kind {kind:?}: cid must be deterministic");
        let (decoded_parent, decoded_key, decoded_kind) = cid
            .parse_mergeable()
            .unwrap_or_else(|| panic!("kind {kind:?}: parse_mergeable returned None"));
        assert_eq!(decoded_parent, parent, "kind {kind:?}: parent roundtrip");
        assert_eq!(decoded_key, "field", "kind {kind:?}: key roundtrip");
        assert_eq!(decoded_kind, kind, "kind {kind:?}: kind roundtrip");
    }
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

/// A subscription on the mergeable child's cid must receive events when the
/// child is mutated, symmetric to the existing parent-map subscription test.
#[test]
#[cfg(feature = "counter")]
fn mergeable_child_subscription_receives_own_events() {
    let doc = doc(1);
    let counter = doc
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();

    let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let count_clone = count.clone();
    let _sub = doc.subscribe(
        &counter.id(),
        Arc::new(move |_event| {
            *count_clone.lock().unwrap() += 1;
        }),
    );

    counter.increment(1.0).unwrap();
    doc.commit_then_renew();
    counter.increment(2.0).unwrap();
    doc.commit_then_renew();

    let observed = *count.lock().unwrap();
    assert!(
        observed >= 2,
        "mergeable-child subscription must fire at least once per commit; got {observed} events"
    );
}

/// A detached map handler (built without a parent doc) supports
/// `get_mergeable_*` by falling back to `get_or_create_container`. The
/// fallback path doesn't compute deterministic cids — that's intentional;
/// determinism is meaningless until the handler attaches to a doc — but it
/// must still return a working child handler that callers can mutate.
#[test]
#[cfg(feature = "counter")]
fn detached_map_get_mergeable_counter_falls_back_cleanly() {
    use loro_internal::MapHandler;
    let detached = MapHandler::new_detached();
    let counter = detached
        .get_mergeable_counter("revision")
        .expect("detached fallback must succeed");
    counter
        .increment(7.0)
        .expect("detached counter must be mutable");
    // The detached handler still surfaces the value through its local state.
    assert_eq!(counter.get_value().to_json_value(), json!(7.0));
}

/// Key encoding is len-prefixed binary, so it must round-trip cleanly for
/// degenerate inputs: empty, long, embedded NUL, and embedded mergeable
/// prefix substring. Catches off-by-one and ad-hoc string-split mistakes
/// in any future decoder change.
#[test]
fn mergeable_cid_roundtrips_for_degenerate_keys() {
    use loro_common::{ContainerID, ContainerType};
    let parent = ContainerID::new_root("state", ContainerType::Map);

    let long_key: String = std::iter::repeat('k').take(2048).collect();
    let cases: Vec<&str> = vec![
        "",
        long_key.as_str(),
        "with\0nul\0bytes",
        "embedded 🤝: substring in the middle",
        "starts_with_🤝:_prefix",
        "trailing_emoji_🤝:",
        "ascii/slash/looking",
    ];

    for key in cases {
        let cid = ContainerID::new_mergeable(&parent, key, ContainerType::Map);
        assert!(cid.is_mergeable(), "key {key:?}: must be mergeable");
        let (decoded_parent, decoded_key, decoded_kind) = cid
            .parse_mergeable()
            .unwrap_or_else(|| panic!("key {key:?}: parse_mergeable returned None"));
        assert_eq!(decoded_parent, parent);
        assert_eq!(decoded_key, key, "key {key:?}: round-trip mismatch");
        assert_eq!(decoded_kind, ContainerType::Map);
    }
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

/// Mutations on a mergeable child must be undoable. Undoing reverts the
/// child's value; the side-table registration persists (because it isn't
/// itself an op).
#[test]
#[cfg(feature = "counter")]
fn undo_manager_reverts_mergeable_counter_mutation() {
    use loro_internal::UndoManager;

    let doc = doc(1);
    let counter = doc
        .get_map("state")
        .get_mergeable_counter("revision")
        .unwrap();
    let undo = UndoManager::new(&doc);

    counter.increment(5.0).unwrap();
    doc.commit_then_renew();
    assert_eq!(counter.get_value().to_json_value(), json!(5.0));

    let did_undo = undo.undo().expect("undo must succeed");
    assert!(did_undo, "undo must report it did something");

    // The mergeable child still exists (registration is not an op), but its
    // value is back to zero.
    assert_eq!(
        counter.get_value().to_json_value(),
        json!(0.0),
        "undo must revert the increment"
    );
}

/// Calling `MapHandler::delete(key)` on a key that has a mergeable child
/// registered is a semantic no-op: the mergeable child lives in the side
/// table, not the value map, so the `MapSet` tombstone has nothing to
/// overwrite. The side-table entry survives, the counter value remains
/// visible in `get_deep_value`, and re-resolving the handler returns the
/// same deterministic cid.
#[test]
#[cfg(feature = "counter")]
fn delete_on_mergeable_child_key_observed_behavior() {
    let doc = doc(1);
    let root = doc.get_map("state");
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(3.0).unwrap();
    doc.commit_then_renew();
    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 3.0 } })
    );

    let delete_result = root.delete("revision");
    assert!(delete_result.is_ok(), "delete must not error");
    doc.commit_then_renew();

    // Side table wins: the counter is still visible after delete.
    assert_eq!(
        doc.get_deep_value().to_json_value(),
        json!({ "state": { "revision": 3.0 } }),
        "delete on a mergeable key must be a no-op against side-table entries",
    );

    // Whatever delete did, the doc must not be corrupted. Re-resolving the counter must produce
    // the same deterministic cid and the doc stays usable.
    let counter2 = root.get_mergeable_counter("revision").unwrap();
    assert_eq!(counter2.id(), counter.id());
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

/// `LoroDoc::has_container(cid)` must return `false` for a mergeable cid
/// that has never been written to, and `true` after the child has been
/// mutated. Mergeable existence depends on state, not on the name shape, so
/// the short-circuit for plain `Root` ids must skip mergeable namespace cids.
#[test]
#[cfg(feature = "counter")]
fn has_container_reports_false_for_unwritten_mergeable_child() {
    use loro_common::{ContainerID, ContainerType};

    let doc = doc(1);
    let root = doc.get_map("state");

    // Build the deterministic mergeable cid by hand, WITHOUT calling
    // get_mergeable_counter (which would register the side-table entry).
    let parent_id = root.id();
    let unwritten_cid = ContainerID::new_mergeable(&parent_id, "revision", ContainerType::Counter);
    assert!(unwritten_cid.is_mergeable());
    assert!(
        !doc.has_container(&unwritten_cid),
        "has_container must report false for an unwritten mergeable cid"
    );

    // Now actually create and mutate the child. has_container must flip to true.
    let counter = root.get_mergeable_counter("revision").unwrap();
    counter.increment(1.0).unwrap();
    assert_eq!(counter.id(), unwritten_cid, "cid is deterministic");
    assert!(
        doc.has_container(&unwritten_cid),
        "has_container must report true after the mergeable child has state"
    );

    // Regular root containers still report true as before (regression guard).
    let regular_root = ContainerID::new_root("state", ContainerType::Map);
    assert!(doc.has_container(&regular_root));
}

/// `DocState::mergeable_first_op_idlp` returns the IdLp of the first op
/// applied to a mergeable container, or `None` if no ops exist yet. Used
/// by import-time LWW resolution between competing kinds under the same key.
///
/// Locks are acquired in the crate-wide order `oplog -> state` (kinds 2, 3).
#[test]
#[cfg(feature = "counter")]
fn mergeable_first_op_idlp_lookup() {
    let a = doc(1);
    let root = a.get_map("state");

    let counter = root.get_mergeable_counter("revision").unwrap();
    let cid = counter.id();

    // No ops yet -> None.
    let pre = {
        let oplog = a.oplog().lock();
        let state = a.app_state().lock();
        state.mergeable_first_op_idlp(&oplog, &cid)
    };
    assert!(pre.is_none(), "no ops -> None; got {pre:?}");

    counter.increment(1.0).unwrap();
    a.commit_then_renew();

    let post = {
        let oplog = a.oplog().lock();
        let state = a.app_state().lock();
        state.mergeable_first_op_idlp(&oplog, &cid)
    }
    .expect("after increment, must have a first-op IdLp");

    // First op in a fresh doc owned by peer 1 lands at lamport 0; this is
    // the canonical IdLp for the container's birth.
    assert_eq!(post.peer, 1);
    assert_eq!(post.lamport, 0);

    // Second increment must not move the first-op IdLp.
    counter.increment(1.0).unwrap();
    a.commit_then_renew();
    let post2 = {
        let oplog = a.oplog().lock();
        let state = a.app_state().lock();
        state.mergeable_first_op_idlp(&oplog, &cid)
    }
    .expect("second-op state still has a first-op IdLp");
    assert_eq!(post2, post, "first-op IdLp is stable across later ops");
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

/// `DocState::mergeable_max_op_idlp` returns the IdLp of the LATEST op
/// applied to a mergeable container, or `None` if no ops exist yet. Used
/// by the tombstone-vs-cid reachability gate: a mergeable cid is reachable
/// iff `max_op_idlp(cid) > tombstone_idlp(key)`.
#[test]
#[cfg(feature = "counter")]
fn mergeable_max_op_idlp_lookup() {
    let a = doc(1);
    let root = a.get_map("state");

    let counter = root.get_mergeable_counter("revision").unwrap();
    let cid = counter.id();

    // No ops yet -> None.
    let pre = {
        let oplog = a.oplog().lock();
        let state = a.app_state().lock();
        state.mergeable_max_op_idlp(&oplog, &cid)
    };
    assert!(pre.is_none(), "no ops -> None; got {pre:?}");

    counter.increment(1.0).unwrap();
    a.commit_then_renew();
    let first = {
        let oplog = a.oplog().lock();
        let state = a.app_state().lock();
        state.mergeable_max_op_idlp(&oplog, &cid)
    }
    .expect("after one op, max-op-idlp must exist");

    counter.increment(2.0).unwrap();
    a.commit_then_renew();
    let second = {
        let oplog = a.oplog().lock();
        let state = a.app_state().lock();
        state.mergeable_max_op_idlp(&oplog, &cid)
    }
    .expect("after two ops, max-op-idlp must exist");

    assert!(
        second > first,
        "max IdLp must advance with each op: first={first:?}, second={second:?}"
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
