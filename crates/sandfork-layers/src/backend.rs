//! Snapshot backends behind one interface.
//!
//! | Backend            | Fork cost                    | Seal cost                    | Requires  |
//! |--------------------|------------------------------|------------------------------|-----------|
//! | Overlay (default)  | one tmpfs mount + one overlay| proportional to changed bytes| overlayfs |
//! | Plain copy         | proportional to bytes        | proportional to bytes        | nothing   |
//!
//! The plain-copy backend reflinks each file when the filesystem supports
//! `FICLONE` (XFS, btrfs), which makes its cost proportional to file count
//! instead of bytes; on ext4 it is the honest `cp -a` baseline. A btrfs
//! subvolume backend is not implemented (see `docs/layers.md`).

use crate::diff::{self, Change};
use crate::store::{LayerInfo, Snapshot, Store};
use crate::tree::{self, Copier, Owner};
use sandfork_core::{Ctx, Result, SfError};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// One sandbox's root: a snapshot plus one private writable layer.
#[derive(Debug, Clone)]
pub struct RootHandle {
    pub id: String,
    pub snapshot: String,
    /// The snapshot's layers, bottom to top.
    pub layers: Vec<String>,
    pub dir: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    /// Overlay: the mount point. Plain copy: the materialised tree.
    pub rootfs: PathBuf,
    pub tmpfs: bool,
    /// Plain copy only: the flattened snapshot this root was copied from.
    pub copy_base: Option<PathBuf>,
    pub quota_bytes: u64,
}

/// An overlay-format directory holding what a sandbox changed. Temporary
/// views are removed when dropped.
pub struct UpperView {
    pub path: PathBuf,
    temp: Option<PathBuf>,
}

impl Drop for UpperView {
    fn drop(&mut self) {
        if let Some(t) = self.temp.take() {
            let _ = tree::remove_all(&t);
        }
    }
}

pub trait SnapshotFs: Send + Sync {
    fn name(&self) -> &'static str;
    /// Create a sandbox root from a snapshot with a fresh writable layer.
    fn fork_root(
        &self,
        store: &Store,
        snap: &Snapshot,
        id: &str,
        quota_bytes: u64,
    ) -> Result<RootHandle>;
    /// What the sandbox changed, as an overlay-format directory.
    fn upper_view(&self, store: &Store, root: &RootHandle) -> Result<UpperView>;
    fn release(&self, store: &Store, root: &RootHandle) -> Result<()>;

    /// Capture the sandbox's changes as a new layer.
    fn seal(&self, store: &Store, root: &RootHandle) -> Result<LayerInfo> {
        let view = self.upper_view(store, root)?;
        store.seal(&view.path)
    }

    /// What the sandbox changed, relative to its snapshot.
    fn diff(&self, store: &Store, root: &RootHandle) -> Result<Vec<Change>> {
        let view = self.upper_view(store, root)?;
        let lowers: Vec<PathBuf> = root
            .layers
            .iter()
            .rev()
            .map(|l| store.layer_root(l))
            .collect();
        diff::changes(&lowers, &view.path, store.cfg.xattr_ns).ctx("diff writable layer")
    }
}

fn cpath(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| SfError::infra("path", "NUL in path"))
}

fn chown(p: &Path, owner: Option<(u32, u32)>) -> Result<()> {
    if let Some((u, g)) = owner {
        let c = cpath(p)?;
        // SAFETY: `c` is a live NUL-terminated string.
        if unsafe { libc::chown(c.as_ptr(), u, g) } != 0 {
            return Err(SfError::errno(&format!("chown {}", p.display())));
        }
    }
    Ok(())
}

/// The default backend: forking is two `mkdir`s and one tmpfs mount, whatever
/// the snapshot's size. The overlay mount itself is made by the spawner's
/// child, inside the sandbox's own mount namespace.
pub struct OverlayBackend {
    /// Back each writable layer with its own tmpfs, which gives a hard disk
    /// quota for free. Needs mount privilege and (for `userxattr`) Linux 6.6.
    pub tmpfs: bool,
    /// Daemon-side UID and GID of the sandbox's root.
    pub owner: Option<(u32, u32)>,
}

