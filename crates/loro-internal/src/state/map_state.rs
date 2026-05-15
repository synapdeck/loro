use std::{collections::BTreeMap, sync::Weak};

use loro_common::{ContainerID, IdLp, LoroResult, PeerID};
use rustc_hash::FxHashMap;

use crate::{
    configure::Configure,
    container::{idx::ContainerIdx, map::MapSet},
    delta::{MapValue, ResolvedMapDelta, ResolvedMapValue},
    diff_calc::DiffMode,
    event::{Diff, Index, InternalDiff},
    handler::ValueOrHandler,
    op::{Op, RawOp, RawOpContent},
    InternalString, LoroDocInner, LoroValue,
};

use super::{ApplyLocalOpReturn, ContainerState, DiffApplyContext};

#[derive(Debug, Clone)]
pub struct MapState {
    idx: ContainerIdx,
    map: BTreeMap<InternalString, MapValue>,
    child_containers: FxHashMap<ContainerID, InternalString>,
    /// Tombstones for keys whose mergeable child was deleted via
    /// `MapSet { key, value: None }`. Maps each deleted key to the IdLp of the deleting op.
    ///
    /// A mergeable cid registered under a key is reachable (in `child_containers`) iff there is
    /// no tombstone for that key OR the cid's `max_op_idlp > tombstone_idlp`. See
    /// [`DocState::mergeable_max_op_idlp`] and the registration gate in
    /// [`DocState::register_mergeable_children`].
    ///
    /// This field is not serialized as a distinct snapshot field; it is recomputed on snapshot
    /// import from the parent map's existing value table (`map` entries with
    /// `MapValue { value: None, .. }`).
    mergeable_tombstones: FxHashMap<InternalString, IdLp>,
    size: usize,
}

impl ContainerState for MapState {
    fn container_idx(&self) -> ContainerIdx {
        self.idx
    }

    fn is_state_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn apply_diff_and_convert(
        &mut self,
        diff: InternalDiff,
        DiffApplyContext { doc, mode }: DiffApplyContext,
    ) -> Diff {
        let InternalDiff::Map(delta) = diff else {
            unreachable!()
        };
        let doc = &doc.upgrade().unwrap();
        let force = matches!(mode, DiffMode::Checkout | DiffMode::Linear);
        let mut resolved_delta = ResolvedMapDelta::new();
        for (key, value) in delta.updated.into_iter() {
            let Some(value) = value else {
                // uncreate op
                assert_eq!(mode, DiffMode::Checkout);
                self.remove(&key);
                resolved_delta = resolved_delta.with_entry(key, ResolvedMapValue::new_unset());
                continue;
            };

            let mut changed = false;
            if force {
                self.insert(key.clone(), value.clone());
                changed = true;
            } else {
                match self.map.get(&key) {
                    Some(old_value) if old_value > &value => {}
                    _ => {
                        self.insert(key.clone(), value.clone());
                        changed = true;
                    }
                }
            }

            if changed {
                resolved_delta = resolved_delta.with_entry(
                    key,
                    ResolvedMapValue {
                        idlp: IdLp::new(value.peer, value.lamp),
                        value: value.value.map(|v| ValueOrHandler::from_value(v, doc)),
                    },
                )
            }
        }

        Diff::Map(resolved_delta)
    }

    fn apply_diff(&mut self, diff: InternalDiff, ctx: DiffApplyContext) -> LoroResult<()> {
        let _ = self.apply_diff_and_convert(diff, ctx);
        Ok(())
    }

    fn apply_local_op(&mut self, op: &RawOp, _: &Op) -> LoroResult<ApplyLocalOpReturn> {
        let mut ans: ApplyLocalOpReturn = Default::default();
        match &op.content {
            RawOpContent::Map(MapSet { key, value }) => {
                let prev = self.insert(
                    key.clone(),
                    MapValue {
                        lamp: op.lamport,
                        peer: op.id.peer,
                        value: value.clone(),
                    },
                );

                if let Some(MapValue {
                    value: Some(LoroValue::Container(c)),
                    ..
                }) = prev
                {
                    ans.deleted_containers.push(c);
                }
            }
            _ => unreachable!(),
        }

        Ok(ans)
    }

    #[doc = " Convert a state to a diff that when apply this diff on a empty state,"]
    #[doc = " the state will be the same as this state."]
    fn to_diff(&mut self, doc: &Weak<LoroDocInner>) -> Diff {
        Diff::Map(ResolvedMapDelta {
            updated: self
                .map
                .clone()
                .into_iter()
                .map(|(k, v)| (k, ResolvedMapValue::from_map_value(v, doc)))
                .collect::<FxHashMap<_, _>>(),
        })
    }

    fn get_value(&mut self) -> LoroValue {
        let ans = self.to_map();
        LoroValue::Map(ans.into())
    }

