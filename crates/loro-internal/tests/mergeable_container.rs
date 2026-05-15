//! Integration tests for mergeable container support. The actual tests live in focused files
//! under `tests/mergeable_container/`; this harness aggregates them into a single test target.

#[path = "mergeable_container/cid_encoding.rs"]
mod cid_encoding;
#[path = "mergeable_container/convergence.rs"]
mod convergence;
#[path = "mergeable_container/delete_and_tombstone.rs"]
mod delete_and_tombstone;
#[path = "mergeable_container/events_and_paths.rs"]
mod events_and_paths;
#[path = "mergeable_container/internal_helpers.rs"]
mod internal_helpers;
#[path = "mergeable_container/snapshot.rs"]
mod snapshot;
#[path = "mergeable_container/type_conflict.rs"]
mod type_conflict;
