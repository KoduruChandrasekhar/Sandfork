//! Parent side: create the process, write its maps, wait for it to exec.

use crate::child::{build_world, drop_privileges, set_rlimits, StepTimer};
use crate::sys::{self, CloneArgs, CLONE_INTO_CGROUP};
use sandfork_core::fdpass::{pipe, socketpair};
use sandfork_core::wire::{recv_frame, send_frame, SandboxCfg, Step};
use sandfork_core::{monotonic_ns, Level, Result, SfError};
use seccompiler::BpfProgram;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// What the child tells the parent over the report socket. End-of-file with no
/// `Failed` means `execve` succeeded: the socket is close-on-exec.
#[derive(Debug, Serialize, Deserialize)]
enum ChildReport {
    Steps(Vec<Step>),
    Failed(SfError),
}

pub struct Spawned {
    pub pid: i32,
    pub pidfd: OwnedFd,
    /// The daemon's end of the shim's control channel.
    pub ctl: OwnedFd,
    pub steps: Vec<Step>,
}

/// How long the parent waits for the child to finish setup and exec.
const SETUP_TIMEOUT_MS: i32 = 30_000;

fn exec_shim(cfg: &SandboxCfg, path: &str) -> SfError {
    let Ok(prog) = CString::new(path) else {
        return SfError::infra("exec", "bad shim path");
    };
    let arg0 = CString::new("sandfork-shim").unwrap_or_default();
    let mut env: Vec<CString> = cfg
        .env
        .iter()
        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
        .collect();
    for (k, v) in [
        ("SANDFORK_ID", cfg.id.clone()),
        ("SANDFORK_LEVEL", cfg.level.as_str().to_string()),
        ("SANDFORK_AGENT_UID", cfg.agent_uid.to_string()),
        ("SANDFORK_AGENT_GID", cfg.agent_gid.to_string()),
        ("SANDFORK_ROOT", cfg.l0_root.clone()),
    ] {
        if let Ok(c) = CString::new(format!("{k}={v}")) {
            env.push(c);
        }
    }
    let argv = [arg0.as_ptr(), std::ptr::null()];
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|c| c.as_ptr()).collect();
    envp.push(std::ptr::null());
    // SAFETY: argv and envp are null-terminated arrays of live C strings.
    unsafe { libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    SfError::infra(
        "exec",
        format!("execve {path}: {}", std::io::Error::last_os_error()),
    )
}

/// Runs in the child. Never returns.
fn child_main(
    cfg: &SandboxCfg,
    seccomp: &[BpfProgram],
    sync_r: OwnedFd,
    report: OwnedFd,
    ctl: OwnedFd,
) -> ! {
    let fail = |report: RawFd, e: SfError| -> ! {
        let _ = send_frame(report, &ChildReport::Failed(e), &[]);
        // SAFETY: _exit is always safe to call.
        unsafe { libc::_exit(1) }
    };
    // Wait until the parent has written the maps. EOF means the parent failed.
    let mut b = [0u8; 1];
    // SAFETY: reading one byte into a one-byte buffer.
    let n = unsafe { libc::read(sync_r.as_raw_fd(), b.as_mut_ptr().cast(), 1) };
    drop(sync_r);
    if n != 1 {
        // SAFETY: as above.
        unsafe { libc::_exit(1) }
    }

    let mut t = StepTimer::new();
    let namespaced = cfg.level != Level::L0;
    let setup = if namespaced {
        build_world(cfg, &mut t).and_then(|_| drop_privileges(cfg, &mut t))
    } else {
        // L0: own session and process group, rlimits, nothing else.
        // SAFETY: setsid has no preconditions.
        unsafe { libc::setsid() };
        set_rlimits(cfg, false);
        t.mark("rlimits");
        Ok(())
    };
    if let Err(e) = setup {
        fail(report.as_raw_fd(), e);
    }

    // Only the control channel (fd 3) and the report socket (fd 4, close-on-
    // exec) survive. Both are first moved out of the way so neither dup2 can
    // clobber the other.
    // SAFETY: descriptor manipulation with valid descriptors; dies if the
    // spawner dies.
    let (ctl_fd, rep_fd) = unsafe {
        let c = libc::fcntl(ctl.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10);
        let r = libc::fcntl(report.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10);
        if c < 0 || r < 0 || libc::dup2(c, 3) < 0 || libc::dup3(r, 4, libc::O_CLOEXEC) < 0 {
            fail(report.as_raw_fd(), SfError::errno("descriptor shuffle"));
        }
        libc::syscall(libc::SYS_close_range, 5u32, u32::MAX, 0u32);
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        );
        (3, 4)
    };
    let _ = ctl_fd;
    std::mem::forget(ctl);
    std::mem::forget(report);

    if namespaced && cfg.seccomp {
        if let Err(e) = sandfork_seccomp::install(seccomp) {
            fail(rep_fd, SfError::infra("seccomp", e.to_string()));
        }
        t.mark("seccomp");
    }
    let _ = send_frame(
        rep_fd,
        &ChildReport::Steps(std::mem::take(&mut t.steps)),
        &[],
    );
    let shim = if namespaced {
        sandfork_core::paths::SHIM
    } else {
        cfg.shim.as_str()
    };
    let e = exec_shim(cfg, shim);
    fail(rep_fd, e)
}

