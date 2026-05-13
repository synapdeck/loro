use loro_internal::{
    cursor::PosType, handler::ValueOrHandler, loro::ExportMode, ContainerType, HandlerTrait,
    LoroDoc, ToJson,
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
