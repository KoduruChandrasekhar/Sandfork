//! Child side: build the world, then drop privileges.
//!
//! These functions run in a freshly cloned, single-threaded process that is
//! PID 1 of its new PID namespace and holds every capability in its new user
//! namespace. They are shared by the spawner's child and by a zygote's forked
//! child, so both kinds of sandbox are built by exactly the same code.

use crate::sys::{self, bind, chdir, mkdir_p, mount, touch};
use sandfork_core::wire::{Flavor, SandboxCfg, Step};
use sandfork_core::{monotonic_ns, Result, SfError};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Records how long each setup step took (`CLOCK_MONOTONIC`).
pub struct StepTimer {
    last: u64,
    pub steps: Vec<Step>,
}

impl Default for StepTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl StepTimer {
    pub fn new() -> Self {
        StepTimer {
            last: monotonic_ns(),
            steps: Vec::new(),
        }
    }
    pub fn mark(&mut self, name: &str) {
        let now = monotonic_ns();
        self.steps.push(Step {
            name: name.to_string(),
            ns: now - self.last,
        });
        self.last = now;
    }
}

fn open_path(path: &str) -> Result<OwnedFd> {
    let c = sys::cstr(path)?;
    // SAFETY: `c` is a live NUL-terminated string.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(SfError::infra(
            "syscall",
            format!("open({path}, O_PATH): {}", std::io::Error::last_os_error()),
        ));
    }
    // SAFETY: `open` returned a fresh descriptor that we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fd_path(fd: &OwnedFd) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

/// Become the user namespace's root. The maps must already be written.
fn become_root() -> Result<()> {
    // SAFETY: plain credential syscalls with integer arguments.
    unsafe {
        // Fails harmlessly when setgroups is denied (single-id mapping).
        libc::setgroups(0, std::ptr::null());
        if libc::setresgid(0, 0, 0) != 0 {
            return Err(SfError::errno("setresgid(0)"));
        }
        if libc::setresuid(0, 0, 0) != 0 {
            return Err(SfError::errno("setresuid(0)"));
        }
    }
    Ok(())
}

/// `/proc` entries hidden the way runc hides them.
const PROC_MASK: [&str; 9] = [
    "kcore",
    "sysrq-trigger",
    "timer_list",
    "keys",
    "latency_stats",
    "sched_debug",
    "kmsg",
    "acpi",
    "scsi",
];
const PROC_READONLY: [&str; 4] = ["sys", "irq", "bus", "fs"];

fn mask_proc(root: &str) -> Result<()> {
    for name in PROC_MASK {
        let p = format!("{root}/proc/{name}");
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => {
                let _ = mount(
                    Some("tmpfs"),
                    &p,
                    Some("tmpfs"),
                    libc::MS_RDONLY,
                    Some("size=0"),
                );
            }
            Ok(_) => {
                let _ = bind("/dev/null", &p, false, false);
            }
            Err(_) => {}
        }
    }
    for name in PROC_READONLY {
        let p = format!("{root}/proc/{name}");
        if std::fs::symlink_metadata(&p).is_ok() {
            // Not every kernel exposes every entry; a failed bind here leaves
            // the entry governed by the user namespace's own restrictions.
            let _ = bind(&p, &p, true, true);
        }
    }
    Ok(())
}

const DEV_NODES: [&str; 6] = ["null", "zero", "full", "random", "urandom", "tty"];

/// A tmpfs `/dev` holding only bind-mounted nodes. `mknod` is not allowed
/// inside a user namespace, so devices are bound, not created.
fn setup_dev(root: &str) -> Result<()> {
    let dev = format!("{root}/dev");
    mount(
        Some("tmpfs"),
        &dev,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NOEXEC,
        Some("mode=755,size=1m"),
    )?;
    for n in DEV_NODES {
        let dst = format!("{dev}/{n}");
        touch(&dst)?;
        let r = bind(&format!("/dev/{n}"), &dst, false, false);
        if n != "tty" {
            r?;
        }
    }
    for (name, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        let _ = std::os::unix::fs::symlink(target, format!("{dev}/{name}"));
    }
    let shm = format!("{dev}/shm");
    mkdir_p(&shm, 0o1777)?;
    mount(
        Some("tmpfs"),
        &shm,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=1777,size=64m"),
    )?;
    Ok(())
}

