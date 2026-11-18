//! cgroup v2: create, limit, freeze, kill, stats.
//!
//! Layout, under a root Sandfork may write to (the real root at L2, a
//! systemd-delegated scope at L1):
//!
//! ```text
//! <root>/daemon/                 the daemon and spawner (delegated scopes only)
//! <root>/sandfork.slice/
//!     group-<id>/                one per GRPO group: aggregate accounting
//!         sbx-<id>/              one per sandbox: limits; processes live here
//! ```
//!
//! Invariants:
//!
//! * **No internal processes.** cgroup v2 forbids processes in a cgroup that
//!   distributes controllers to children. Processes live only in `sbx-` leaves
//!   (and in `daemon/`). [`CgroupRoot::init`] moves the daemon into its own
//!   leaf *before* enabling controllers; doing it in the other order fails
//!   with `EBUSY`.
//! * **Limits before processes.** A sandbox's limits are written when its leaf
//!   is created, and its first process is cloned directly into the leaf.
//! * **Never sleep and hope.** Waiting for a freeze or for a kill to finish
//!   polls `cgroup.events`, which the kernel notifies on change.

use sandfork_core::wire::Limits;
use sandfork_core::{Result, SfError};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| SfError::infra("cgroup", format!("read {}: {e}", path.display())))
}

fn write(path: &Path, value: &str) -> Result<()> {
    std::fs::write(path, value).map_err(|e| {
        SfError::infra(
            "cgroup",
            format!("write {value:?} to {}: {e}", path.display()),
        )
    })
}

/// The cgroup this process is in, as a path under `/sys/fs/cgroup`.
pub fn own_cgroup() -> Option<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/')))
}

pub fn is_cgroup2() -> bool {
    Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
}

fn key_value(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()? == key)
            .then(|| it.next()?.parse().ok())
            .flatten()
    })
}

/// Block until `cgroup.events` in `dir` reports `key value`, or time out.
fn wait_event(dir: &Path, key: &str, value: u64, timeout_ms: u64) -> Result<bool> {
    let path = dir.join("cgroup.events");
    let deadline = sandfork_core::monotonic_ns() + timeout_ms * 1_000_000;
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        // The cgroup is already gone: nothing can be populated or thawed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => {
            return Err(SfError::infra(
                "cgroup",
                format!("open {}: {e}", path.display()),
            ))
        }
    };
    loop {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if key_value(&text, key) == Some(value) {
            return Ok(true);
        }
        let now = sandfork_core::monotonic_ns();
        if now >= deadline {
            return Ok(false);
        }
        let mut p = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        // SAFETY: `p` is one valid pollfd; the timeout is bounded.
        unsafe {
            libc::poll(
                &mut p,
                1,
                (((deadline - now) / 1_000_000) as i32).clamp(1, 1000),
            )
        };
    }
}

/// Statistics read at teardown, before the cgroup disappears.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CgStats {
    pub memory_peak: u64,
    pub memory_current: u64,
    pub oom_kills: u64,
    pub cpu_usec: u64,
    pub pids_current: u64,
    pub pids_max_events: u64,
}

/// One cgroup directory.
#[derive(Debug, Clone)]
pub struct Cgroup {
    pub path: PathBuf,
}

