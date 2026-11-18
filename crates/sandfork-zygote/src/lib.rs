//! The zygote library.
//!
//! When the prepared state lives in process memory (an interpreter with
//! libraries imported and a dataset loaded), a filesystem snapshot is not
//! enough. A zygote holds that state once, and every sandbox is a
//! copy-on-write fork of it. This is a library rather than a separate
//! process because the fork must happen inside the process that holds the
//! state. It is exposed to Python through a C ABI and `ctypes`.
//!
//! # What happens in one fork
//!
//! The zygote (Z) has no seccomp filter: filters are inherited and can never
//! be removed, so each child installs its own after its setup.
//!
//! 1. Z receives a request: the sandbox's configuration and its control
//!    socket, passed over `SCM_RIGHTS`. A fresh socket per fork: a shared one
//!    would be a single open file description, a hidden channel between
//!    siblings.
//! 2. Z forks child A.
//! 3. A unshares user, mount, network, UTS, IPC, PID and cgroup namespaces.
//!    It is single-threaded, so `CLONE_NEWUSER` is allowed. Z, which is root
//!    in its own user namespace, writes A's UID and GID maps.
//! 4. A forks B, which is PID 1 of the new PID namespace. A exits.
//! 5. B builds the world with the same code the spawner's child uses: a new
//!    overlay with the same lower layers and its own empty writable layer,
//!    `/proc`, `/dev`, `/tmp`, `pivot_root`, hostname, loopback, rlimits,
//!    capabilities, `no_new_privs`. The layer store is visible only in the
//!    zygote's root; after B pivots, nothing the agent runs can reach it.
//! 6. B opens a pidfd on itself and sends it to the daemon, which moves B
//!    into its cgroup and answers. B then installs seccomp.
//! 7. B forks C. B stays PID 1 and runs the shim loop in Rust: it reaps, runs
//!    commands and serves files, and never touches (so never un-shares) the
//!    interpreter's pages. C drops to the agent user, returns from `serve()`
//!    into the program and becomes the sandbox's stateful process.
//!
//! # Invariants
//!
//! * The zygote refuses to serve unless every hygiene check passes, and says
//!   exactly which one failed ([`hygiene`]).
//! * The zygote never runs agent code. Only children do, and only after
//!   seccomp is installed and privileges are dropped.

pub mod hygiene;

use sandfork_core::wire::{
    recv_frame, send_frame, ForkGo, ForkHello, SandboxCfg, ZygoteReq, ZygoteResp,
};
use sandfork_core::{Result, SfError};
use sandfork_ns::child::{build_world, drop_privileges, StepTimer};
use seccompiler::BpfProgram;
use std::ffi::{c_char, c_int, CStr};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

fn write_msg(buf: *mut c_char, len: usize, msg: &str) {
    if buf.is_null() || len == 0 {
        return;
    }
    let bytes = msg.as_bytes();
    let n = bytes.len().min(len - 1);
    // SAFETY: the caller promises `buf` is writable for `len` bytes; at most
    // `len - 1` bytes plus a terminating NUL are written.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), n);
        *buf.add(n) = 0;
    }
}

enum Forked {
    /// In the zygote: the child was started (or not).
    Parent(Result<()>),
    /// In the sandbox's stateful process: return into the program.
    Child,
}

fn exit_now(code: i32) -> ! {
    // SAFETY: _exit is always safe to call.
    unsafe { libc::_exit(code) }
}

/// Drop from the sandbox's root to the agent user. Mirrors what the shim
/// does for every command it runs.
fn become_agent(cfg: &SandboxCfg) -> Result<()> {
    // SAFETY: credential syscalls with integer arguments.
    unsafe {
        if cfg.agent_uid != 0 {
            let g = cfg.agent_gid;
            libc::setgroups(1, &g);
            if libc::setresgid(g, g, g) != 0
                || libc::setresuid(cfg.agent_uid, cfg.agent_uid, cfg.agent_uid) != 0
            {
                return Err(SfError::errno("drop to the agent user"));
            }
        } else {
            // Single-ID mapping: no separate agent user, so run without
            // capabilities instead.
            #[repr(C)]
            struct Hdr {
                version: u32,
                pid: i32,
            }
            let hdr = Hdr {
                version: 0x2008_0522,
                pid: 0,
            };
            let data = [0u32; 6];
            if libc::syscall(libc::SYS_capset, &hdr as *const Hdr, data.as_ptr()) != 0 {
                return Err(SfError::errno("drop capabilities"));
            }
        }
    }
    Ok(())
}

