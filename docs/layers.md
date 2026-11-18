# Layered filesystem snapshots

Code: `crates/sandfork-layers`. Forking a filesystem should cost one mount
whatever the size of the prepared environment. This crate builds the store
that makes that true.

## Problem

A rollout needs a private, writable copy of the task's prepared filesystem:
the repository, installed dependencies, warm caches. Copying it per sandbox
costs time and disk proportional to its size, G times per group.

## Design

* **Layer:** an immutable directory tree in overlayfs upper format, plus a
  manifest. A deleted path is a whiteout (character device 0/0). A directory
  that replaces the one below it carries an opaque marker.
* **Snapshot:** an ordered list of layer IDs, bottom to top, plus metadata
  (task, parent, kind `base`/`prepared`/`turn`/`final`, idempotency key).
* **Sandbox:** a snapshot plus one private writable layer.

```
<state>/layers/<id[..20]>/root/        the tree, read-only once sealed
<state>/layers/<id[..20]>/manifest.bin
<state>/rollouts/<sandbox-id>/         per-sandbox tmpfs: upper/ work/ root/
<state>/store.sqlite                   layers, snapshots, sandboxes, mounts, checkpoints
```

Layer directories use the first 20 hex digits of the ID, so many lowers fit
in one mount option string. Lowers are passed relative to the store root for
the same reason.

### Manifests and content addressing

A manifest lists every path with its type, mode, owner (as a sandbox-side
ID), size, BLAKE3 content hash, symlink target, extended attributes and
hard-link target. The layer ID is the BLAKE3 hash of the canonical
serialisation, with modification times stored but excluded. Identical
preparation results therefore deduplicate. For example, all 200
data-analysis tasks share one prepared state: same layers, and one zygote.

### Forking

`OverlayBackend::fork_root` mounts a tmpfs of the task's `disk_mb` size,
creates `upper/`, `work/` and `root/` in it, and records the mounted layers
in SQLite. The overlay itself is mounted by the sandbox's own first process,
inside its user namespace, with `userxattr,volatile`. That is two `mkdir`s
and two mounts whatever the snapshot's size. The tmpfs doubles as a hard
disk quota: a rollout that fills it gets `ENOSPC`, which the live test
checks.

### Sealing: copy, never swap

`Store::seal` copies the writable layer into a staging directory, scans it
into a manifest, and renames it into place, or drops it when the identical
layer already exists. A live root filesystem cannot have its upper directory
replaced underneath it, so copying is the honest choice (decision 0004). The
cost grows with the changes since the fork, not with the environment. Before
a seal the daemon holds the shim's control channel, so no agent command is
running, and it freezes the cgroup and waits for `frozen 1`.

### Incremental seals

`Store::seal_delta` compares the writable layer's current manifest with the
one from the previous seal. It copies only entries whose content or metadata
changed, adds whiteouts for paths that existed then and are gone now, and
rewrites the whole subtree of a directory that has *become* opaque. Stacked
on the earlier layers, that delta is a correct overlay.

### Depth and squashing

Every branch point and every checkpoint adds a layer. When a sandbox's chain
grows past `--squash-k` (default 16), it is squashed into one layer, applied
bottom-up with whiteouts and opaque markers *kept*, because layers below the
run may still hold the paths they hide. A directory that replaces a whiteout
or a file becomes opaque for the same reason.

### Garbage collection

Snapshots hold layers. Sandboxes, pins (a prepared snapshot in use) and
checkpoints hold snapshots. A mark-and-sweep pass deletes unreferenced layers
only after a grace period, and only when no recorded mount lists them. Under
a disk budget, unpinned snapshots are evicted least-recently-used first.
Orphan directories (a crash between rename and insert, abandoned staging)
are removed too.

## Invariants

* Layers are immutable once renamed into `layers/`.
* **One extended-attribute namespace per store.** Rootless overlayfs
  (`userxattr`) writes `user.overlay.*` markers and overlayfs mounted by real
  root uses `trusted.overlay.*`. A layer mounted with the other namespace has
  its opaque directories silently ignored, so deleted directories come back.
  The namespace is recorded at creation and opening with the other one is
  refused (`the_other_xattr_namespace_is_refused`). Sandfork always mounts
  overlays from inside the sandbox's user namespace, so stores use
  `user.overlay.*` at every level.
* Copies preserve whiteouts, extended attributes, hard links and sparse
  files (`Copier`).

## Tests

* **The overlay-against-model property test**
  (`tests/overlay_model.rs`). Random sequences of create, write, append,
  truncate, delete, rename, mkdir, rmdir, rm -r, chmod, symlink and hard link
  are applied through *real overlay mounts* in several stages, and in
  parallel to a plain directory as the reference. After every batch the
  merged view must equal the model, and so must each of these:
  * the stack rebuilt from incremental seals,
  * the stack with a full seal,
  * the stack with the deltas squashed,
  * the userspace flatten.

  Later stages build alternately on delta chains and on squashes.
  `PROPTEST_CASES` sets the count: 48 by default, 500 in CI.
* Unit tests for deduplication, the xattr rule, hand-built whiteout and
  opaque cases through squash and flatten, change detection, incremental
  seals and garbage collection (`tests/store.rs`).

Two overlayfs behaviours the property test surfaced, and how they are
handled:

* Without `index=on`, copying up one name of a hard-linked file breaks the
  link. The model breaks links at the start of every stage to agree.
* Overlayfs refuses some operations a plain directory allows, such as
  renaming a directory from a lower layer without `redirect_dir`. The model
  applies an operation only if the overlay accepted it.

## Failure modes

| Failure | Handling |
|---|---|
| Crash between staging and rename | Staging directory removed by GC after the grace period |
| Crash between rename and the SQLite insert | Orphan layer directory removed by GC |
| Seal fails half-way | Staging removed; the error is `INFRA`; the sandbox is untouched (failpoints `seal_before_copy`, `seal_after_copy`) |
| Overlay option string too long | Refused with an error telling the caller to squash |

## Copy-up: where overlayfs gets expensive

Overlayfs copies a whole file up on its first write. The py-heavy family's
13 MB SQLite database is shared read-only by every rollout, but a rollout
that writes to it pays a 13 MB copy and 13 MB of tmpfs (counted against its
memory cgroup). The `metacopy` option avoids this only for metadata changes,
and needs real root. This is a known limitation; measuring it is part of
E1/E2 for py-heavy.

## Alternatives considered

| Backend | Fork cost | Seal cost | Status |
|---|---|---|---|
| Overlay (default) | two mounts | proportional to changed bytes | implemented |
| Plain copy (`cp -a`, reflink when available) | proportional to bytes (or file count with reflinks) | proportional to bytes | implemented: the L0 backend and baseline |
| Btrfs subvolume snapshot | constant | constant | not implemented: needs a btrfs volume and root |
