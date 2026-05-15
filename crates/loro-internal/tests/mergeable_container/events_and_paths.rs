//! Path resolution, deep value, subscriptions, undo, and has_container.

#[path = "common.rs"]
mod common;
use common::doc;

use std::sync::{Arc, Mutex};

use loro_internal::{event::Index, HandlerTrait, ToJson};
use serde_json::json;

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