    fn get_child_index(&self, id: &ContainerID) -> Option<Index> {
        self.child_containers.get(id).map(|x| Index::Key(x.clone()))
    }

    fn contains_child(&self, id: &ContainerID) -> bool {
        self.child_containers.contains_key(id)
    }

    fn get_child_containers(&self) -> Vec<ContainerID> {
        let mut ans = Vec::new();
        for (_, value) in self.map.iter() {
            if let Some(LoroValue::Container(x)) = &value.value {
                ans.push(x.clone());
            }
        }
        // Include mergeable children registered through the side table — they
        // are not in `self.map` (no `MapSet` op encodes them) but they are
        // logical children of this map for reachability / parent-edge
        // wiring.
        for (id, _) in self.child_containers.iter() {
            if id.is_mergeable() {
                ans.push(id.clone());
            }
        }
        ans
    }

    fn fork(&self, _config: &Configure) -> Self {
        self.clone()
    }
}

impl MapState {
    pub fn new(idx: ContainerIdx) -> Self {
        Self {
            idx,
            map: Default::default(),
            child_containers: Default::default(),
            mergeable_tombstones: Default::default(),
            size: 0,
        }
    }

    pub fn insert(&mut self, key: InternalString, value: MapValue) -> Option<MapValue> {
        let value_yes = value.value.is_some();
        if let Some(LoroValue::Container(id)) = &value.value {
            self.child_containers.insert(id.clone(), key.clone());
        }

        let result = self.map.insert(key.clone(), value);
        if let Some(Some(LoroValue::Container(c))) = result.as_ref().map(|x| &x.value) {
            self.child_containers.remove(c);
        }

        match (&result, value_yes) {
            (Some(x), true) => {
                if x.value.is_none() {
                    self.size += 1;
                }
            }
            (None, true) => {
                self.size += 1;
            }
            (Some(x), false) => {
                if x.value.is_some() {
                    self.size -= 1;
                }
            }
            _ => {}
        };

        result
    }

    pub fn remove(&mut self, key: &InternalString) {
        let result = self.map.remove(key);
        if let Some(x) = result {
            if x.value.is_some() {
                self.size -= 1;
            }
            if let Some(LoroValue::Container(id)) = x.value {
                self.child_containers.remove(&id);
            }
        };
    }

    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, InternalString, MapValue> {
        self.map.iter()
    }

    fn to_map(&self) -> FxHashMap<String, LoroValue> {
        let mut ans = FxHashMap::with_capacity_and_hasher(self.len(), Default::default());
        for (key, value) in self.map.iter() {
            if value.value.is_none() {
                continue;
            }

            ans.insert(key.to_string(), value.value.as_ref().cloned().unwrap());
        }

        ans
    }

