//! The daemon's own long-lived user and mount namespace (L1).
//!
//! Rootless Podman uses the same trick with its pause process. Inside this
//! namespace the daemon and spawner are root, so they can mount each
//! sandbox's tmpfs, chown layer files into the sandbox ID range, and still
//! read a writable layer after the sandbox has pivoted into its own root.
//!
//! Mapping, when `/etc/subuid` and `newuidmap` are available ("full"):
//!
//! ```text
//! outer 0          <- the invoking host user
//! outer 1..=65536  <- the user's subordinate range
//! ```
//!
//! Every sandbox then maps its IDs `0..65536` onto outer `1..=65536`, so a
//! sandbox's root is a subordinate host UID, never the invoking user.
//! Without subordinate IDs the mapping degrades to a single ID ("single"):
//! sandbox root is the invoking user and there is no separate agent user.

use crate::sys;
use sandfork_core::fdpass::pipe;
use sandfork_core::{Result, SfError};
use std::os::fd::AsRawFd;

/// How sandbox IDs map into the namespace the daemon runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdPlan {
    /// The daemon-side ID of sandbox UID 0.
    pub base: u32,
    /// How many IDs each sandbox maps.
    pub count: u32,
}

impl IdPlan {
    pub fn single(&self) -> bool {
        self.count == 1
    }
    /// The line written to a sandbox's `uid_map` / `gid_map`.
    pub fn map_line(&self) -> String {
        format!("0 {} {}", self.base, self.count)
    }
    /// The daemon-side ID for a sandbox-side ID.
    pub fn outer(&self, sandbox_id: u32) -> u32 {
        if self.single() {
            self.base
        } else {
            self.base + sandbox_id.min(self.count - 1)
        }
    }
    /// The in-sandbox ID of the agent user.
    pub fn agent(&self) -> u32 {
        if self.single() {
            0
        } else {
            sandfork_core::AGENT_UID
        }
    }
}

/// Parse a subordinate ID file for `name` or `uid`. Returns (start, count).
pub fn parse_subid(text: &str, name: &str, uid: u32) -> Option<(u32, u32)> {
    let uid_s = uid.to_string();
    text.lines().find_map(|line| {
        let mut it = line.trim().split(':');
        let who = it.next()?;
        if who != name && who != uid_s {
            return None;
        }
        let start = it.next()?.parse().ok()?;
        let count = it.next()?.parse().ok()?;
        Some((start, count))
    })
}

fn user_name(uid: u32) -> String {
    // SAFETY: getpwuid returns null or a pointer to a static passwd record;
    // this process is single-threaded, so nothing else can overwrite it.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() || (*pw).pw_name.is_null() {
            return std::env::var("USER").unwrap_or_default();
        }
        std::ffi::CStr::from_ptr((*pw).pw_name)
            .to_string_lossy()
            .into_owned()
    }
}

fn find_helper(name: &str) -> Option<String> {
    ["/usr/bin", "/bin", "/usr/sbin", "/usr/local/bin"]
        .iter()
        .map(|d| format!("{d}/{name}"))
        .find(|p| std::path::Path::new(p).exists())
}

fn write_single_maps(uid: u32, gid: u32) -> Result<()> {
    sys::write_str("/proc/self/setgroups", "deny")?;
    sys::write_str("/proc/self/uid_map", &format!("0 {uid} 1"))?;
    sys::write_str("/proc/self/gid_map", &format!("0 {gid} 1"))?;
    Ok(())
}

