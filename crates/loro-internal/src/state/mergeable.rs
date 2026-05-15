//! Mergeable-container bookkeeping on `DocState`.
//!
//! Mergeable child containers (created via `MapHandler::get_mergeable_*`) live as deterministic
//! `ContainerID::Root` ids in a reserved namespace, but their parent-edge information is kept in
//! a side table on the parent [`MapState`] rather than encoded in the op stream. This module
//! groups the [`DocState`] methods that maintain that side table during snapshot import, update
//! import, and op-time bookkeeping.
//!
//! See [`MapState`]'s `mergeable_*` accessors for the side-table primitives, and
//! [`ContainerID::new_mergeable`] / [`ContainerID::parse_mergeable`] for the encoding.
//!
//! [`MapState`]: super::map_state::MapState

use loro_common::{ContainerID, ContainerType, IdLp, InternalString};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{container::idx::ContainerIdx, OpLog};

use super::DocState;

impl DocState {
    /// Walk all known containers and recover mergeable side tables that are not serialized in
    /// snapshots.
    ///
    /// Mergeable children are stored in KV like any other container (they have their own
    /// state), so they survive snapshot round-trip at the container level. But the parent
    /// MapState side table that the deep-value walk and path resolution consult is intentionally
    /// not serialized: encoding it would require extending the on-wire format, and the data is
    /// fully recoverable from the deterministic cid alone via [`ContainerID::parse_mergeable`].
    ///
    /// Called from [`Self::init_with_states_and_version`] right after a snapshot decode so the
    /// imported doc's deep value, path lookups, and reachability checks see mergeable children
    /// without requiring the caller to re-invoke `get_mergeable_*` on every nested key.
    pub(super) fn repopulate_mergeable_child_side_tables(&mut self, oplog: &OpLog) {
        // Pass 1: collect ALL map cids in the store. Iterating all map states (not just parents
        // of current mergeable children) is required for the fresh-receiver case where the
        // snapshot has a tombstone for a key that this peer has never seen a mergeable child
        // under. A later remote diff carrying a child-creation op from a third peer must be
        // gated by that tombstone; if we only seeded for parents-of-current-mergeable-cids,
        // the gate would be silently empty and the child would register.
        let map_cids: Vec<ContainerID> = self
            .store
            .iter_all_container_ids()
            .filter(|id| matches!(id.container_type(), ContainerType::Map))
            .collect();

        // Pass 2: for each map cid, walk its value table and collect every
        // `MapValue { value: None }` entry as a `(parent_cid, key, idlp)` tombstone-seed
        // candidate. Materialize before mutating to avoid holding an iterator borrow across the
        // seeding loop.
        let mut tombstones_to_seed: Vec<(ContainerID, InternalString, IdLp)> = Vec::new();
        for parent_id in &map_cids {
            let Some(parent_idx) = self.arena.id_to_idx(parent_id) else {
                continue;
            };
            let Some(state) = self.store.get_container(parent_idx) else {
                continue;
            };
            let Some(map_state) = state.as_map_state() else {
                continue;
            };
            for (key, mv) in map_state.iter() {
                if mv.value.is_none() {
                    tombstones_to_seed.push((parent_id.clone(), key.clone(), mv.idlp()));
                }
            }
        }

        // Pass 3: write the seeds. `set_mergeable_tombstone` is monotonic-max, so duplicates
        // and out-of-order entries are handled correctly even if a single (parent, key) had
        // multiple `None`-valued history entries.
        for (parent_id, key, idlp) in tombstones_to_seed {
            let Some(parent_idx) = self.arena.id_to_idx(&parent_id) else {
                continue;
            };
            if let Some(state) = self.store.get_container_mut(parent_idx) {
                if let Some(map_state) = state.as_map_state_mut() {
                    map_state.set_mergeable_tombstone(key, idlp);
                }
            }
        }

        // Pass 4: register mergeable cids. `register_mergeable_children` consults the just-
        // seeded tombstones; cids whose max-op IdLp is dominated by a tombstone are filtered
        // out before LWW selection.
        let mergeable: Vec<ContainerID> = self
            .store
            .iter_all_container_ids()
            .filter(|id| id.is_mergeable())
            .collect();
        self.register_mergeable_children(oplog, mergeable);
    }