fn write_maps(cfg: &SandboxCfg, pid: i32) -> Result<()> {
    // An unprivileged writer may only map its own id, and must deny setgroups
    // before the kernel accepts a gid_map.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        sys::write_str(&format!("/proc/{pid}/setgroups"), "deny")?;
    }
    sys::write_str(&format!("/proc/{pid}/uid_map"), &cfg.uid_map)?;
    sys::write_str(&format!("/proc/{pid}/gid_map"), &cfg.gid_map)?;
    Ok(())
}

fn kill_and_reap(pid: i32) {
    // SAFETY: signalling and reaping our own child.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut st = 0;
        libc::waitpid(pid, &mut st, 0);
    }
}

/// Create one sandbox. The caller must be single-threaded.
///
/// At L1 and L2 the returned process is PID 1 of its own namespaces, running
/// the shim, with `ctl` connected to the shim's descriptor 3. At L0 it is an
/// ordinary child running the shim.
pub fn spawn(cfg: &SandboxCfg, seccomp: &[BpfProgram]) -> Result<Spawned> {
    crate::assert_single_threaded("the spawner")?;
    let namespaced = cfg.level != Level::L0;
    let mut steps = Vec::new();
    let mut last = monotonic_ns();
    let mut mark = |name: &str, steps: &mut Vec<Step>| {
        let now = monotonic_ns();
        steps.push(Step {
            name: name.to_string(),
            ns: now - last,
        });
        last = now;
    };

    let cg_fd = if namespaced && !cfg.cgroup.is_empty() {
        let c = sys::cstr(&cfg.cgroup)?;
        // SAFETY: `c` is a live NUL-terminated string.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(SfError::errno(&format!("open cgroup {}", cfg.cgroup)));
        }
        // SAFETY: `open` returned a fresh descriptor that we now own.
        Some(unsafe { OwnedFd::from_raw_fd(fd) })
    } else {
        None
    };
    let (sync_r, sync_w) = pipe()?;
    let (rep_p, rep_c) = socketpair()?;
    let (ctl_p, ctl_c) = socketpair()?;

    let mut pidfd: libc::c_int = -1;
    let mut args = CloneArgs {
        exit_signal: libc::SIGCHLD as u64,
        ..Default::default()
    };
    args.flags = libc::CLONE_PIDFD as u64;
    args.pidfd = &mut pidfd as *mut libc::c_int as u64;
    if namespaced {
        args.flags |= (libc::CLONE_NEWUSER
            | libc::CLONE_NEWNS
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWNET
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWCGROUP) as u64;
        if let Some(fd) = &cg_fd {
            args.flags |= CLONE_INTO_CGROUP;
            args.cgroup = fd.as_raw_fd() as u64;
        }
    }
    // SAFETY: the spawner is single-threaded (asserted above), and the child
    // branch calls `child_main`, which never returns.
    let pid = unsafe { sys::clone3(&mut args)? };
    if pid == 0 {
        drop((sync_w, rep_p, ctl_p));
        child_main(cfg, seccomp, sync_r, rep_c, ctl_c);
    }
    drop((sync_r, rep_c, ctl_c, cg_fd));
    // SAFETY: CLONE_PIDFD stored a fresh descriptor in `pidfd`.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
    mark("clone3", &mut steps);

    if namespaced {
        if let Err(e) = write_maps(cfg, pid) {
            drop(sync_w);
            kill_and_reap(pid);
            return Err(e);
        }
    }
    mark("maps", &mut steps);
    // SAFETY: writing one byte from a one-byte buffer.
    unsafe { libc::write(sync_w.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    drop(sync_w);

    // A timeout on this end of the report socket: a child that hangs in setup
    // is killed rather than waited on forever.
    loop {
        if !sys::poll_readable(rep_p.as_raw_fd(), SETUP_TIMEOUT_MS) {
            kill_and_reap(pid);
            return Err(SfError::infra(
                "spawn_timeout",
                format!(
                    "sandbox {} did not finish setup in {SETUP_TIMEOUT_MS} ms",
                    cfg.id
                ),
            ));
        }
        match recv_frame::<ChildReport>(rep_p.as_raw_fd()) {
            Ok((ChildReport::Steps(mut s), _)) => steps.append(&mut s),
            Ok((ChildReport::Failed(e), _)) => {
                kill_and_reap(pid);
                return Err(e);
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                kill_and_reap(pid);
                return Err(SfError::infra("spawn", format!("report channel: {e}")));
            }
        }
    }
    // The child marks its last step just before execve, so the gap between
    // that mark and end-of-file on the report socket is the exec itself.
    let child_total: u64 = steps
        .iter()
        .filter(|s| s.name != "clone3" && s.name != "maps")
        .map(|s| s.ns)
        .sum();
    let since = monotonic_ns() - last;
    steps.push(Step {
        name: "exec".into(),
        ns: since.saturating_sub(child_total),
    });
    Ok(Spawned {
        pid,
        pidfd,
        ctl: ctl_p,
        steps,
    })
}