/// Runs in B: PID 1 of the new sandbox. Returns only in C.
fn sandbox_init(cfg: &SandboxCfg, ctl: OwnedFd, seccomp: &[BpfProgram]) {
    let fd = ctl.as_raw_fd();
    let fail = |e: SfError| -> ! {
        let _ = send_frame(fd, &ForkHello::Failed(e), &[]);
        exit_now(1)
    };
    let mut t = StepTimer::new();
    if let Err(e) = build_world(cfg, &mut t).and_then(|_| drop_privileges(cfg, &mut t)) {
        fail(e);
    }
    // A pidfd on ourselves: PID 1 in this namespace is this process. The
    // daemon gets a race-free handle with no PID translation.
    let pidfd = match sandfork_ns::sys::pidfd_open(1) {
        Ok(p) => p,
        Err(e) => fail(e),
    };
    if send_frame(
        fd,
        &ForkHello::Hello {
            steps: std::mem::take(&mut t.steps),
        },
        &[pidfd.as_raw_fd()],
    )
    .is_err()
    {
        exit_now(1);
    }
    drop(pidfd);
    // Wait until the daemon has moved this process into its cgroup, so the
    // stateful process starts inside the leaf with limits already set.
    if !sandfork_ns::sys::poll_readable(fd, 30_000) || recv_frame::<ForkGo>(fd).is_err() {
        exit_now(1);
    }
    if cfg.seccomp {
        if let Err(e) = sandfork_seccomp::install(seccomp) {
            fail(SfError::infra("seccomp", e.to_string()));
        }
    }
    // SAFETY: this process is single-threaded (the hygiene checks guarantee
    // the zygote was, and nothing since has started a thread).
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail(SfError::errno("fork the stateful process"));
    }
    if pid > 0 {
        // B: the reaper and command server. It runs only Rust code from here
        // on, so it never dirties the interpreter's shared pages.
        let raw = ctl.as_raw_fd();
        std::mem::forget(ctl);
        sandfork_shim::run(
            raw,
            sandfork_shim::ShimCfg {
                namespaced: true,
                root: "/".into(),
                agent_uid: cfg.agent_uid,
                agent_gid: cfg.agent_gid,
            },
        );
    }
    // C: the sandbox's stateful process. Its output goes to a log in the
    // sandbox's own /tmp, which no sibling and not the zygote can see.
    drop(ctl);
    // SAFETY: open and dup2 on paths and descriptors this process owns.
    unsafe {
        let fd = libc::open(
            c"/tmp/sandfork-kernel.log".as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC,
            0o644,
        );
        if fd >= 0 {
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            libc::close(fd);
        }
    }
    if become_agent(cfg).is_err() {
        exit_now(1);
    }
    let _ = std::env::set_current_dir(sandfork_core::paths::WORKSPACE);
}

fn write_file(path: &str, data: &str) -> Result<()> {
    std::fs::write(path, data).map_err(|e| SfError::infra("io", format!("write {path}: {e}")))
}

