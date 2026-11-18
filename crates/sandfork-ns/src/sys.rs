//! Thin wrappers over the syscalls sandbox setup needs. Each returns an
//! infrastructure error that names the call and its arguments.

use sandfork_core::{Result, SfError};
use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

pub fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| SfError::infra("path", format!("NUL byte in {s:?}")))
}

fn os_err(what: String) -> SfError {
    SfError::infra(
        "syscall",
        format!("{what}: {}", std::io::Error::last_os_error()),
    )
}

pub fn mount(
    src: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let s = src.map(cstr).transpose()?;
    let t = cstr(target)?;
    let f = fstype.map(cstr).transpose()?;
    let d = data.map(cstr).transpose()?;
    // SAFETY: every pointer is either null or a live NUL-terminated string.
    let r = unsafe {
        libc::mount(
            s.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            t.as_ptr(),
            f.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            flags,
            d.as_ref().map_or(std::ptr::null(), |c| c.as_ptr().cast()),
        )
    };
    if r != 0 {
        return Err(os_err(format!(
            "mount({src:?} -> {target}, type {fstype:?}, flags {flags:#x})"
        )));
    }
    Ok(())
}

pub fn umount_detach(target: &str) -> Result<()> {
    let t = cstr(target)?;
    // SAFETY: `t` is a live NUL-terminated string.
    if unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) } != 0 {
        return Err(os_err(format!("umount2({target}, MNT_DETACH)")));
    }
    Ok(())
}

pub fn pivot_root(new_root: &str, put_old: &str) -> Result<()> {
    let n = cstr(new_root)?;
    let p = cstr(put_old)?;
    // SAFETY: both arguments are live NUL-terminated strings.
    if unsafe { libc::syscall(libc::SYS_pivot_root, n.as_ptr(), p.as_ptr()) } != 0 {
        return Err(os_err(format!("pivot_root({new_root}, {put_old})")));
    }
    Ok(())
}

pub fn chdir(path: &str) -> Result<()> {
    let p = cstr(path)?;
    // SAFETY: `p` is a live NUL-terminated string.
    if unsafe { libc::chdir(p.as_ptr()) } != 0 {
        return Err(os_err(format!("chdir({path})")));
    }
    Ok(())
}

pub fn sethostname(name: &str) -> Result<()> {
    // SAFETY: pointer and length describe `name`'s bytes.
    if unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) } != 0 {
        return Err(os_err(format!("sethostname({name})")));
    }
    Ok(())
}

pub fn mkdir_p(path: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .map_err(|e| SfError::infra("io", format!("mkdir {path}: {e}")))
}

pub fn touch(path: &str) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Ok(());
    }
    std::fs::File::create(path)
        .map(|_| ())
        .map_err(|e| SfError::infra("io", format!("create {path}: {e}")))
}

pub fn write_str(path: &str, s: &str) -> Result<()> {
    std::fs::write(path, s).map_err(|e| SfError::infra("io", format!("write {path}: {e}")))
}

/// Arguments for `clone3(2)`.
#[repr(C)]
#[derive(Default)]
pub struct CloneArgs {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

pub const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// `clone3` with fork semantics: returns 0 in the child, the child's PID in
/// the parent.
///
/// # Safety
/// The caller must be single-threaded. The child shares no memory with the
/// parent, but runs on a copy of the same stack, so it must either `execve` or
/// `_exit` and must not unwind into frames that assume they run once.
pub unsafe fn clone3(args: &mut CloneArgs) -> Result<i32> {
    let r = libc::syscall(
        libc::SYS_clone3,
        args as *mut CloneArgs,
        std::mem::size_of::<CloneArgs>(),
    );
    if r < 0 {
        return Err(os_err(format!("clone3(flags {:#x})", args.flags)));
    }
    Ok(r as i32)
}

pub fn pidfd_open(pid: i32) -> Result<OwnedFd> {
    // SAFETY: plain syscall with integer arguments.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(os_err(format!("pidfd_open({pid})")));
    }
    // SAFETY: the kernel returned a fresh descriptor that we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

pub fn pidfd_kill(pidfd: RawFd, sig: i32) {
    // SAFETY: plain syscall; a null siginfo asks the kernel to synthesise one.
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
}

/// Wait for `fd` to become readable. Returns false on timeout.
pub fn poll_readable(fd: RawFd, timeout_ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `p` is one valid pollfd.
        let r = unsafe { libc::poll(&mut p, 1, timeout_ms) };
        if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return r > 0;
    }
}

/// Translate `statvfs` flags of an existing mount into `MS_*` flags, so a
/// bind remount inside a user namespace keeps the flags the kernel has locked.
pub fn locked_mount_flags(path: &str) -> libc::c_ulong {
    let Ok(p) = cstr(path) else { return 0 };
    // SAFETY: an all-zero statvfs is a valid out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `p` is a live C string and `st` is writable.
    if unsafe { libc::statvfs(p.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    let f = st.f_flag;
    let mut out = 0;
    for (st_flag, ms_flag) in [
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (libc::ST_RELATIME, libc::MS_RELATIME),
    ] {
        if f & st_flag != 0 {
            out |= ms_flag;
        }
    }
    out
}

/// Bind `src` onto `dst`; optionally make it read-only.
pub fn bind(src: &str, dst: &str, readonly: bool, recursive: bool) -> Result<()> {
    let rec = if recursive { libc::MS_REC } else { 0 };
    mount(Some(src), dst, None, libc::MS_BIND | rec, None)?;
    if readonly {
        let keep = locked_mount_flags(dst);
        mount(
            None,
            dst,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | keep,
            None,
        )?;
    }
    Ok(())
}
