//! The skeleton layer at the bottom of every snapshot.
//!
//! It holds the mount points a sandbox needs (`/proc`, `/dev`, `/tmp`, the
//! shim, the grader), a generated `/etc`, and the agent's home and workspace.
//! For a `host` base the system itself comes from read-only binds of the
//! host's `/usr`, so the skeleton also carries the usual `/bin -> usr/bin`
//! links. For a `rootfs` base it is stacked on an imported root filesystem
//! and adds only the mount points.

use crate::store::{LayerInfo, Store};
use crate::tree;
use sandfork_core::task::BaseSpec;
use sandfork_core::{Ctx, Result, SfError, AGENT_GID, AGENT_UID};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Bumped whenever the skeleton's contents change, so bases built from an
/// older skeleton are not reused from the cache.
pub const SKELETON_VERSION: u32 = 2;

const TOP_LINKS: [&str; 6] = ["bin", "sbin", "lib", "lib32", "lib64", "libx32"];

/// Host directories bound read-only into every sandbox of a `host` base: the
/// ones the base lists, plus top-level system directories that are real
/// directories on this host (a distribution without merged `/usr`).
pub fn effective_binds(base: &BaseSpec) -> Vec<String> {
    if base.kind != "host" {
        return vec![];
    }
    let mut out = base.binds.clone();
    for name in TOP_LINKS {
        let p = format!("/{name}");
        if let Ok(m) = std::fs::symlink_metadata(&p) {
            if m.is_dir() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

fn mkdir(root: &Path, rel: &str, mode: u32) -> std::io::Result<()> {
    let p = root.join(rel);
    std::fs::create_dir_all(&p)?;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode))
}

fn lchown(p: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    let c = CString::new(p.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: `c` is a live NUL-terminated string.
    if unsafe { libc::lchown(c.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Build the skeleton layer for `base`.
///
/// `outer` maps a sandbox-side ID to the daemon-side ID to chown to; `None`
/// leaves ownership alone (L0). `agent` is the in-sandbox agent UID and GID.
pub fn build_skeleton(
    store: &Store,
    base: &BaseSpec,
    outer: Option<&dyn Fn(u32) -> u32>,
    agent: (u32, u32),
) -> Result<LayerInfo> {
    let staging = store.new_staging()?;
    let root = staging.join("root");
    let host = base.kind == "host";
    let r = (|| -> std::io::Result<()> {
        for (d, mode) in [
            ("proc", 0o555),
            ("dev", 0o755),
            ("tmp", 0o1777),
            ("run", 0o755),
            ("etc", 0o755),
            ("opt", 0o755),
            ("var/tmp", 0o1777),
            ("home/agent", 0o755),
            ("workspace", 0o755),
            ("grader", 0o755),
            ("wheels", 0o755),
            (".sandfork/rt", 0o755),
            (".sandfork/store", 0o755),
            // Mount point for a zygote's view of the layer store. Every mount
            // point a sandbox needs must exist here: creating one at start-up
            // would write to the writable layer.
            (".sandfork/store/layers", 0o755),
            (".sandfork/rollouts", 0o755),
            ("root", 0o700),
        ] {
            mkdir(&root, d, mode)?;
        }
        std::fs::write(root.join(".sandfork/shim"), b"")?;
        std::fs::set_permissions(
            root.join(".sandfork/shim"),
            std::fs::Permissions::from_mode(0o755),
        )?;
        if !host {
            return Ok(());
        }
        for b in effective_binds(base) {
            mkdir(&root, b.trim_start_matches('/'), 0o755)?;
        }
        for name in TOP_LINKS {
            let hp = format!("/{name}");
            if let Ok(m) = std::fs::symlink_metadata(&hp) {
                if m.file_type().is_symlink() {
                    std::os::unix::fs::symlink(std::fs::read_link(&hp)?, root.join(name))?;
                }
            }
        }
        let (au, ag) = agent;
        let mut passwd = String::from("root:x:0:0:root:/root:/bin/sh\n");
        let mut group = String::from("root:x:0:\n");
        if au != 0 {
            passwd.push_str(&format!("agent:x:{au}:{ag}:agent:/home/agent:/bin/sh\n"));
            group.push_str(&format!("agent:x:{ag}:\n"));
        }
        passwd.push_str("nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n");
        group.push_str("nogroup:x:65534:\n");
        for (name, body) in [
            ("passwd", passwd.as_str()),
            ("group", group.as_str()),
            ("hosts", "127.0.0.1 localhost sandbox\n::1 localhost\n"),
            ("hostname", "sandbox\n"),
            ("resolv.conf", ""),
            (
                "nsswitch.conf",
                "passwd: files\ngroup: files\nhosts: files\n",
            ),
        ] {
            std::fs::write(root.join("etc").join(name), body)?;
        }
        for src in &base.etc_copy {
            let sp = Path::new(src);
            let Ok(m) = std::fs::symlink_metadata(sp) else {
                continue;
            };
            let dst = root.join(src.trim_start_matches('/'));
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut cp = tree::Copier::new(tree::Owner::Skip);
            cp.copy_entry(sp, &dst, "", &m)?;
            if m.is_dir() {
                cp.copy_tree(sp, &dst, &[])?;
            }
        }
        Ok(())
    })();
    let r = r.and_then(|_| {
        let Some(outer) = outer else { return Ok(()) };
        let (au, ag) = agent;
        lchown(&root, outer(0), outer(0))?;
        for (rel, _) in tree::walk(&root)? {
            let mine = rel == "workspace" || rel == "home/agent";
            let (u, g) = if mine {
                (outer(au), outer(ag))
            } else {
                (outer(0), outer(0))
            };
            lchown(&root.join(&rel), u, g)?;
        }
        Ok(())
    });
    if let Err(e) = r {
        let _ = tree::remove_all(&staging);
        return Err(SfError::infra(
            "base",
            format!("build skeleton for base {}: {e}", base.name),
        ));
    }
    store.finalize(&staging).ctx("finalize skeleton")
}

/// The default agent IDs.
pub fn default_agent() -> (u32, u32) {
    (AGENT_UID, AGENT_GID)
}
