//! Fork hygiene: checks a process must pass before it may serve as a zygote.
//!
//! | Check                  | Why |
//! |------------------------|-----|
//! | exactly one thread     | A child of a multithreaded process inherits locks held by threads that no longer exist, and deadlocks on them. BLAS and OpenMP thread pools are the usual cause. |
//! | no file open for writing | Siblings would share one open file description, including its offset: a hidden channel between sandboxes. |
//! | no sockets or pipes    | Same reason. Only the control channel is allowed. |
//! | empty writable layer   | Children re-root onto the lower layers plus their own empty layer. Anything the zygote wrote would vanish for them while its memory still assumed it existed. |
//! | no child processes     | Children are not inherited across fork. |
//! | no timers, no pending signals | A timer or signal would fire in every child. |
//!
//! The seventh check, a settled garbage collector, belongs to the language
//! runtime: the Python wrapper calls `gc.collect()` then `gc.freeze()`
//! before serving, which keeps the collector from walking, and writing to,
//! every shared object in every child.

use std::os::fd::RawFd;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub check: &'static str,
    pub detail: String,
}

fn fail(check: &'static str, detail: impl Into<String>) -> Result<(), Failure> {
    Err(Failure {
        check,
        detail: detail.into(),
    })
}

pub fn one_thread() -> Result<(), Failure> {
    let n = std::fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(0);
    if n != 1 {
        return fail("threads", format!("{n} threads exist; set OMP_NUM_THREADS=1, OPENBLAS_NUM_THREADS=1 and MKL_NUM_THREADS=1 before any import, and start no threads before serve()"));
    }
    Ok(())
}

/// Sockets, pipes and files open for writing, other than the control channel.
pub fn descriptors(ctl: RawFd) -> Result<(), Failure> {
    let me = std::process::id();
    let names: Vec<String> = match std::fs::read_dir("/proc/self/fd") {
        Ok(d) => d
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(e) => return fail("descriptors", format!("cannot list /proc/self/fd: {e}")),
    };
    for name in names {
        let Ok(fd) = name.parse::<RawFd>() else {
            continue;
        };
        if fd == ctl {
            continue;
        }
        let Ok(target) = std::fs::read_link(format!("/proc/self/fd/{fd}")) else {
            continue;
        };
        let target = target.to_string_lossy().into_owned();
        // The descriptor used to list the directory itself.
        if target == format!("/proc/{me}/fd") {
            continue;
        }
        if target.starts_with("socket:") || target.starts_with("pipe:") {
            return fail(
                "sockets_or_pipes",
                format!("descriptor {fd} is {target}; only the control channel may be open"),
            );
        }
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap_or_default();
        let flags = info
            .lines()
            .find_map(|l| l.strip_prefix("flags:"))
            .and_then(|v| u32::from_str_radix(v.trim(), 8).ok())
            .unwrap_or(0);
        let writable = flags & (libc::O_ACCMODE as u32) != libc::O_RDONLY as u32;
        let regular = std::fs::metadata(format!("/proc/self/fd/{fd}"))
            .map(|m| m.is_file())
            .unwrap_or(false);
        if writable && regular {
            return fail(
                "writable_files",
                format!("descriptor {fd} is open for writing on {target}"),
            );
        }
    }
    Ok(())
}

pub fn empty_writable_layer(upper: &Path) -> Result<(), Failure> {
    let entries: Vec<String> = match std::fs::read_dir(upper) {
        Ok(d) => d
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        // The layer is not visible from here (a cold start): nothing to check.
        Err(_) => return Ok(()),
    };
    if !entries.is_empty() {
        let shown: Vec<&str> = entries.iter().take(5).map(|s| s.as_str()).collect();
        return fail(
            "writable_layer",
            format!("the zygote wrote to its writable layer ({}); anything that writes files belongs in the task's preparation steps (set PYTHONDONTWRITEBYTECODE=1 or compile in a step)", shown.join(", ")),
        );
    }
    Ok(())
}

pub fn no_children() -> Result<(), Failure> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Ok(());
    };
    for t in tasks.flatten() {
        // Needs CONFIG_PROC_CHILDREN; where it is missing the check is
        // skipped rather than failed.
        let children = std::fs::read_to_string(t.path().join("children")).unwrap_or_default();
        if !children.trim().is_empty() {
            return fail(
                "children",
                format!(
                    "child processes exist ({}); children are not inherited across fork",
                    children.trim()
                ),
            );
        }
    }
    Ok(())
}

pub fn no_timers_or_pending_signals() -> Result<(), Failure> {
    let timers = std::fs::read_to_string("/proc/self/timers").unwrap_or_default();
    if !timers.trim().is_empty() {
        return fail(
            "timers",
            "POSIX timers are armed; a timer would fire in every child",
        );
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for key in ["SigPnd:", "ShdPnd:"] {
        if let Some(v) = status.lines().find_map(|l| l.strip_prefix(key)) {
            if u64::from_str_radix(v.trim(), 16).unwrap_or(0) != 0 {
                return fail(
                    "pending_signals",
                    format!(
                        "{key} {} is non-zero; a pending signal would be delivered in every child",
                        v.trim()
                    ),
                );
            }
        }
    }
    Ok(())
}

/// Every check, in the order the documentation lists them. The first failure
/// is returned, naming the check.
pub fn check_all(ctl: RawFd, upper: Option<&Path>) -> Result<(), Failure> {
    one_thread()?;
    descriptors(ctl)?;
    if let Some(u) = upper {
        empty_writable_layer(u)?;
    }
    no_children()?;
    no_timers_or_pending_signals()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_empty_layer_is_rejected_with_its_entries() {
        let d = std::env::temp_dir().join(format!("sf-hyg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(empty_writable_layer(&d).is_ok());
        std::fs::write(d.join("__pycache__"), b"x").unwrap();
        let f = empty_writable_layer(&d).unwrap_err();
        assert_eq!(f.check, "writable_layer");
        assert!(f.detail.contains("__pycache__"), "{}", f.detail);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_test_harness_itself_fails_the_thread_check() {
        // cargo's harness runs tests on worker threads.
        let f = one_thread().unwrap_err();
        assert_eq!(f.check, "threads");
        assert!(f.detail.contains("OMP_NUM_THREADS"));
    }

    #[test]
    fn a_writable_file_and_a_pipe_are_rejected() {
        let path = std::env::temp_dir().join(format!("sf-hyg-w-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let f = descriptors(-1).unwrap_err();
        assert!(
            f.check == "writable_files" || f.check == "sockets_or_pipes",
            "{f:?}"
        );
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}
