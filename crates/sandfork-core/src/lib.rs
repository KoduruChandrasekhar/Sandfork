//! Shared types for every Sandfork process.
//!
//! Invariants this crate owns:
//!
//! * **One wire format.** The daemon, the spawner, the shim and the zygote all
//!   link [`wire`], so the control protocol cannot drift between them. A frame
//!   is a little-endian `u32` length followed by one `postcard`-encoded enum.
//! * **Three error classes.** Every error carries an [`ErrorClass`]. `Task`
//!   belongs to the trajectory, `Infra` means retry or drop, `Client` is a bad
//!   request. Nothing downstream ever has to guess which is which.
//! * **No threads, no async.** Everything here is usable between `fork` and
//!   `exec`, so nothing in this crate spawns a thread or takes a global lock.

pub mod error;
pub mod fail;
pub mod fdpass;
pub mod glob;
pub mod ids;
pub mod rng;
pub mod task;
pub mod wire;

pub use error::{Ctx, ErrorClass, Result, SfError};

use serde::{Deserialize, Serialize};

/// Isolation level of a sandbox. See `docs/security.md` for what each stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Level {
    /// Own process group, rlimits, a private copy of the working directory.
    L0,
    /// Rootless namespaces, overlayfs, pivot_root, seccomp.
    L1,
    /// L1 run by real root, with cgroup v2 limits and CRIU.
    L2,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "l0" | "0" => Some(Level::L0),
            "l1" | "1" => Some(Level::L1),
            "l2" | "2" => Some(Level::L2),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Level::L0 => "l0",
            Level::L1 => "l1",
            Level::L2 => "l2",
        }
    }
}

/// UID and GID of the unprivileged in-sandbox user that runs agent commands.
pub const AGENT_UID: u32 = 1000;
pub const AGENT_GID: u32 = 1000;

/// Paths that exist inside every namespaced sandbox.
pub mod paths {
    pub const WORKSPACE: &str = "/workspace";
    pub const GRADER: &str = "/grader";
    pub const WHEELS: &str = "/wheels";
    pub const SHIM: &str = "/.sandfork/shim";
    pub const RT: &str = "/.sandfork/rt";
    pub const STORE: &str = "/.sandfork/store";
    pub const ROLLOUTS: &str = "/.sandfork/rollouts";
    pub const KERNEL_SOCK: &str = "/run/sandfork-kernel.sock";
}

/// Monotonic clock in nanoseconds. Every latency in Sandfork is measured with
/// `CLOCK_MONOTONIC`, never wall-clock time.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec and CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Wall-clock seconds since the epoch, for metadata only.
pub fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