    /// Register each mergeable cid in `cids` under its parent MapState's `child_containers`
    /// side table. Shared body for both the snapshot recovery walk and the update-import path
    /// in `apply_diff`.
    ///
    /// Non-mergeable cids are silently ignored, so callers may pass a mixed iterator. The work
    /// is idempotent: calling it twice with the same cid is a no-op on the second call
    /// (`register_mergeable_child` inserts into a HashMap keyed by cid).
    pub(super) fn register_mergeable_children(
        &mut self,
        oplog: &OpLog,
        cids: impl IntoIterator<Item = ContainerID>,
    ) {
        // Pass 1: group cids by (parent_id, key). Each group represents a potential conflict
        // (one cid per kind under the same parent/key).
        let mut by_key: FxHashMap<(ContainerID, InternalString), Vec<ContainerID>> =
            FxHashMap::default();
        for cid in cids {
            if !cid.is_mergeable() {
                continue;
            }
            let Some((parent_id, key, _kind)) = cid.parse_mergeable() else {
                continue;
            };
            by_key.entry((parent_id, key.into())).or_default().push(cid);
        }

        // Augment each group with any mergeable cids ALREADY registered under the same
        // (parent, key) on the parent MapState's side table. This is critical for the update-
        // import path: the incoming diff batch only carries newly-arrived cids, but a local
        // pre-existing cid of a competing kind must still be considered as a candidate so the
        // LWW resolver makes a correct, full-information decision.
        for ((parent_id, key), candidates) in by_key.iter_mut() {
            let Some(parent_idx) = self.arena.id_to_idx(parent_id) else {
                continue;
            };
            let Some(state) = self.store.get_container_mut(parent_idx) else {
                continue;
            };
            let Some(map) = state.as_map_state_mut() else {
                continue;
            };
            for existing in map.mergeable_child_ids_for_key(key) {
                if !candidates.iter().any(|c| c == &existing) {
                    candidates.push(existing);
                }
            }
        }

        // Pass 2: for each group, look up first-op IdLp (the LWW winner key) and max-op IdLp
        // (the tombstone reachability gate) per cid, then pick the LWW winner among tombstone-
        // reachable candidates. Cids with no ops yet defer — they aren't registered; their
        // conflict, if any, is re-resolved once ops arrive. This preserves the invariant that
        // an unmutated mergeable child does not round-trip through a snapshot.
        let mut decisions: Vec<(ContainerID, InternalString, ContainerID)> = Vec::new();
        for ((parent_id, key), candidates) in by_key {
            let tombstone: Option<IdLp> = self.arena.id_to_idx(&parent_id).and_then(|idx| {
                self.store
                    .get_container_mut(idx)
                    .and_then(|state| state.as_map_state())
                    .and_then(|map_state| map_state.mergeable_tombstone(&key))
            });

            // Decorate each candidate with its first-op IdLp (used for LWW winner selection)
            // and max-op IdLp (used for the tombstone reachability gate). Drop candidates with
            // no ops yet — they can't beat any tombstone, and LWW also needs a first-op to
            // compare. Deferred candidates re-resolve once ops arrive.
            let mut decorated: Vec<(ContainerID, IdLp, IdLp)> = candidates
                .into_iter()
                .filter_map(|cid| {
                    let first = self.mergeable_first_op_idlp(oplog, &cid)?;
                    let max = self.mergeable_max_op_idlp(oplog, &cid)?;
                    Some((cid, first, max))
                })
                .collect();

            // Filter by tombstone reachability BEFORE LWW selection. Filtering after LWW would
            // leave a dominated winner while discarding reachable losers; filtering first
            // picks among the surviving reachable candidates.
            if let Some(t) = tombstone {
                decorated.retain(|(_, _, max_idlp)| *max_idlp > t);
            }
            if decorated.is_empty() {
                continue;
            }

            // LWW: pick the candidate with the largest first-op IdLp (the most-recent
            // established claim).
            decorated.sort_by_key(|(_, first, _)| *first);
            let (winner, _, _) = decorated.pop().unwrap();
            decisions.push((parent_id, key, winner));
        }

        // Pass 3: register the winners (mutates `self.store` and `self.arena`).
        for (parent_id, key, winner) in decisions {
            // Ensure the parent container exists in the store; it normally does (the mergeable
            // child was created by writing to it), but `ensure_container` is idempotent and
            // cheap when present.
            self.store.ensure_container(&parent_id);
            let parent_idx = self.arena.register_container(&parent_id);
            // Also make sure the arena parent edge is wired. Re-registering the mergeable cid
            // is idempotent and will set the parent via `parse_mergeable` if it wasn't
            // already set.
            self.arena.register_container(&winner);

            if let Some(state) = self.store.get_container_mut(parent_idx) {
                if let Some(map) = state.as_map_state_mut() {
                    // Use the LWW-aware variant: any competing-kind mergeable cid previously
                    // registered under this key is evicted so exactly one survives per
                    // (parent, key).
                    map.replace_mergeable_child_for_key(key, winner);
                }
            }
        }
    }

