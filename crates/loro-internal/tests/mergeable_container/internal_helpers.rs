//! Smoke tests for DocState helpers (op-IdLp extremum lookups).

#[path = "common.rs"]
mod common;
use common::doc;

use loro_internal::{HandlerTrait, IdLp};

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


/// `MapState::set_mergeable_tombstone` is monotonic-max: a later (higher) IdLp
/// replaces an earlier one, but an earlier (lower) IdLp never overwrites a later
/// one. This matches the LWW semantic for deletes across concurrent peers and is
/// relied on by the snapshot recovery + apply_local_op + apply_diff hooks that
/// all funnel through this helper.
#[test]
fn set_mergeable_tombstone_is_monotonic_max() {
    let doc = doc(1);
    let root = doc.get_map("state");

    let key = "k";
    let earlier = IdLp::new(1, 10);
    let later = IdLp::new(1, 20);

    // Apply the earlier tombstone first.
    root.with_state(|state| {
        state
            .as_map_state_mut()
            .unwrap()
            .set_mergeable_tombstone(key.into(), earlier);
        Ok(())
    })
    .unwrap();
    let stored = root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_tombstone(&key.into()))
        })
        .unwrap()
        .expect("tombstone must be set after first call");
    assert_eq!(stored, earlier);

    // Apply the LATER tombstone: must replace.
    root.with_state(|state| {
        state
            .as_map_state_mut()
            .unwrap()
            .set_mergeable_tombstone(key.into(), later);
        Ok(())
    })
    .unwrap();
    let stored = root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_tombstone(&key.into()))
        })
        .unwrap()
        .expect("tombstone must still be set after monotonic-max replacement");
    assert_eq!(
        stored, later,
        "a later tombstone must replace an earlier one"
    );

    // Apply the EARLIER tombstone again: must NOT regress.
    root.with_state(|state| {
        state
            .as_map_state_mut()
            .unwrap()
            .set_mergeable_tombstone(key.into(), earlier);
        Ok(())
    })
    .unwrap();
    let stored = root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_tombstone(&key.into()))
        })
        .unwrap()
        .expect("tombstone must still be set");
    assert_eq!(
        stored, later,
        "an earlier tombstone must NOT overwrite a later one (monotonic-max)"
    );

    // Re-applying the SAME (later) idlp must be a no-op.
    root.with_state(|state| {
        state
            .as_map_state_mut()
            .unwrap()
            .set_mergeable_tombstone(key.into(), later);
        Ok(())
    })
    .unwrap();
    let stored = root
        .with_state(|state| {
            Ok(state
                .as_map_state()
                .unwrap()
                .mergeable_tombstone(&key.into()))
        })
        .unwrap()
        .expect("tombstone must still be set");
    assert_eq!(stored, later, "re-applying the same idlp must be a no-op");
}


