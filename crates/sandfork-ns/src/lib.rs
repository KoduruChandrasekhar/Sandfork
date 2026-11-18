//! Building one sandbox from raw syscalls.
//!
//! # The setup order, and why each step precedes the next
//!
//! Parent ([`spawn::spawn`], single-threaded):
//!
//! 1. Open the sandbox's cgroup directory, so the child can start *inside* its
//!    leaf with limits already set (`CLONE_INTO_CGROUP`). Setting limits after
//!    processes exist is how fork bombs survive the PID limit.
//! 2. Create the sync pipe, the report socket and the control socket pair.
//! 3. `clone3` with user, mount, PID, network, UTS, IPC and cgroup namespaces
//!    plus `CLONE_PIDFD`. The pidfd is how the daemon watches the sandbox
//!    without PID-reuse races.
//! 4. Write the UID and GID maps. The child cannot write a multi-range map for
//!    itself, and it must not proceed before the maps exist.
//! 5. Release the child through the sync pipe.
//!
//! Child ([`child::build_world`], then [`child::drop_privileges`]):
//!
//! 1. `mount(/, MS_REC | MS_PRIVATE)` first. Without it the overlay mount can
//!    propagate back into the parent's mount namespace.
//! 2. Mount the overlay. Path-based mounts happen while the child still has
//!    the *spawner's* kernel UID: that UID owns the directories leading to the
//!    layer store, so path lookup needs no extra privilege. The mapped root
//!    UID could not traverse a mode-0750 home directory.
//! 3. Bind mounts (shim, runtime, host `/usr`, grader), then `/proc`, `/dev`
//!    and `/tmp`. `/proc` needs the new PID namespace, which the child is
//!    already PID 1 of.
//! 4. `pivot_root(".", ".")` then `umount2(".", MNT_DETACH)`. The new root must
//!    be a mount point, which the overlay is. After this the host tree is
//!    unreachable, so everything that needs host paths is already done.
//! 5. Hostname, loopback, rlimits.
//! 6. Become the mapped root (`setresuid(0)`), then cut capabilities to the few
//!    the shim needs. This comes after the mounts because of step 2.
//! 7. `no_new_privs`, `PR_SET_PDEATHSIG`, close every descriptor except the
//!    control channel.
//! 8. Seccomp, last, because it forbids the calls steps 1 to 4 just made.
//! 9. `execve` the shim.
//!
//! # Invariants
//!
//! * Nothing in this crate may run in a multithreaded process. `fork` in a
//!   threaded process leaves locks held forever, and `unshare(CLONE_NEWUSER)`
//!   fails with `EINVAL`. [`assert_single_threaded`] enforces it at startup.
//! * The child never returns into the caller's code on failure: it reports the
//!   error over the report socket and exits.

pub mod child;
pub mod outer;
pub mod spawn;
pub mod sys;

use sandfork_core::{Result, SfError};

/// Number of threads in this process, from `/proc/self/task`.
pub fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(1)
}

/// Refuse to continue in a multithreaded process.
pub fn assert_single_threaded(who: &str) -> Result<()> {
    let n = thread_count();
    if n != 1 {
        return Err(SfError::infra(
            "threads",
            format!("{who} must be single-threaded, found {n} threads"),
        ));
    }
    Ok(())
}