impl Cgroup {
    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }

    /// Write this sandbox's limits. Controllers that are not delegated are
    /// skipped; `enabled` is the parent's `cgroup.subtree_control`.
    pub fn set_limits(&self, limits: &Limits, enabled: &[String]) -> Result<()> {
        let has = |c: &str| enabled.iter().any(|e| e == c);
        if has("memory") {
            let max = limits.memory_mb << 20;
            write(&self.path.join("memory.max"), &max.to_string())?;
            // Throttle and reclaim before the hard limit.
            write(&self.path.join("memory.high"), &(max / 10 * 9).to_string())?;
            // No swap, so memory numbers mean something. Absent when swap
            // accounting is off, which already means no swap is charged.
            let _ = std::fs::write(self.path.join("memory.swap.max"), "0");
            // An out-of-memory kill takes the whole sandbox at once.
            let _ = std::fs::write(self.path.join("memory.oom.group"), "1");
        }
        if has("pids") {
            write(&self.path.join("pids.max"), &limits.pids.to_string())?;
        }
        if has("cpu") && limits.cpu > 0.0 {
            let quota = (limits.cpu * 100_000.0) as u64;
            write(&self.path.join("cpu.max"), &format!("{quota} 100000"))?;
        }
        Ok(())
    }

    pub fn add_pid(&self, pid: i32) -> Result<()> {
        write(&self.path.join("cgroup.procs"), &pid.to_string())
    }

    pub fn pids(&self) -> Vec<i32> {
        std::fs::read_to_string(self.path.join("cgroup.procs"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    /// Raise or lower this sandbox's CPU share (straggler policy).
    pub fn set_weight(&self, weight: u32) -> Result<()> {
        write(
            &self.path.join("cpu.weight"),
            &weight.clamp(1, 10_000).to_string(),
        )
    }

    /// Freeze and wait until the kernel reports `frozen 1`, so background
    /// processes have stopped writing before a snapshot is taken.
    pub fn freeze(&self, timeout_ms: u64) -> Result<()> {
        write(&self.path.join("cgroup.freeze"), "1")?;
        if !wait_event(&self.path, "frozen", 1, timeout_ms)? {
            let _ = std::fs::write(self.path.join("cgroup.freeze"), "0");
            return Err(SfError::infra(
                "freeze_timeout",
                format!("{} did not freeze in {timeout_ms} ms", self.path.display()),
            ));
        }
        Ok(())
    }

    pub fn thaw(&self) -> Result<()> {
        write(&self.path.join("cgroup.freeze"), "0")
    }

    /// Kill every process and wait until `populated 0`.
    pub fn kill(&self, timeout_ms: u64) -> Result<()> {
        if !self.exists() {
            return Ok(());
        }
        // A frozen cgroup's processes cannot die until thawed.
        let _ = std::fs::write(self.path.join("cgroup.freeze"), "0");
        if std::fs::write(self.path.join("cgroup.kill"), "1").is_err() {
            // Before Linux 5.14: signal everything in cgroup.procs, repeatedly.
            for _ in 0..50 {
                let pids = self.pids();
                if pids.is_empty() {
                    break;
                }
                for p in pids {
                    // SAFETY: signalling a PID read from our own cgroup.
                    unsafe { libc::kill(p, libc::SIGKILL) };
                }
                let _ = wait_event(&self.path, "populated", 0, 20);
            }
        }
        if !wait_event(&self.path, "populated", 0, timeout_ms)? {
            return Err(SfError::infra(
                "kill_timeout",
                format!(
                    "{} still populated after {timeout_ms} ms",
                    self.path.display()
                ),
            ));
        }
        Ok(())
    }

    pub fn stats(&self) -> CgStats {
        let num = |f: &str| {
            std::fs::read_to_string(self.path.join(f))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
        };
        let kv = |f: &str, k: &str| {
            std::fs::read_to_string(self.path.join(f))
                .ok()
                .and_then(|s| key_value(&s, k))
        };
        let current = num("memory.current").unwrap_or(0);
        CgStats {
            // memory.peak needs Linux 5.19; fall back to the current value.
            memory_peak: num("memory.peak").unwrap_or(current),
            memory_current: current,
            oom_kills: kv("memory.events", "oom_kill").unwrap_or(0),
            cpu_usec: kv("cpu.stat", "usage_usec").unwrap_or(0),
            pids_current: num("pids.current").unwrap_or(0),
            pids_max_events: kv("pids.events", "max").unwrap_or(0),
        }
    }

    /// Remove the (empty) cgroup directory.
    pub fn remove(&self) -> Result<()> {
        match std::fs::remove_dir(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SfError::infra(
                "cgroup",
                format!("rmdir {}: {e}", self.path.display()),
            )),
        }
    }
}

/// The subtree Sandfork owns.
#[derive(Debug, Clone)]
pub struct CgroupRoot {
    pub base: PathBuf,
    pub slice: PathBuf,
    /// Controllers enabled for sandbox leaves.
    pub controllers: Vec<String>,
}

const WANTED: [&str; 4] = ["cpu", "memory", "pids", "io"];

fn enable_controllers(dir: &Path) -> Vec<String> {
    let available = std::fs::read_to_string(dir.join("cgroup.controllers")).unwrap_or_default();
    for c in WANTED {
        if available.split_whitespace().any(|a| a == c) {
            let _ = std::fs::write(dir.join("cgroup.subtree_control"), format!("+{c}"));
        }
    }
    std::fs::read_to_string(dir.join("cgroup.subtree_control"))
        .unwrap_or_default()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

impl CgroupRoot {
    /// Take ownership of `base`. With `move_self`, every process currently in
    /// `base` (the daemon and its spawner) is first moved into `base/daemon`,
    /// which is required before `base` may distribute controllers.
    pub fn init(base: &Path, move_self: bool) -> Result<CgroupRoot> {
        if !base.join("cgroup.controllers").exists() {
            return Err(SfError::infra(
                "cgroup",
                format!("{} is not a cgroup v2 directory", base.display()),
            ));
        }
        if move_self {
            let leaf = base.join("daemon");
            std::fs::create_dir_all(&leaf)
                .map_err(|e| SfError::infra("cgroup", format!("mkdir {}: {e}", leaf.display())))?;
            for pid in read(&base.join("cgroup.procs"))?.lines() {
                write(&leaf.join("cgroup.procs"), pid.trim())?;
            }
        }
        enable_controllers(base);
        let slice = base.join("sandfork.slice");
        std::fs::create_dir_all(&slice)
            .map_err(|e| SfError::infra("cgroup", format!("mkdir {}: {e}", slice.display())))?;
        let controllers = enable_controllers(&slice);
        if !controllers.iter().any(|c| c == "memory") || !controllers.iter().any(|c| c == "pids") {
            return Err(SfError::infra(
                "cgroup",
                format!(
                    "memory and pids controllers are not delegated to {} (have: {controllers:?})",
                    base.display()
                ),
            ));
        }
        Ok(CgroupRoot {
            base: base.to_path_buf(),
            slice,
            controllers,
        })
    }

    fn group_dir(&self, group: &str) -> PathBuf {
        self.slice.join(format!("group-{group}"))
    }

    /// Create a group cgroup (idempotent).
    pub fn create_group(&self, group: &str) -> Result<Cgroup> {
        let dir = self.group_dir(group);
        std::fs::create_dir_all(&dir)
            .map_err(|e| SfError::infra("cgroup", format!("mkdir {}: {e}", dir.display())))?;
        enable_controllers(&dir);
        Ok(Cgroup { path: dir })
    }

    /// Create a sandbox leaf with its limits already written.
    pub fn create_sandbox(&self, group: &str, sandbox: &str, limits: &Limits) -> Result<Cgroup> {
        self.create_group(group)?;
        let dir = self.group_dir(group).join(format!("sbx-{sandbox}"));
        std::fs::create_dir(&dir)
            .map_err(|e| SfError::infra("cgroup", format!("mkdir {}: {e}", dir.display())))?;
        let cg = Cgroup { path: dir };
        if let Err(e) = cg.set_limits(limits, &self.controllers) {
            let _ = cg.remove();
            return Err(e);
        }
        Ok(cg)
    }

    pub fn remove_group(&self, group: &str) {
        let _ = std::fs::remove_dir(self.group_dir(group));
    }

    /// Every sandbox leaf that exists, as (group, sandbox) ids. Used by the
    /// reconciliation loop to find cgroups no live sandbox owns.
    pub fn list_sandboxes(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let Ok(groups) = std::fs::read_dir(&self.slice) else {
            return out;
        };
        for g in groups.flatten() {
            let gname = g.file_name().to_string_lossy().into_owned();
            let Some(gid) = gname.strip_prefix("group-") else {
                continue;
            };
            let Ok(leaves) = std::fs::read_dir(g.path()) else {
                continue;
            };
            for l in leaves.flatten() {
                if let Some(sid) = l.file_name().to_string_lossy().strip_prefix("sbx-") {
                    out.push((gid.to_string(), sid.to_string()));
                }
            }
        }
        out
    }

    pub fn list_groups(&self) -> Vec<String> {
        std::fs::read_dir(&self.slice)
            .map(|d| {
                d.flatten()
                    .filter_map(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .strip_prefix("group-")
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn sandbox(&self, group: &str, sandbox: &str) -> Cgroup {
        Cgroup {
            path: self.group_dir(group).join(format!("sbx-{sandbox}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_keyed_files() {
        let t = "populated 1\nfrozen 0\n";
        assert_eq!(key_value(t, "populated"), Some(1));
        assert_eq!(key_value(t, "frozen"), Some(0));
        assert_eq!(key_value(t, "missing"), None);
        assert_eq!(
            key_value("low 0\nhigh 3\nmax 1\noom 1\noom_kill 2\n", "oom_kill"),
            Some(2)
        );
    }

    #[test]
    fn waiting_on_a_missing_cgroup_returns_immediately() {
        assert!(wait_event(Path::new("/nonexistent/cgroup"), "populated", 0, 10).unwrap());
    }
}
