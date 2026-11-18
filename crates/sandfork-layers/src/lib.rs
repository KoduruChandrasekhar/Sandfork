//! Layered filesystem snapshots.
//!
//! Forking a filesystem should cost one mount, however large the prepared
//! environment is. This crate builds the layer store that makes that true.
//!
//! # The model
//!
//! * **Layer**: an immutable directory tree in overlayfs upper format, with
//!   whiteouts and opaque-directory markers, plus a manifest.
//! * **Snapshot**: an ordered list of layer IDs, bottom to top, plus metadata.
//! * **Sandbox**: a snapshot plus one private writable layer.
//!
//! # Invariants
//!
//! * **Layers are immutable once sealed.** Nothing writes under
//!   `layers/<id>/root` after [`Store::finalize`] has renamed it into place.
//! * **A layer's ID is the hash of its manifest**, with modification times
//!   excluded, so identical preparation results deduplicate.
//! * **One extended-attribute namespace per store.** The namespace is recorded
//!   when the store is created; opening it with the other one is refused.
//! * **Seal by copy, never by swapping the upper directory.** A live root
//!   filesystem cannot have its upper directory replaced underneath it, so a
//!   snapshot is a copy and its cost grows with the changes since the fork.
//! * **Whiteouts and opaque markers survive squashing**, because layers below
//!   the squashed run may still hold the paths they hide.

pub mod backend;
pub mod base;
pub mod diff;
pub mod manifest;
pub mod store;
pub mod tree;

pub use backend::{CopyBackend, OverlayBackend, RootHandle, SnapshotFs, UpperView};
pub use diff::{Change, ChangeKind};
pub use manifest::{Entry, Kind, Manifest};
pub use store::{CheckpointRow, GcReport, LayerInfo, Snapshot, Store, StoreCfg, StoreStats};
pub use tree::XattrNs;