impl SnapshotFs for OverlayBackend {
    fn name(&self) -> &'static str {
        "overlay"
    }

    fn fork_root(
        &self,
        store: &Store,
        snap: &Snapshot,
        id: &str,
        quota_bytes: u64,
    ) -> Result<RootHandle> {
        let dir = store.rollout_dir(id);
        std::fs::create_dir(&dir).ctx_with(|| format!("create {}", dir.display()))?;
        let cleanup = |e: SfError| {
            if self.tmpfs {
                if let Ok(c) = cpath(&dir) {
                    // SAFETY: `c` is a live NUL-terminated string.
                    unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
                }
            }
            let _ = tree::remove_all(&dir);
            e
        };
        if self.tmpfs {
            let mut opts = format!("size={},mode=0755", quota_bytes.max(1 << 20));
            if let Some((u, g)) = self.owner {
                opts.push_str(&format!(",uid={u},gid={g}"));
            }
            let (target, fstype, data) = (
                cpath(&dir)?,
                CString::new("tmpfs").unwrap_or_default(),
                CString::new(opts).unwrap_or_default(),
            );
            // SAFETY: all pointers are live NUL-terminated strings.
            let r = unsafe {
                libc::mount(
                    fstype.as_ptr(),
                    target.as_ptr(),
                    fstype.as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV,
                    data.as_ptr().cast(),
                )
            };
            if r != 0 {
                return Err(cleanup(SfError::errno(&format!(
                    "mount tmpfs at {}",
                    dir.display()
                ))));
            }
        } else {
            chown(&dir, self.owner).map_err(cleanup)?;
        }
        let root = RootHandle {
            id: id.to_string(),
            snapshot: snap.id.clone(),
            layers: snap.layers.clone(),
            upper: dir.join("upper"),
            work: dir.join("work"),
            rootfs: dir.join("root"),
            dir: dir.clone(),
            tmpfs: self.tmpfs,
            copy_base: None,
            quota_bytes,
        };
        for d in [&root.upper, &root.work, &root.rootfs] {
            std::fs::create_dir(d)
                .ctx("create writable layer")
                .and_then(|_| chown(d, self.owner))
                .map_err(cleanup)?;
        }
        store
            .register_sandbox(id, &snap.id, &snap.layers)
            .map_err(cleanup)?;
        Ok(root)
    }

    fn upper_view(&self, _store: &Store, root: &RootHandle) -> Result<UpperView> {
        Ok(UpperView {
            path: root.upper.clone(),
            temp: None,
        })
    }

    fn release(&self, store: &Store, root: &RootHandle) -> Result<()> {
        if root.tmpfs {
            let c = cpath(&root.dir)?;
            // SAFETY: `c` is a live NUL-terminated string.
            unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
        }
        tree::remove_all(&root.dir).ctx_with(|| format!("remove {}", root.dir.display()))?;
        store.unregister_sandbox(&root.id)
    }
}

/// The baseline: materialise the snapshot once, then copy it per sandbox.
/// Used at L0, where there are no mounts, and as the comparison point for
/// the overlay backend.
pub struct CopyBackend {
    pub owner: Option<(u32, u32)>,
}

impl CopyBackend {
    fn flat(&self, store: &Store, snap: &Snapshot) -> Result<PathBuf> {
        let flat = store.root().join("flat").join(&snap.id);
        if !flat.exists() {
            let tmp = store.root().join("flat").join(format!("{}.tmp", snap.id));
            let _ = tree::remove_all(&tmp);
            store.flatten_into(&snap.layers, &tmp)?;
            if std::fs::rename(&tmp, &flat).is_err() {
                let _ = tree::remove_all(&tmp); // another fork materialised it first
            }
        }
        Ok(flat)
    }
}

impl SnapshotFs for CopyBackend {
    fn name(&self) -> &'static str {
        "copy"
    }

    fn fork_root(
        &self,
        store: &Store,
        snap: &Snapshot,
        id: &str,
        quota_bytes: u64,
    ) -> Result<RootHandle> {
        let flat = self.flat(store, snap)?;
        let dir = store.rollout_dir(id);
        std::fs::create_dir(&dir).ctx_with(|| format!("create {}", dir.display()))?;
        let rootfs = dir.join("root");
        let owner = if self.owner.is_some() {
            Owner::Preserve
        } else {
            Owner::Skip
        };
        if let Err(e) = Copier::new(owner).copy_tree(&flat, &rootfs, &[]) {
            let _ = tree::remove_all(&dir);
            return Err(SfError::infra(
                "layers",
                format!("copy snapshot for {id}: {e}"),
            ));
        }
        store.register_sandbox(id, &snap.id, &snap.layers)?;
        Ok(RootHandle {
            id: id.to_string(),
            snapshot: snap.id.clone(),
            layers: snap.layers.clone(),
            upper: PathBuf::new(),
            work: PathBuf::new(),
            rootfs,
            dir,
            tmpfs: false,
            copy_base: Some(flat),
            quota_bytes,
        })
    }

    fn upper_view(&self, store: &Store, root: &RootHandle) -> Result<UpperView> {
        let base = root
            .copy_base
            .as_ref()
            .ok_or_else(|| SfError::infra("layers", "copy root without a base"))?;
        let temp = store.new_staging()?;
        let owner = if self.owner.is_some() {
            Owner::Preserve
        } else {
            Owner::Skip
        };
        tree::tree_delta(base, &root.rootfs, &temp.join("root"), owner)
            .ctx("compute tree delta")?;
        Ok(UpperView {
            path: temp.join("root"),
            temp: Some(temp),
        })
    }

    fn release(&self, store: &Store, root: &RootHandle) -> Result<()> {
        tree::remove_all(&root.dir).ctx_with(|| format!("remove {}", root.dir.display()))?;
        store.unregister_sandbox(&root.id)
    }
}