    /// Walk selected map states' tombstones and evict any side-table cid whose
    /// `max_op_idlp <= tombstone`. This is deliberately per-cid, not per-key: when multiple
    /// mergeable cids temporarily share a key with mixed reachability, a bulk key eviction
    /// would incorrectly remove reachable children.
    ///
    /// Called from `apply_diff`'s post-loop to handle remote parent deletes that arrive
    /// without the child cid in the diff batch. Lock-order: caller acquires `oplog` before the
    /// state lock and passes `&OpLog` in.
    pub(crate) fn reconcile_mergeable_tombstones_for(
        &mut self,
        oplog: &OpLog,
        map_parent_idxs: &FxHashSet<ContainerIdx>,
    ) {
        let mut eviction_decisions: Vec<(ContainerIdx, ContainerID)> = Vec::new();

        for parent_idx in map_parent_idxs {
            let tombstones: Vec<(InternalString, IdLp)> = self
                .store
                .get_container(*parent_idx)
                .and_then(|state| state.as_map_state())
                .map(|map_state| {
                    map_state
                        .iter_mergeable_tombstones()
                        .map(|(key, tombstone)| (key.clone(), *tombstone))
                        .collect()
                })
                .unwrap_or_default();

            for (key, tombstone) in tombstones {
                let registered_cids = self
                    .store
                    .get_container(*parent_idx)
                    .and_then(|state| state.as_map_state())
                    .map(|map_state| map_state.mergeable_child_ids_for_key(&key))
                    .unwrap_or_default();

                for cid in registered_cids {
                    let dominated = match self.mergeable_max_op_idlp(oplog, &cid) {
                        Some(max_idlp) => max_idlp <= tombstone,
                        None => true,
                    };
                    if dominated {
                        eviction_decisions.push((*parent_idx, cid));
                    }
                }
            }
        }

        let did_evict = !eviction_decisions.is_empty();
        for (parent_idx, cid) in eviction_decisions {
            if let Some(state) = self.store.get_container_mut(parent_idx) {
                if let Some(map_state) = state.as_map_state_mut() {
                    map_state.evict_mergeable_child_cid(&cid);
                }
            }
        }

        if did_evict {
            self.dead_containers_cache.clear_alive();
        }
    }

    /// Walk every map state in the doc and reconcile its mergeable tombstones. Convenience
    /// wrapper around [`Self::reconcile_mergeable_tombstones_for`] for callers that don't have
    /// a precomputed parent-idx set; production code uses the per-batch variant from the
    /// `apply_diff` post-loop and only this wrapper is exposed for tests that exercise the
    /// reconciliation pass directly.
    #[doc(hidden)]
    pub fn reconcile_mergeable_tombstones(&mut self, oplog: &OpLog) {
        let candidates: Vec<ContainerID> = self.store.iter_all_container_ids().collect();
        let map_idxs: FxHashSet<ContainerIdx> = candidates
            .into_iter()
            .filter_map(|id| {
                let idx = self.arena.id_to_idx(&id)?;
                let state = self.store.get_container(idx)?;
                state.as_map_state().map(|_| idx)
            })
            .collect();
        self.reconcile_mergeable_tombstones_for(oplog, &map_idxs);
    }