    pub fn get(&self, k: &str) -> Option<&LoroValue> {
        match self.map.get(&k.into()) {
            Some(value) => match &value.value {
                Some(v) => Some(v),
                None => None,
            },
            None => None,
        }
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn get_last_edit_peer(&self, key: &str) -> Option<PeerID> {
        self.map.get(&key.into()).map(|v| v.peer)
    }

    /// Register a mergeable child container under `key` without emitting a
    /// `MapSet(key, Container(cid))` op on the parent.
    ///
    /// Mergeable children have deterministic [`ContainerID::Root`] ids derived
    /// from `(parent_id, key, kind)` (see [`ContainerID::new_mergeable`]).
    /// Recording them through the normal `MapSet` op stream would put
    /// `LoroValue::Container(cid)` into the parent's value slot for `key`,
    /// which would resurrect the LWW lost-update bug we are trying to avoid:
    /// concurrent first-touches by different peers would each write a
    /// `MapSet`, one would win, and the other peer's container would be
    /// orphaned.
    ///
    /// Instead, this side-table registration only populates `child_containers`
    /// so the mergeable cid is reachable for parent-edge walks (deep value,
    /// path resolution, reachability, deletion checks, child enumeration)
    /// while leaving `self.map` — and therefore the encoded op stream and the
    /// fast snapshot value — untouched.
    pub(crate) fn register_mergeable_child(&mut self, key: InternalString, id: ContainerID) {
        debug_assert!(
            id.is_mergeable(),
            "register_mergeable_child must only be called with mergeable container ids"
        );
        self.child_containers.insert(id, key);
    }

    /// LWW-resolver-friendly variant of [`Self::register_mergeable_child`].
    /// Removes any previously-registered mergeable cids under the same `key`
    /// (competing-kind losers) and inserts `id` as the sole survivor.
    ///
    /// Non-mergeable child entries under the same key (if any) are left
    /// untouched: those are normal containers tracked through `MapSet`, not
    /// through the mergeable side table.
    pub(crate) fn replace_mergeable_child_for_key(&mut self, key: InternalString, id: ContainerID) {
        debug_assert!(
            id.is_mergeable(),
            "replace_mergeable_child_for_key must only be called with mergeable container ids"
        );
        self.child_containers
            .retain(|cid, k| !(cid.is_mergeable() && *k == key));
        self.child_containers.insert(id, key);
    }

    /// Iterate `(key, cid)` pairs for mergeable children registered on this
    /// map via [`Self::register_mergeable_child`]. Used by the deep-value walk
    /// to nest mergeable child containers under their logical parent key.
    pub(crate) fn iter_mergeable_children(
        &self,
    ) -> impl Iterator<Item = (&InternalString, &ContainerID)> {
        self.child_containers
            .iter()
            .filter(|(id, _)| id.is_mergeable())
            .map(|(id, key)| (key, id))
    }

    /// Return every mergeable cid currently registered under `key`. Normally
    /// at most one survives after the import-time LWW resolver runs; multiple
    /// can be transiently present mid-resolution.
    pub(crate) fn mergeable_child_ids_for_key(&self, key: &InternalString) -> Vec<ContainerID> {
        self.child_containers
            .iter()
            .filter(|(id, k)| id.is_mergeable() && *k == key)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Return the cid of the mergeable child currently registered under `key`,
    /// if any. Used by `get_mergeable_*` to detect type-mismatch requests on
    /// the same key (e.g. `get_mergeable_text("k")` then
    /// `get_mergeable_map("k")`) before they produce divergent containers.
    pub(crate) fn get_mergeable_child_id(&self, key: &InternalString) -> Option<&ContainerID> {
        self.child_containers
            .iter()
            .find(|(id, k)| id.is_mergeable() && *k == key)
            .map(|(id, _)| id)
    }

    /// Return the tombstone IdLp for the given key, if any.
    pub(crate) fn mergeable_tombstone(&self, key: &InternalString) -> Option<IdLp> {
        self.mergeable_tombstones.get(key).copied()
    }

    /// Set or update the tombstone for `key`. Monotonic: a later tombstone
    /// (higher IdLp) replaces an earlier one; an earlier tombstone never
    /// overwrites a later one. This matches the LWW semantic for deletes
    /// across concurrent peers.
    pub(crate) fn set_mergeable_tombstone(&mut self, key: InternalString, idlp: IdLp) {
        let entry = self.mergeable_tombstones.entry(key).or_insert(idlp);
        if idlp > *entry {
            *entry = idlp;
        }
    }

    /// Iterate all `(key, idlp)` tombstone pairs. Used by the post-loop reconciliation hook to
    /// walk tombstones recorded during the current import batch.
    pub(crate) fn iter_mergeable_tombstones(
        &self,
    ) -> impl Iterator<Item = (&InternalString, &IdLp)> {
        self.mergeable_tombstones.iter()
    }

    /// Evict a specific mergeable cid from the side table. Returns true if the cid was
    /// registered and removed; false if it wasn't present.
    ///
    /// The caller decides WHICH cids to evict based on tombstone domination. Eviction is per-cid
    /// rather than per-key because mixed reachable/dominated states under the same key require
    /// keeping the reachable cids while dropping the dominated ones.
    pub(crate) fn evict_mergeable_child_cid(&mut self, cid: &ContainerID) -> bool {
        self.child_containers.remove(cid).is_some()
    }
}

mod snapshot {

    use loro_common::{InternalString, LoroValue};
    use rustc_hash::{FxHashMap, FxHashSet};
    use serde_columnar::Itertools;

    use crate::{
        delta::MapValue,
        encoding::value_register::ValueRegister,
        state::{ContainerCreationContext, ContainerState, FastStateSnapshot},
    };

    use super::MapState;

    impl FastStateSnapshot for MapState {
        fn encode_snapshot_fast<W: std::io::prelude::Write>(&mut self, mut w: W) {
            // 1. LoroValue
            // 2. Vec<String> keys_with_none_value
            // 3. leb128 peer_num + peers (in u64)
            // 3. Groups of (leb128 peer_idx, leb128 lamport), each has a respective map entry
            //    from either 1 or 2 when they all sorted by the key strings
            let value = self.get_value().into_map().unwrap();
            postcard::to_io(&*value, &mut w).unwrap();

            let keys_with_none_value = self
                .map
                .iter()
                .filter_map(|(k, v)| if v.value.is_some() { None } else { Some(k) })
                .collect_vec();
            postcard::to_io(&keys_with_none_value, &mut w).unwrap();
            let mut peer_register = ValueRegister::new();
            for v in self.map.values() {
                peer_register.register(&v.peer);
            }

            leb128::write::unsigned(&mut w, peer_register.vec().len() as u64).unwrap();
            for p in peer_register.vec() {
                w.write_all(&p.to_le_bytes()).unwrap();
            }
            let mut keys: Vec<&InternalString> = self.map.keys().collect();
            keys.sort_unstable();
            for key in keys.into_iter() {
                let value = self.map.get(key).unwrap();
                let peer_idx = peer_register.register(&value.peer);
                leb128::write::unsigned(&mut w, peer_idx as u64).unwrap();
                leb128::write::unsigned(&mut w, value.lamp as u64).unwrap();
            }
        }

        fn decode_value(bytes: &[u8]) -> loro_common::LoroResult<(loro_common::LoroValue, &[u8])> {
            let (value, bytes) = postcard::take_from_bytes::<FxHashMap<String, LoroValue>>(bytes)
                .map_err(|_| {
                loro_common::LoroError::DecodeError(
                    "Decode map value failed".to_string().into_boxed_str(),
                )
            })?;
            Ok((LoroValue::Map(value.into()), bytes))
        }

        fn decode_snapshot_fast(
            idx: crate::container::idx::ContainerIdx,
            (value, bytes): (loro_common::LoroValue, &[u8]),
            _ctx: ContainerCreationContext,
        ) -> loro_common::LoroResult<Self>
        where
            Self: Sized,
        {
            let value = value.into_map().unwrap();
            // keys_with_none_value
            let (keys_with_none_value, mut bytes) =
                postcard::take_from_bytes::<Vec<InternalString>>(bytes).map_err(|_| {
                    loro_common::LoroError::DecodeError(
                        "Decode map keys_with_none_value failed"
                            .to_string()
                            .into_boxed_str(),
                    )
                })?;
            let keys_with_none_value: FxHashSet<_> = keys_with_none_value.into_iter().collect();

            // peers
            let peer_count = leb128::read::unsigned(&mut bytes).unwrap() as usize;
            let mut peers = Vec::with_capacity(peer_count);
            for _ in 0..peer_count {
                let peer = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                bytes = &bytes[8..];
                peers.push(peer);
            }

            //
            let mut ans = MapState::new(idx);
            let mut keys: Vec<_> = value.keys().map(|x| x.as_str().into()).collect();
            keys.extend(keys_with_none_value.iter().cloned());
            keys.sort_unstable();

            for key in keys {
                let peer_idx = leb128::read::unsigned(&mut bytes).unwrap() as usize;
                let lamp = leb128::read::unsigned(&mut bytes).unwrap() as u32;
                let peer = peers[peer_idx];

                if keys_with_none_value.contains(&key) {
                    ans.insert(
                        key,
                        MapValue {
                            value: None,
                            lamp,
                            peer,
                        },
                    );
                } else {
                    let value = value.get(&*key).unwrap();
                    ans.insert(
                        key,
                        MapValue {
                            value: Some(value.clone()),
                            lamp,
                            peer,
                        },
                    );
                }
            }

            Ok(ans)
        }
    }

    #[cfg(test)]
    mod map_snapshot_test {
        use loro_common::LoroValue;

        use crate::container::idx::ContainerIdx;

        use super::*;

        #[test]
        fn map_fast_snapshot() {
            let mut map = MapState::new(ContainerIdx::from_index_and_type(
                0,
                loro_common::ContainerType::Map,
            ));
            map.insert(
                "1".into(),
                MapValue {
                    value: None,
                    lamp: 1,
                    peer: 1,
                },
            );
            map.insert(
                "2".into(),
                MapValue {
                    value: Some(LoroValue::I64(0)),
                    lamp: 2,
                    peer: 2,
                },
            );
            map.insert(
                "3".into(),
                MapValue {
                    value: Some(LoroValue::Double(1.0)),
                    lamp: 3,
                    peer: 3,
                },
            );

            let mut bytes = Vec::new();
            map.encode_snapshot_fast(&mut bytes);
            assert!(bytes.len() <= 50);

            let (value, bytes) = MapState::decode_value(&bytes).unwrap();
            {
                let m = value.clone().into_map().unwrap();
                assert_eq!(m.len(), 2);
                assert_eq!(m.get("2").unwrap(), &LoroValue::I64(0));
                assert_eq!(m.get("3").unwrap(), &LoroValue::Double(1.0));
            }

            let new_map = MapState::decode_snapshot_fast(
                ContainerIdx::from_index_and_type(0, loro_common::ContainerType::Map),
                (value, bytes),
                ContainerCreationContext {
                    configure: &Default::default(),
                    peer: 0,
                },
            )
            .unwrap();
            let v = new_map.map.get(&"2".into()).unwrap();
            assert_eq!(
                v,
                &MapValue {
                    value: Some(LoroValue::I64(0)),
                    lamp: 2,
                    peer: 2,
                }
            );
            let v = new_map.map.get(&"1".into()).unwrap();
            assert_eq!(
                v,
                &MapValue {
                    value: None,
                    lamp: 1,
                    peer: 1,
                }
            );
        }
    }
}
