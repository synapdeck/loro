use super::{ContainerState, DocState};
use crate::container::idx::ContainerIdx;
use crate::InternalString;
use rustc_hash::FxHashMap;

#[derive(Default, Debug, Clone)]
pub(super) struct DeadContainersCache {
    cache: FxHashMap<ContainerIdx, bool>,
}

impl DeadContainersCache {
    pub fn clear(&mut self) {
        self.cache.clear();
    }

    pub(crate) fn clear_alive(&mut self) {
        self.cache.retain(|_, is_deleted| *is_deleted);
    }
}

impl DocState {
    pub(crate) fn is_deleted(&mut self, idx: ContainerIdx) -> bool {
        #[cfg(not(debug_assertions))]
        {
            // Cache stores only deleted containers.
            if self.dead_containers_cache.cache.contains_key(&idx) {
                return true;
            }
        }

        let mut visited = vec![idx];
        let mut idx = idx;
        let is_deleted = loop {
            let id = self.arena.idx_to_id(idx).unwrap();
            if let Some(parent_idx) = self.arena.get_parent(idx) {
                let parent_id = self.arena.idx_to_id(parent_idx).unwrap();
                let Some(parent_state) = self.store.get_container_mut(parent_idx) else {
                    break true;
                };
                if !parent_state.contains_child(&id) {
                    // A mergeable child can be absent from `child_containers` while its KV
                    // state remains in the store: a `delete` evicts the side-table entry but
                    // preserves the underlying container so that a future post-tombstone op
                    // can resurrect it. Treat that detached-but-preserved case as not deleted
                    // (the handler stays usable for reads); any other "parent has no record
                    // of this child" case is a real deletion.
                    let is_tombstoned_mergeable_child = id
                        .parse_mergeable()
                        .filter(|(expected_parent, _, _)| expected_parent == &parent_id)
                        .is_some_and(|(_, key, _)| {
                            let key: InternalString = key.into();
                            parent_state
                                .as_map_state()
                                .and_then(|map_state| map_state.mergeable_tombstone(&key))
                                .is_some()
                        });

                    if !is_tombstoned_mergeable_child {
                        break true;
                    }
                }

                idx = parent_idx;
                visited.push(idx);
            } else {
                // No parent in the arena: top-level Roots are always alive; anything else
                // (including a mergeable Root whose parent edge was never wired) is treated
                // as deleted.
                break !id.is_root() || id.is_mergeable();
            }
        };

        #[cfg(debug_assertions)]
        {
            if let Some(cached_is_deleted) = self.dead_containers_cache.cache.get(&idx) {
                assert_eq!(is_deleted, *cached_is_deleted);
            }
        }

        if is_deleted {
            for idx in visited {
                self.dead_containers_cache.cache.insert(idx, true);
            }
        } else {
            for idx in visited {
                self.dead_containers_cache.cache.remove(&idx);
            }
        }

        is_deleted
    }
}