/// Bring `lo` up with `SIOCSIFFLAGS`; no netlink needed.
fn loopback_up() -> Result<()> {
    // SAFETY: socket/ioctl/close with a correctly sized ifreq buffer. An
    // `ifreq` is 40 bytes on Linux: 16 for the name, then the flags union.
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 {
            return Err(SfError::errno("socket for loopback"));
        }
        let mut ifr = [0u8; 40];
        ifr[..2].copy_from_slice(b"lo");
        if libc::ioctl(s, libc::SIOCGIFFLAGS as _, ifr.as_mut_ptr()) != 0 {
            let e = SfError::errno("SIOCGIFFLAGS lo");
            libc::close(s);
            return Err(e);
        }
        let flags =
            i16::from_ne_bytes([ifr[16], ifr[17]]) | (libc::IFF_UP | libc::IFF_RUNNING) as i16;
        ifr[16..18].copy_from_slice(&flags.to_ne_bytes());
        let r = libc::ioctl(s, libc::SIOCSIFFLAGS as _, ifr.as_mut_ptr());
        let e = SfError::errno("SIOCSIFFLAGS lo");
        libc::close(s);
        if r != 0 {
            return Err(e);
        }
    }
    Ok(())
}

/// Mount everything, pivot into the new root, set hostname and loopback.
/// On return the process is the mapped root of its namespace, in `/`.
pub fn build_world(cfg: &SandboxCfg, t: &mut StepTimer) -> Result<()> {
    // Host paths are opened while this process still has the spawner's kernel
    // UID, which owns the directories leading to them. Later mounts name them
    // through /proc/self/fd, which needs no path lookup.
    chdir(&cfg.layers_dir)?;
    let shim_fd = open_path(&cfg.shim)?;
    let rt_fd = if cfg.rt_dir.is_empty() {
        None
    } else {
        Some(open_path(&cfg.rt_dir)?)
    };
    let mut bind_fds = Vec::with_capacity(cfg.binds.len());
    for b in &cfg.binds {
        bind_fds.push(open_path(&b.src)?);
    }
    become_root()?;

    // Nothing may propagate back to the parent's mount namespace. A zygote
    // host stays a slave instead, so the per-sandbox tmpfs mounts the daemon
    // makes later still arrive in it.
    let prop = if cfg.flavor == Flavor::Zygote {
        libc::MS_SLAVE
    } else {
        libc::MS_PRIVATE
    };
    mount(None, "/", None, libc::MS_REC | prop, None)?;

    // Lower layers are listed top first and are read-only by construction.
    // Paths are relative to the working directory to keep the option string
    // short: the legacy mount API caps it at one page.
    let mut opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        cfg.lowers.join(":"),
        cfg.upper,
        cfg.work
    );
    if cfg.xattr_user {
        opts.push_str(",userxattr");
    }
    if cfg.volatile {
        opts.push_str(",volatile");
    }
    if opts.len() >= 4000 {
        return Err(SfError::infra(
            "layers",
            format!(
                "overlay option string is {} bytes; squash the snapshot",
                opts.len()
            ),
        ));
    }
    let root = cfg.rootfs.as_str();
    mount(
        Some("overlay"),
        root,
        Some("overlay"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some(&opts),
    )?;
    t.mark("overlay");

    mkdir_p(&format!("{root}/.sandfork"), 0o755)?;
    let shim_dst = format!("{root}/.sandfork/shim");
    touch(&shim_dst)?;
    bind(&fd_path(&shim_fd), &shim_dst, true, false)?;
    if let Some(fd) = &rt_fd {
        let dst = format!("{root}/.sandfork/rt");
        mkdir_p(&dst, 0o755)?;
        bind(&fd_path(fd), &dst, true, true)?;
    }
    for (b, fd) in cfg.binds.iter().zip(&bind_fds) {
        let dst = format!("{root}{}", b.dst);
        let src = fd_path(fd);
        let is_dir = std::fs::metadata(&src).map(|m| m.is_dir()).unwrap_or(true);
        if is_dir {
            mkdir_p(&dst, 0o755)?;
        } else {
            touch(&dst)?;
        }
        bind(&src, &dst, b.readonly, b.recursive)?;
    }
    t.mark("binds");

    let proc = format!("{root}/proc");
    mkdir_p(&proc, 0o555)?;
    mount(
        Some("proc"),
        &proc,
        Some("proc"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )?;
    mask_proc(root)?;
    t.mark("proc");

    mkdir_p(&format!("{root}/dev"), 0o755)?;
    setup_dev(root)?;
    t.mark("dev");

    for (dir, size) in [("tmp", cfg.tmp_bytes.max(1 << 20)), ("run", 16 << 20)] {
        let p = format!("{root}/{dir}");
        mkdir_p(&p, 0o1777)?;
        mount(
            Some("tmpfs"),
            &p,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some(&format!("mode=1777,size={size}")),
        )?;
    }
    t.mark("tmp");

    // The documented way to pivot without a put_old directory: stack the old
    // root on the new one, then detach it.
    chdir(root)?;
    sys::pivot_root(".", ".")?;
    sys::umount_detach(".")?;
    chdir("/")?;
    drop((shim_fd, rt_fd, bind_fds));
    t.mark("pivot_root");

    sys::sethostname(&cfg.hostname)?;
    loopback_up()?;
    t.mark("net");
    Ok(())
}

/// Capabilities the shim keeps so it can drop to the agent user, serve file
/// requests under that user, and kill its children: CHOWN, DAC_OVERRIDE,
/// DAC_READ_SEARCH, FOWNER, FSETID, KILL, SETGID, SETUID.
pub const SHIM_CAPS: [u32; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Reduce the bounding, permitted and effective sets to `keep`.
pub fn limit_caps(keep: &[u32]) -> Result<()> {
    const PR_CAP_AMBIENT: libc::c_int = 47;
    const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
    const CAP_V3: u32 = 0x2008_0522;
    let mask = keep.iter().fold(0u32, |m, c| m | (1 << c));
    // SAFETY: prctl and capset with integer arguments and a correctly laid out
    // version-3 header followed by two data structs.
    unsafe {
        libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0);
        for cap in 0..64u32 {
            if !keep.contains(&cap) {
                // EINVAL for capabilities this kernel does not know.
                libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
            }
        }
        let hdr = CapHeader {
            version: CAP_V3,
            pid: 0,
        };
        let data = [
            CapData {
                effective: mask,
                permitted: mask,
                inheritable: 0,
            },
            CapData {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
        ];
        if libc::syscall(libc::SYS_capset, &hdr as *const CapHeader, data.as_ptr()) != 0 {
            return Err(SfError::errno("capset"));
        }
    }
    Ok(())
}

fn set_rlimit(res: libc::c_int, value: u64) {
    let r = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: `r` is a valid rlimit; the resource constant is passed through.
    unsafe { libc::setrlimit(res as _, &r) };
}

/// rlimits as a backstop behind the cgroup limits.
pub fn set_rlimits(cfg: &SandboxCfg, namespaced: bool) {
    set_rlimit(
        libc::RLIMIT_NOFILE as libc::c_int,
        cfg.limits.nofile.max(64),
    );
    set_rlimit(libc::RLIMIT_CORE as libc::c_int, 0);
    if namespaced {
        // Counted per user namespace and UID since Linux 5.14, so this bounds
        // one sandbox. At L0 it would count the whole host user: skip it.
        set_rlimit(libc::RLIMIT_NPROC as libc::c_int, cfg.limits.pids.max(16));
        set_rlimit(
            libc::RLIMIT_FSIZE as libc::c_int,
            (cfg.limits.disk_mb.max(64) << 20) * 4,
        );
    }
}

/// rlimits, capability reduction and `no_new_privs`. Seccomp is separate,
/// because it must come after the descriptor shuffle that precedes `execve`.
pub fn drop_privileges(cfg: &SandboxCfg, t: &mut StepTimer) -> Result<()> {
    set_rlimits(cfg, true);
    if cfg.flavor == Flavor::Normal {
        limit_caps(&SHIM_CAPS)?;
        // SAFETY: prctl with integer arguments.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(SfError::errno("PR_SET_NO_NEW_PRIVS"));
        }
    }
    t.mark("privs");
    Ok(())
}