    /// Return the IdLp of the first applied op against the given mergeable container, or
    /// `None` if no ops have been applied yet.
    ///
    /// "First" is determined by total IdLp order (lamport, peer): the op with the smallest
    /// IdLp among all ops on this container is the container's birth. Used by import-time LWW
    /// resolution between competing-kind mergeable cids under the same parent key.
    ///
    /// Returns `None` for non-mergeable cids, for unknown cids, or when the oplog has no ops
    /// targeting this container yet.
    ///
    /// The caller passes in an `&OpLog` rather than this method acquiring the oplog lock
    /// itself: the crate-wide order is `oplog -> state`, and callers already hold the state
    /// lock to reach `&self`.
    pub fn mergeable_first_op_idlp(&self, oplog: &OpLog, cid: &ContainerID) -> Option<IdLp> {
        self.mergeable_op_idlp_extremum(oplog, cid, OpIdlpExtremum::Min)
    }

    /// Return the IdLp of the LATEST applied op against the given mergeable container, or
    /// `None` if no ops have been applied yet.
    ///
    /// "Latest" is determined by total IdLp order (lamport, peer): the op with the largest
    /// IdLp among all ops on this container is the container's most-recent activity. Used by
    /// the tombstone-vs-cid reachability gate: a mergeable cid is reachable iff
    /// `max_op_idlp(cid) > tombstone_idlp(key)`.
    ///
    /// Returns `None` for non-mergeable cids, for mergeable cids that are not registered in
    /// the arena, and for mergeable cids registered in the arena but with no applied ops.
    ///
    /// Lock-order convention: caller acquires `oplog` before `state` and passes `&OpLog` in.
    /// See [`Self::mergeable_first_op_idlp`] for the parallel min-side helper.
    pub fn mergeable_max_op_idlp(&self, oplog: &OpLog, cid: &ContainerID) -> Option<IdLp> {
        self.mergeable_op_idlp_extremum(oplog, cid, OpIdlpExtremum::Max)
    }

    fn mergeable_op_idlp_extremum(
        &self,
        oplog: &OpLog,
        cid: &ContainerID,
        extremum: OpIdlpExtremum,
    ) -> Option<IdLp> {
        if !cid.is_mergeable() {
            return None;
        }
        let target_idx = self.arena.id_to_idx(cid)?;
        let mut best: Option<IdLp> = None;
        oplog.change_store().visit_all_changes(&mut |change| {
            let base_counter = change.id.counter;
            let base_lamport = change.lamport;
            let peer = change.id.peer;
            for op in change.ops.iter() {
                if op.container != target_idx {
                    continue;
                }
                let lamport = base_lamport + (op.counter - base_counter) as crate::change::Lamport;
                let idlp = IdLp::new(peer, lamport);
                best = Some(match best {
                    Some(cur) => match extremum {
                        OpIdlpExtremum::Min if cur <= idlp => cur,
                        OpIdlpExtremum::Max if cur >= idlp => cur,
                        _ => idlp,
                    },
                    None => idlp,
                });
            }
        });
        best
    }

    /// Collect `(key, cid)` pairs for mergeable children registered on the parent MapState's
    /// side table for the given container index.
    ///
    /// Returns an empty Vec if the container is not a Map or has no mergeable children. Used
    /// by both `get_container_deep_value` and `get_container_deep_value_with_id` to nest
    /// mergeable child values under their logical parent key during the deep-value walk.
    pub(super) fn collect_mergeable_children_of(
        &mut self,
        container: ContainerIdx,
    ) -> Vec<(InternalString, ContainerID)> {
        self.store
            .get_container_mut(container)
            .and_then(|state| state.as_map_state())
            .map(|map_state| {
                map_state
                    .iter_mergeable_children()
                    .map(|(key, id)| (key.clone(), id.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Copy, Clone)]
enum OpIdlpExtremum {
    Min,
    Max,
}