/// Enter a new user and mount namespace and make every mount private.
/// Must be called before any thread exists.
pub fn enter_outer_userns() -> Result<IdPlan> {
    crate::assert_single_threaded("enter_outer_userns")?;
    // SAFETY: getuid/getgid/getpid have no preconditions.
    let (uid, gid, me) = unsafe { (libc::getuid(), libc::getgid(), libc::getpid()) };
    if uid == 0 {
        return Err(SfError::client(
            "userns",
            "already root: use level l2, or run as an ordinary user for l1",
        ));
    }
    let name = user_name(uid);
    let sub_u = std::fs::read_to_string("/etc/subuid")
        .ok()
        .and_then(|t| parse_subid(&t, &name, uid));
    let sub_g = std::fs::read_to_string("/etc/subgid")
        .ok()
        .and_then(|t| parse_subid(&t, &name, gid));
    let helpers = find_helper("newuidmap").zip(find_helper("newgidmap"));

    let full = match (sub_u, sub_g, &helpers) {
        (Some(u), Some(g), Some(_)) if u.1 >= 2 && g.1 >= 2 => Some((u, g)),
        _ => None,
    };

    let Some(((ustart, ucount), (gstart, gcount))) = full else {
        // SAFETY: single-threaded (asserted above).
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
            return Err(SfError::errno("unshare(CLONE_NEWUSER|CLONE_NEWNS)"));
        }
        write_single_maps(uid, gid)?;
        sys::mount(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;
        return Ok(IdPlan { base: 0, count: 1 });
    };
    let count = ucount.min(gcount).min(65536);
    let (newuidmap, newgidmap) = helpers.unwrap_or_default();

    // The helper stays in the initial user namespace: newuidmap must be run
    // from outside the namespace it is mapping.
    let (go_r, go_w) = pipe()?;
    // SAFETY: single-threaded, so the forked child may use the full runtime.
    let helper = unsafe { libc::fork() };
    if helper < 0 {
        return Err(SfError::errno("fork map helper"));
    }
    if helper == 0 {
        drop(go_w);
        let mut b = [0u8; 1];
        // SAFETY: reading one byte into a one-byte buffer.
        let n = unsafe { libc::read(go_r.as_raw_fd(), b.as_mut_ptr().cast(), 1) };
        let mut ok = n == 1;
        if ok {
            let run = |prog: &str, own: u32, start: u32| {
                std::process::Command::new(prog)
                    .args([
                        me.to_string(),
                        "0".into(),
                        own.to_string(),
                        "1".into(),
                        "1".into(),
                        start.to_string(),
                        count.to_string(),
                    ])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            };
            ok = run(&newuidmap, uid, ustart) && run(&newgidmap, gid, gstart);
        }
        // SAFETY: _exit is always safe to call.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) }
    }
    drop(go_r);
    // SAFETY: single-threaded (asserted above).
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
        let e = SfError::errno("unshare(CLONE_NEWUSER|CLONE_NEWNS)");
        drop(go_w);
        // SAFETY: reaping our own child.
        unsafe { libc::waitpid(helper, std::ptr::null_mut(), 0) };
        return Err(e);
    }
    // SAFETY: writing one byte, then reaping our own child.
    let status = unsafe {
        libc::write(go_w.as_raw_fd(), [1u8].as_ptr().cast(), 1);
        drop(go_w);
        let mut st = 0;
        libc::waitpid(helper, &mut st, 0);
        st
    };
    let plan = if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        IdPlan { base: 1, count }
    } else {
        // newuidmap refused (for example, a restrictive AppArmor profile).
        // The maps are still unwritten, so fall back to a single-ID mapping.
        write_single_maps(uid, gid)?;
        IdPlan { base: 0, count: 1 }
    };
    sys::mount(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    Ok(plan)
}

/// L2: a private mount namespace for a daemon that is already root, so its
/// mounts never reach the host's mount table and vanish when it exits.
pub fn enter_private_mountns() -> Result<()> {
    crate::assert_single_threaded("enter_private_mountns")?;
    // SAFETY: single-threaded (asserted above).
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        return Err(SfError::errno("unshare(CLONE_NEWNS)"));
    }
    sys::mount(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subid_parsing() {
        let t = "root:100000:65536\nalice:165536:65536\n1001:300000:1000\n";
        assert_eq!(parse_subid(t, "alice", 1000), Some((165536, 65536)));
        assert_eq!(parse_subid(t, "bob", 1001), Some((300000, 1000)));
        assert_eq!(parse_subid(t, "carol", 1002), None);
    }

    #[test]
    fn id_plans() {
        let full = IdPlan {
            base: 1,
            count: 65536,
        };
        assert_eq!(full.map_line(), "0 1 65536");
        assert_eq!(full.outer(0), 1);
        assert_eq!(full.outer(1000), 1001);
        assert_eq!(full.agent(), 1000);
        let single = IdPlan { base: 0, count: 1 };
        assert!(single.single());
        assert_eq!(single.outer(1000), 0);
        assert_eq!(single.agent(), 0);
    }
}