/// Fork one sandbox from this process.
fn fork_sandbox(
    cfg: &SandboxCfg,
    ctl: OwnedFd,
    seccomp: &[BpfProgram],
    zygote_ctl: RawFd,
) -> Forked {
    let pipes =
        sandfork_core::fdpass::pipe().and_then(|a| sandfork_core::fdpass::pipe().map(|b| (a, b)));
    let ((unshared_r, unshared_w), (mapped_r, mapped_w)) = match pipes {
        Ok(p) => p,
        Err(e) => return Forked::Parent(Err(SfError::infra("pipe", e.to_string()))),
    };
    // SAFETY: the zygote is single-threaded (hygiene check), so fork is safe
    // and the child may run ordinary code.
    let a = unsafe { libc::fork() };
    if a < 0 {
        return Forked::Parent(Err(SfError::errno("fork")));
    }
    if a == 0 {
        // ---- child A ----
        drop((unshared_r, mapped_w));
        // SAFETY: closing the zygote's own channel in the child; nothing
        // else in this process uses that descriptor.
        unsafe { libc::close(zygote_ctl) };
        let flags = libc::CLONE_NEWUSER
            | libc::CLONE_NEWNS
            | libc::CLONE_NEWNET
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWCGROUP;
        // SAFETY: single-threaded, so CLONE_NEWUSER is permitted.
        if unsafe { libc::unshare(flags) } != 0 {
            let _ = send_frame(
                ctl.as_raw_fd(),
                &ForkHello::Failed(SfError::errno("unshare in zygote child")),
                &[],
            );
            exit_now(1);
        }
        let mut b = [0u8; 1];
        // SAFETY: one-byte write and read on pipes this process owns.
        unsafe {
            libc::write(unshared_w.as_raw_fd(), [1u8].as_ptr().cast(), 1);
            if libc::read(mapped_r.as_raw_fd(), b.as_mut_ptr().cast(), 1) != 1 {
                libc::_exit(1);
            }
        }
        drop((unshared_w, mapped_r));
        // The PID namespace applies to A's children: B is its PID 1.
        // SAFETY: single-threaded.
        let b_pid = unsafe { libc::fork() };
        if b_pid != 0 {
            exit_now(if b_pid > 0 { 0 } else { 1 });
        }
        // ---- child B ----
        sandbox_init(cfg, ctl, seccomp);
        return Forked::Child; // only C gets here
    }
    // ---- the zygote ----
    drop((unshared_w, mapped_r, ctl));
    let mut b = [0u8; 1];
    // SAFETY: one-byte read on a pipe this process owns.
    let unshared = unsafe { libc::read(unshared_r.as_raw_fd(), b.as_mut_ptr().cast(), 1) } == 1;
    let result = if unshared {
        // The zygote is root in its own user namespace, so it may write a
        // full map for a namespace nested inside it.
        write_file(&format!("/proc/{a}/uid_map"), &cfg.uid_map)
            .and_then(|_| write_file(&format!("/proc/{a}/gid_map"), &cfg.gid_map))
    } else {
        Err(SfError::infra(
            "zygote_fork",
            "child failed before its namespaces existed",
        ))
    };
    if result.is_ok() {
        // SAFETY: one-byte write on a pipe this process owns.
        unsafe { libc::write(mapped_w.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }
    drop((mapped_w, unshared_r));
    let mut st = 0;
    // SAFETY: reaping our own child A, which exits as soon as B exists.
    unsafe { libc::waitpid(a, &mut st, 0) };
    Forked::Parent(result.and_then(|_| {
        if libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0 {
            Ok(())
        } else {
            Err(SfError::infra(
                "zygote_fork",
                format!("intermediate child exited with status {st}"),
            ))
        }
    }))
}

/// Run the hygiene checks only. Returns 0 if a zygote here could serve.
///
/// # Safety
/// `upper` must be null or a NUL-terminated string, and `buf` must be null
/// or writable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn sandfork_zygote_check(
    ctl_fd: c_int,
    upper: *const c_char,
    buf: *mut c_char,
    len: usize,
) -> c_int {
    let upper = if upper.is_null() {
        None
    } else {
        CStr::from_ptr(upper)
            .to_str()
            .ok()
            .filter(|s| !s.is_empty())
    };
    match hygiene::check_all(ctl_fd, upper.map(std::path::Path::new)) {
        Ok(()) => 0,
        Err(f) => {
            write_msg(buf, len, &format!("{}: {}", f.check, f.detail));
            2
        }
    }
}

/// Become a zygote: run the hygiene checks, then loop forever forking
/// sandboxes on request.
///
/// Returns 0 in a forked sandbox's stateful process (continue into the
/// program), 1 in the zygote when its channel closed (exit), and 2 if a
/// hygiene check failed (the message is in `buf` and was sent to the daemon).
///
/// # Safety
/// `ctl_fd` must be the zygote's channel to the daemon. `upper` must be null
/// or a NUL-terminated string, and `buf` must be null or writable for `len`
/// bytes. The calling process must be single-threaded.
#[no_mangle]
pub unsafe extern "C" fn sandfork_zygote_serve(
    ctl_fd: c_int,
    upper: *const c_char,
    buf: *mut c_char,
    len: usize,
) -> c_int {
    let upper = if upper.is_null() {
        None
    } else {
        CStr::from_ptr(upper)
            .to_str()
            .ok()
            .filter(|s| !s.is_empty())
    };
    if let Err(f) = hygiene::check_all(ctl_fd, upper.map(std::path::Path::new)) {
        write_msg(buf, len, &format!("{}: {}", f.check, f.detail));
        let _ = send_frame(
            ctl_fd,
            &ZygoteResp::HygieneFailed {
                check: f.check.to_string(),
                detail: f.detail,
            },
            &[],
        );
        return 2;
    }
    // Compiled once; every child inherits the programs through fork.
    let seccomp = match sandfork_seccomp::compile(&sandfork_seccomp::policy()) {
        Ok(p) => p,
        Err(e) => {
            let _ = send_frame(
                ctl_fd,
                &ZygoteResp::HygieneFailed {
                    check: "seccomp".into(),
                    detail: e.to_string(),
                },
                &[],
            );
            return 2;
        }
    };
    if send_frame(
        ctl_fd,
        &ZygoteResp::Ready {
            rss_kb: rss_kb(),
            pid: libc::getpid(),
        },
        &[],
    )
    .is_err()
    {
        return 1;
    }
    let mut forks = 0u64;
    loop {
        let (req, mut fds) = match recv_frame::<ZygoteReq>(ctl_fd) {
            Ok(r) => r,
            Err(_) => return 1,
        };
        match req {
            ZygoteReq::Exit => return 1,
            ZygoteReq::Ping => {
                if send_frame(
                    ctl_fd,
                    &ZygoteResp::Pong {
                        forks,
                        rss_kb: rss_kb(),
                    },
                    &[],
                )
                .is_err()
                {
                    return 1;
                }
            }
            ZygoteReq::Fork(cfg) => {
                let Some(ctl) = fds.pop() else {
                    let _ = send_frame(
                        ctl_fd,
                        &ZygoteResp::Forked {
                            ok: false,
                            detail: "fork request arrived without a control socket".into(),
                        },
                        &[],
                    );
                    continue;
                };
                match fork_sandbox(&cfg, ctl, &seccomp, ctl_fd) {
                    Forked::Child => return 0,
                    Forked::Parent(r) => {
                        forks += 1;
                        let resp = match r {
                            Ok(()) => ZygoteResp::Forked {
                                ok: true,
                                detail: String::new(),
                            },
                            Err(e) => ZygoteResp::Forked {
                                ok: false,
                                detail: e.message,
                            },
                        };
                        if send_frame(ctl_fd, &resp, &[]).is_err() {
                            return 1;
                        }
                    }
                }
            }
        }
    }
}
