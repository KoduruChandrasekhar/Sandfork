//! The shim: PID 1 in every sandbox.
//!
//! It is a library as well as a binary because a zygote's forked child runs
//! the same loop in-process, without an `execve` that would throw away the
//! memory it was forked to share.
//!
//! # Contract
//!
//! 1. **Reap.** On every wakeup, loop `wait4(-1, WNOHANG)` until nothing is
//!    left. Orphans reparent to PID 1, and `SIGCHLD` coalesces, so reaping one
//!    child per signal would fill the sandbox with zombies.
//! 2. **Run commands.** The child calls `setsid`, drops to the unprivileged
//!    in-sandbox user and `execve`s. The parent polls both pipes, kills the
//!    whole process group with `SIGKILL` at the timeout, and returns the exit
//!    status, CPU time and peak RSS from `wait4`.
//! 3. **Bound its own memory.** Output is forwarded up to a per-stream cap;
//!    after that only a fixed-size tail is kept. An agent can print gigabytes;
//!    the shim never allocates in proportion to that.
//! 4. **Serve file requests** resolved with `openat2`: `RESOLVE_BENEATH` from
//!    the workspace for relative paths, `RESOLVE_IN_ROOT` for absolute ones,
//!    under the agent's filesystem UID so permissions apply exactly as they
//!    would to the agent. Path resolution is never hand-rolled.
//! 5. **Handle signals as PID 1.** `SIGTERM` from outside the sandbox kills
//!    its children and exits; `SIGTERM`, `SIGINT` and `SIGHUP` from inside are
//!    ignored, and the kernel never delivers `SIGKILL` to PID 1 from inside.
//!
//! # Trust
//!
//! The shim trusts the daemon (its only peer on the control channel) and
//! trusts nothing inside the sandbox. It holds no secrets and no host paths.

use sandfork_core::wire::{
    recv_frame, send_frame, DirEnt, ExecExit, ExecSpec, ShimReq, ShimResp, CHUNK,
};
use sandfork_core::{monotonic_ns, SfError};
use std::collections::VecDeque;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub struct ShimCfg {
    /// False at L0, where there is no private root.
    pub namespaced: bool,
    /// `/` in a namespaced sandbox; the sandbox's directory at L0.
    pub root: String,
    pub agent_uid: u32,
    pub agent_gid: u32,
}

/// Bytes of each stream kept after the forwarded head is full.
const TAIL: usize = 8192;
/// How long to keep draining output after the command's process has exited
/// (a background grandchild may still hold the pipes open).
const DRAIN_NS: u64 = 100_000_000;

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_IN_ROOT: u64 = 0x10;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

struct Stream {
    fd: Option<OwnedFd>,
    forwarded: u64,
    total: u64,
    tail: VecDeque<u8>,
    dropped: u64,
}

impl Stream {
    fn new(fd: OwnedFd) -> Self {
        Stream {
            fd: Some(fd),
            forwarded: 0,
            total: 0,
            tail: VecDeque::with_capacity(TAIL),
            dropped: 0,
        }
    }
}

struct Running {
    pid: i32,
    out: Stream,
    err: Stream,
    started: u64,
    deadline: u64,
    max_output: u64,
    exit: Option<(i32, libc::rusage)>,
    exited_at: u64,
    timed_out: bool,
    cancelled: bool,
}

struct Shim {
    cfg: ShimCfg,
    ctl: RawFd,
    sigfd: OwnedFd,
    root_fd: OwnedFd,
    ws_fd: Option<OwnedFd>,
    started: u64,
    reaped: u64,
    running: Option<Running>,
    background: Vec<i32>,
}

fn cstring(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in string"))
}

fn open_dir(path: &str) -> io::Result<OwnedFd> {
    let c = cstring(path)?;
    // SAFETY: `c` is a live NUL-terminated string.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` returned a fresh descriptor that we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn set_nonblocking(fd: RawFd) {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
}

fn io_err(e: io::Error, what: &str) -> SfError {
    // A failed file operation inside the sandbox is the task's outcome
    // (missing file, permission, path escape), never an infrastructure error.
    SfError::task("file", format!("{what}: {e}"))
}

impl Shim {
    fn send(&self, resp: &ShimResp) {
        if send_frame(self.ctl, resp, &[]).is_err() {
            // The daemon is gone. A sandbox without its daemon has no purpose.
            self.shutdown(0);
        }
    }

    fn shutdown(&self, code: i32) -> ! {
        // SAFETY: signalling and exiting. As PID 1 of a PID namespace,
        // kill(-1) reaches every process in the sandbox and nothing else.
        unsafe {
            if self.cfg.namespaced {
                libc::kill(-1, libc::SIGKILL);
            } else {
                if let Some(r) = &self.running {
                    libc::kill(-r.pid, libc::SIGKILL);
                }
                for p in &self.background {
                    libc::kill(-p, libc::SIGKILL);
                }
            }
            libc::_exit(code)
        }
    }

    /// Reap everything that has exited. Called on every wakeup, because
    /// SIGCHLD coalesces.
    fn reap(&mut self) {
        loop {
            let mut st = 0;
            // SAFETY: an all-zero rusage is a valid out-parameter.
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            // SAFETY: both out-parameters are valid for writes.
            let pid = unsafe { libc::wait4(-1, &mut st, libc::WNOHANG, &mut ru) };
            if pid <= 0 {
                break;
            }
            if let Some(r) = &mut self.running {
                if r.pid == pid {
                    r.exit = Some((st, ru));
                    r.exited_at = monotonic_ns();
                    continue;
                }
            }
            self.background.retain(|p| *p != pid);
            self.reaped += 1;
        }
    }

    /// Runs in the forked child. Never returns.
    fn exec_child(
        &self,
        spec: &ExecSpec,
        stdin: RawFd,
        stdout: RawFd,
        stderr: RawFd,
        keep: Option<RawFd>,
    ) -> ! {
        let die = |msg: String, code: i32| -> ! {
            let m = format!("sandfork-shim: {msg}\n");
            // SAFETY: writing a live buffer to stderr, then exiting.
            unsafe {
                libc::write(2, m.as_ptr().cast(), m.len());
                libc::_exit(code)
            }
        };
        // SAFETY: the shim is single-threaded, so the child may run ordinary
        // code. Everything here is descriptor, signal and credential setup.
        unsafe {
            libc::setsid();
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            for s in [
                libc::SIGINT,
                libc::SIGHUP,
                libc::SIGPIPE,
                libc::SIGTERM,
                libc::SIGCHLD,
            ] {
                libc::signal(s, libc::SIG_DFL);
            }
            // Move the descriptors to keep out of the 0..=3 range first, so
            // no dup2 below can overwrite one that is still needed.
            let hi = |fd: RawFd| {
                if fd >= 0 {
                    libc::fcntl(fd, libc::F_DUPFD, 10)
                } else {
                    -1
                }
            };
            let (i, o, e, k) = (hi(stdin), hi(stdout), hi(stderr), keep.map(hi));
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            libc::dup2(if i >= 0 { i } else { null }, 0);
            libc::dup2(if o >= 0 { o } else { null }, 1);
            libc::dup2(if e >= 0 { e } else { null }, 2);
            let first_closed = match k {
                Some(k) => {
                    libc::dup2(k, 3);
                    4
                }
                None => 3,
            };
            libc::syscall(libc::SYS_close_range, first_closed as u32, u32::MAX, 0u32);
        }
        let cwd = self.host_path(&spec.cwd);
        if let Err(e) = std::env::set_current_dir(&cwd) {
            die(format!("chdir {}: {e}", spec.cwd), 126);
        }
        if self.cfg.namespaced && !spec.as_root && self.cfg.agent_uid != 0 {
            // SAFETY: credential syscalls with integer arguments. The group
            // list is cleared before the IDs change; the order matters,
            // because setresuid drops the capability setgroups needs.
            unsafe {
                let g = self.cfg.agent_gid;
                libc::setgroups(1, &g);
                if libc::setresgid(g, g, g) != 0
                    || libc::setresuid(self.cfg.agent_uid, self.cfg.agent_uid, self.cfg.agent_uid)
                        != 0
                {
                    die(
                        format!(
                            "drop to uid {}: {}",
                            self.cfg.agent_uid,
                            io::Error::last_os_error()
                        ),
                        126,
                    );
                }
            }
        }
        if self.cfg.namespaced && !spec.as_root && self.cfg.agent_uid == 0 {
            // Single-ID mapping: there is no separate agent user to drop to,
            // so agent commands at least run without any capability. With
            // no_new_privs set, execve cannot hand them back.
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
            // SAFETY: capset with a version-3 header and two zeroed data structs.
            if unsafe { libc::syscall(libc::SYS_capset, &hdr as *const Hdr, data.as_ptr()) } != 0 {
                die(
                    format!("drop capabilities: {}", io::Error::last_os_error()),
                    126,
                );
            }
        }
        for (k, _) in std::env::vars_os() {
            std::env::remove_var(k);
        }
        for (k, v) in &spec.env {
            std::env::set_var(k, v);
        }
        let Some(prog) = spec.argv.first() else {
            die("empty argv".into(), 127)
        };
        let args: Vec<CString> = spec
            .argv
            .iter()
            .filter_map(|a| CString::new(a.as_str()).ok())
            .collect();
        let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        // SAFETY: argv is a null-terminated array of live C strings.
        unsafe { libc::execvp(args[0].as_ptr(), argv.as_ptr()) };
        die(format!("exec {prog}: {}", io::Error::last_os_error()), 127)
    }

    /// Map a sandbox path to a path this process can use. Identity in a
    /// namespaced sandbox; at L0 absolute paths live under the sandbox's
    /// directory.
    fn host_path(&self, p: &str) -> String {
        let p = if p.is_empty() {
            sandfork_core::paths::WORKSPACE
        } else {
            p
        };
        if self.cfg.namespaced || !p.starts_with('/') {
            p.to_string()
        } else {
            format!("{}{}", self.cfg.root.trim_end_matches('/'), p)
        }
    }

    fn start_exec(&mut self, spec: ExecSpec) {
        if self.running.is_some() {
            return self.send(&ShimResp::Err(SfError::client(
                "busy",
                "a command is already running in this sandbox",
            )));
        }
        let (pipes, stdin) = match (|| -> io::Result<_> {
            let out = sandfork_core::fdpass::pipe()?;
            let err = sandfork_core::fdpass::pipe()?;
            let stdin = if spec.stdin.is_empty() {
                None
            } else {
                Some(sandfork_core::fdpass::pipe()?)
            };
            Ok(((out, err), stdin))
        })() {
            Ok(p) => p,
            Err(e) => return self.send(&ShimResp::Err(SfError::infra("pipe", e.to_string()))),
        };
        let ((out_r, out_w), (err_r, err_w)) = pipes;
        // SAFETY: the shim is single-threaded; the child branch never returns.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            // Hitting pids.max or RLIMIT_NPROC is the sandbox's own doing.
            return self.send(&ShimResp::Err(SfError::task(
                "fork",
                io::Error::last_os_error().to_string(),
            )));
        }
        if pid == 0 {
            let sin = stdin.as_ref().map_or(-1, |(r, _)| r.as_raw_fd());
            self.exec_child(&spec, sin, out_w.as_raw_fd(), err_w.as_raw_fd(), None);
        }
        drop((out_w, err_w));
        if let Some((r, w)) = stdin {
            drop(r);
            // At most one pipe buffer, so this cannot block the loop.
            let data = &spec.stdin[..spec.stdin.len().min(60_000)];
            // SAFETY: writing a live buffer to a pipe we own.
            unsafe { libc::write(w.as_raw_fd(), data.as_ptr().cast(), data.len()) };
        }
        set_nonblocking(out_r.as_raw_fd());
        set_nonblocking(err_r.as_raw_fd());
        let now = monotonic_ns();
        let timeout_ms = if spec.timeout_ms == 0 {
            3_600_000
        } else {
            spec.timeout_ms
        };
        self.running = Some(Running {
            pid,
            out: Stream::new(out_r),
            err: Stream::new(err_r),
            started: now,
            deadline: now + timeout_ms * 1_000_000,
            max_output: spec.max_output as u64,
            exit: None,
            exited_at: 0,
            timed_out: false,
            cancelled: false,
        });
    }

    /// Read what is available from one stream. Forward the head, keep a tail.
    fn pump(&mut self, is_err: bool) {
        let ctl = self.ctl;
        let Some(r) = self.running.as_mut() else {
            return;
        };
        let max = r.max_output;
        let s = if is_err { &mut r.err } else { &mut r.out };
        let Some(fd) = s.fd.as_ref().map(|f| f.as_raw_fd()) else {
            return;
        };
        let mut buf = [0u8; 65536];
        loop {
            // SAFETY: reading into a live buffer of the stated length.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n == 0 {
                s.fd = None;
                return;
            }
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => return,
                    _ => {
                        s.fd = None;
                        return;
                    }
                }
            }
            let data = &buf[..n as usize];
            s.total += data.len() as u64;
            let head = (max.saturating_sub(s.forwarded) as usize).min(data.len());
            if head > 0 {
                let chunk = data[..head].to_vec();
                s.forwarded += head as u64;
                let msg = if is_err {
                    ShimResp::Stderr(chunk)
                } else {
                    ShimResp::Stdout(chunk)
                };
                if send_frame(ctl, &msg, &[]).is_err() {
                    // SAFETY: exiting; see `shutdown`.
                    unsafe { libc::_exit(0) };
                }
            }
            for b in &data[head..] {
                if s.tail.len() == TAIL {
                    s.tail.pop_front();
                    s.dropped += 1;
                }
                s.tail.push_back(*b);
            }
        }
    }

    fn kill_running(&mut self, timed_out: bool) {
        if let Some(r) = &mut self.running {
            if r.exit.is_none() {
                // The whole process group: a command that double-forked must
                // not leave grandchildren running.
                // SAFETY: signalling a process group we created with setsid.
                unsafe { libc::kill(-r.pid, libc::SIGKILL) };
                r.timed_out |= timed_out;
                r.cancelled |= !timed_out;
            }
        }
    }

    /// Finish the running command if its process is gone and its output is drained.
    fn maybe_finish(&mut self) {
        let now = monotonic_ns();
        let done = match &self.running {
            Some(r) => {
                r.exit.is_some()
                    && ((r.out.fd.is_none() && r.err.fd.is_none()) || now - r.exited_at > DRAIN_NS)
            }
            None => false,
        };
        if !done {
            return;
        }
        let Some(r) = self.running.take() else { return };
        let Some((st, ru)) = r.exit else { return };
        let ms = |t: libc::timeval| t.tv_sec as u64 * 1000 + t.tv_usec as u64 / 1000;
        let (code, signal) = if libc::WIFEXITED(st) {
            (libc::WEXITSTATUS(st), 0)
        } else if libc::WIFSIGNALED(st) {
            (-1, libc::WTERMSIG(st))
        } else {
            (-1, 0)
        };
        let exit = ExecExit {
            code,
            signal,
            timed_out: r.timed_out,
            cancelled: r.cancelled,
            wall_ms: (now - r.started) / 1_000_000,
            user_ms: ms(ru.ru_utime),
            sys_ms: ms(ru.ru_stime),
            max_rss_kb: ru.ru_maxrss as u64,
            truncated: r.out.total > r.out.forwarded || r.err.total > r.err.forwarded,
            dropped_bytes: r.out.dropped + r.err.dropped,
            tail_stdout: r.out.tail.into_iter().collect(),
            tail_stderr: r.err.tail.into_iter().collect(),
        };
        self.send(&ShimResp::Exit(exit));
    }

    /// `openat2`, the only way the shim ever resolves a caller-supplied path.
    fn open_in(&self, path: &str, flags: i32, mode: u32) -> io::Result<OwnedFd> {
        let (base, rel, resolve) = if let Some(abs) = path.strip_prefix('/') {
            (
                self.root_fd.as_raw_fd(),
                abs.trim_start_matches('/'),
                RESOLVE_IN_ROOT,
            )
        } else {
            let ws = self.ws_fd.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "sandbox has no workspace")
            })?;
            (ws.as_raw_fd(), path, RESOLVE_BENEATH)
        };
        let rel = if rel.is_empty() { "." } else { rel };
        let c = cstring(rel)?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: mode as u64,
            resolve: resolve | RESOLVE_NO_MAGICLINKS,
        };
        // SAFETY: `c` and `how` are live for the call; the size matches.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                base,
                c.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel returned a fresh descriptor that we now own.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    /// Run `f` with the agent's filesystem credentials, so permissions apply
    /// exactly as they would to the agent.
    fn as_agent<T>(&self, f: impl FnOnce(&Self) -> T) -> T {
        let switch = self.cfg.namespaced && self.cfg.agent_uid != 0;
        // SAFETY: setfsgid/setfsuid take plain integers and cannot fail in a
        // way that needs handling here: with the IDs unchanged the operation
        // simply runs as the shim's own user.
        unsafe {
            if switch {
                libc::setfsgid(self.cfg.agent_gid);
                libc::setfsuid(self.cfg.agent_uid);
            }
            let r = f(self);
            if switch {
                libc::setfsuid(0);
                libc::setfsgid(0);
            }
            r
        }
    }

    fn read_file(&self, path: &str, offset: u64, max_len: u32) -> io::Result<ShimResp> {
        let fd = self.open_in(path, libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY, 0)?;
        // SAFETY: an all-zero stat is a valid out-parameter for fstat.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open and `st` is writable.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        let size = st.st_size as u64;
        let want = (max_len as usize).min(CHUNK);
        let mut data = vec![0u8; want];
        let mut got = 0;
        while got < want {
            // SAFETY: reading into the unfilled tail of `data`.
            let n = unsafe {
                libc::pread(
                    fd.as_raw_fd(),
                    data[got..].as_mut_ptr().cast(),
                    want - got,
                    (offset + got as u64) as i64,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                break;
            }
            got += n as usize;
        }
        data.truncate(got);
        Ok(ShimResp::File {
            eof: offset + got as u64 >= size,
            data,
            size,
        })
    }

    /// Create the missing parent directories of `path`, one component at a time.
    fn make_parents(&self, path: &str) -> io::Result<()> {
        let lead = if path.starts_with('/') { "/" } else { "" };
        let parts: Vec<&str> = path
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        for i in 0..parts.len().saturating_sub(1) {
            let prefix = format!("{lead}{}", parts[..=i].join("/"));
            if self
                .open_in(&prefix, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                .is_ok()
            {
                continue;
            }
            if parts[i] == ".." {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "`..` in a path that does not exist yet",
                ));
            }
            let parent = match (i, lead) {
                (0, "") => ".".to_string(),
                (0, _) => "/".to_string(),
                _ => format!("{lead}{}", parts[..i].join("/")),
            };
            // The parent is opened through openat2, so it is a real directory
            // inside the sandbox; creating one plain name in it cannot escape.
            let dir = self.open_in(&parent, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            let c = cstring(parts[i])?;
            // SAFETY: `dir` is an open directory and `c` a single path component.
            if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o755) } != 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn write_file(&self, path: &str, data: &[u8], mode: u32, append: bool) -> io::Result<ShimResp> {
        self.make_parents(path)?;
        let flags = libc::O_WRONLY
            | libc::O_CREAT
            | if append {
                libc::O_APPEND
            } else {
                libc::O_TRUNC
            };
        let fd = self.open_in(path, flags, if mode == 0 { 0o644 } else { mode & 0o7777 })?;
        let mut off = 0;
        while off < data.len() {
            // SAFETY: writing the unwritten tail of `data`.
            let n = unsafe {
                libc::write(
                    fd.as_raw_fd(),
                    data[off..].as_ptr().cast(),
                    data.len() - off,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(e);
            }
            off += n as usize;
        }
        Ok(ShimResp::Written {
            bytes: data.len() as u64,
        })
    }

    fn list(&self, path: &str) -> io::Result<ShimResp> {
        let fd = self.open_in(path, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        // SAFETY: duplicating a descriptor we own; fdopendir takes ownership
        // of the duplicate and closedir releases it.
        let dup = unsafe { libc::dup(fd.as_raw_fd()) };
        let dir = unsafe { libc::fdopendir(dup) };
        if dir.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut out = Vec::new();
        loop {
            // SAFETY: `dir` is a valid DIR*; readdir returns null at the end.
            let ent = unsafe { libc::readdir(dir) };
            if ent.is_null() || out.len() >= 2000 {
                break;
            }
            // SAFETY: d_name is a NUL-terminated string inside the dirent.
            let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            // SAFETY: an all-zero stat is a valid out-parameter.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: `fd` is an open directory; `name` is one live component.
            unsafe {
                libc::fstatat(
                    fd.as_raw_fd(),
                    name.as_ptr(),
                    &mut st,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            let kind = match st.st_mode & libc::S_IFMT {
                libc::S_IFREG => b'f',
                libc::S_IFDIR => b'd',
                libc::S_IFLNK => b'l',
                _ => b'o',
            };
            out.push(DirEnt {
                name: String::from_utf8_lossy(bytes).into_owned(),
                kind,
                size: st.st_size as u64,
            });
        }
        // SAFETY: `dir` came from fdopendir and is closed exactly once.
        unsafe { libc::closedir(dir) };
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ShimResp::Listing(out))
    }

    fn spawn_background(&mut self, spec: ExecSpec, keep: Option<OwnedFd>) {
        // SAFETY: the shim is single-threaded; the child branch never returns.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return self.send(&ShimResp::Err(SfError::task(
                "fork",
                io::Error::last_os_error().to_string(),
            )));
        }
        if pid == 0 {
            self.exec_child(&spec, -1, -1, -1, keep.as_ref().map(|k| k.as_raw_fd()));
        }
        self.background.push(pid);
        self.send(&ShimResp::Spawned { pid });
    }

    /// One request to the stateful kernel: a length-prefixed code string out,
    /// a length-prefixed reply back.
    fn eval(&self, socket: &str, code: &str, timeout_ms: u64) -> io::Result<ShimResp> {
        use std::io::{Read, Write};
        let mut s = std::os::unix::net::UnixStream::connect(self.host_path(socket))?;
        let t = Some(std::time::Duration::from_millis(timeout_ms.max(1)));
        s.set_read_timeout(t)?;
        s.set_write_timeout(t)?;
        s.write_all(&(code.len() as u32).to_le_bytes())?;
        s.write_all(code.as_bytes())?;
        let mut hdr = [0u8; 5];
        s.read_exact(&mut hdr)?;
        let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        if len > 4 << 20 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "kernel reply too large",
            ));
        }
        let mut body = vec![0u8; len];
        s.read_exact(&mut body)?;
        Ok(ShimResp::Eval {
            ok: hdr[4] == 1,
            output: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    fn wait_path(&mut self, path: &str, timeout_ms: u64) -> ShimResp {
        let p = self.host_path(path);
        let deadline = monotonic_ns() + timeout_ms * 1_000_000;
        loop {
            if std::path::Path::new(&p).exists() {
                return ShimResp::Ok;
            }
            self.reap();
            if monotonic_ns() >= deadline {
                return ShimResp::Err(SfError::task(
                    "wait_path",
                    format!("{path} did not appear within {timeout_ms} ms"),
                ));
            }
            // Wake on SIGCHLD (a dead warm process will never create the
            // path) or after a short interval.
            let mut pfd = libc::pollfd {
                fd: self.sigfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `pfd` is one valid pollfd.
            unsafe { libc::poll(&mut pfd, 1, 2) };
            if pfd.revents & libc::POLLIN != 0 {
                self.drain_signals();
            }
        }
    }

    fn drain_signals(&mut self) {
        loop {
            // SAFETY: an all-zero signalfd_siginfo is a valid read buffer.
            let mut si: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            // SAFETY: reading one signalfd_siginfo into a buffer of that size.
            let n = unsafe {
                libc::read(
                    self.sigfd.as_raw_fd(),
                    (&mut si as *mut libc::signalfd_siginfo).cast(),
                    std::mem::size_of::<libc::signalfd_siginfo>(),
                )
            };
            if n as usize != std::mem::size_of::<libc::signalfd_siginfo>() {
                break;
            }
            // A sender PID of 0 means the signal came from outside this PID
            // namespace: the spawner or the daemon. SIGTERM from a process
            // inside the sandbox is ignored, like SIGINT and SIGHUP.
            if si.ssi_signo == libc::SIGTERM as u32 && (!self.cfg.namespaced || si.ssi_pid == 0) {
                self.shutdown(0);
            }
        }
        self.reap();
    }

    fn handle(&mut self, req: ShimReq, mut fds: Vec<OwnedFd>) {
        let busy = self.running.is_some();
        match req {
            ShimReq::Ping => self.send(&ShimResp::Pong {
                uptime_ms: (monotonic_ns() - self.started) / 1_000_000,
                reaped: self.reaped,
            }),
            ShimReq::Cancel => self.kill_running(false),
            ShimReq::Shutdown => self.shutdown(0),
            _ if busy => self.send(&ShimResp::Err(SfError::client(
                "busy",
                "a command is already running in this sandbox",
            ))),
            ShimReq::Exec(spec) => self.start_exec(spec),
            ShimReq::Spawn(spec) => self.spawn_background(spec, None),
            ShimReq::StartZygote(spec) => match fds.pop() {
                Some(fd) => self.spawn_background(spec, Some(fd)),
                None => self.send(&ShimResp::Err(SfError::infra(
                    "zygote",
                    "StartZygote arrived without a descriptor",
                ))),
            },
            ShimReq::ReadFile {
                path,
                offset,
                max_len,
            } => {
                let r = self.as_agent(|s| s.read_file(&path, offset, max_len));
                self.send(&r.unwrap_or_else(|e| ShimResp::Err(io_err(e, &format!("read {path}")))));
            }
            ShimReq::WriteFile {
                path,
                data,
                mode,
                append,
            } => {
                let r = self.as_agent(|s| s.write_file(&path, &data, mode, append));
                self.send(
                    &r.unwrap_or_else(|e| ShimResp::Err(io_err(e, &format!("write {path}")))),
                );
            }
            ShimReq::List { path } => {
                let r = self.as_agent(|s| s.list(&path));
                self.send(&r.unwrap_or_else(|e| ShimResp::Err(io_err(e, &format!("list {path}")))));
            }
            ShimReq::Eval {
                socket,
                code,
                timeout_ms,
            } => {
                let r = self.eval(&socket, &code, timeout_ms);
                self.send(
                    &r.unwrap_or_else(|e| ShimResp::Err(SfError::task("eval", e.to_string()))),
                );
            }
            ShimReq::WaitPath { path, timeout_ms } => {
                let r = self.wait_path(&path, timeout_ms);
                self.send(&r);
            }
        }
    }

    fn run(&mut self) -> ! {
        loop {
            self.reap();
            self.maybe_finish();
            let now = monotonic_ns();
            let mut pfds = vec![
                libc::pollfd {
                    fd: self.ctl,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.sigfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let mut timeout = -1;
            if let Some(r) = &self.running {
                for s in [&r.out, &r.err] {
                    pfds.push(libc::pollfd {
                        fd: s.fd.as_ref().map_or(-1, |f| f.as_raw_fd()),
                        events: libc::POLLIN,
                        revents: 0,
                    });
                }
                timeout = if r.exit.is_some() {
                    10
                } else {
                    ((r.deadline.saturating_sub(now) / 1_000_000) as i32).clamp(0, 1000)
                };
            }
            // SAFETY: `pfds` is a valid array of the stated length.
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                continue; // EINTR
            }
            if pfds.len() > 2 {
                if pfds[2].revents != 0 {
                    self.pump(false);
                }
                if pfds[3].revents != 0 {
                    self.pump(true);
                }
            }
            if pfds[1].revents & libc::POLLIN != 0 {
                self.drain_signals();
            }
            if let Some(r) = &self.running {
                if r.exit.is_none() && monotonic_ns() >= r.deadline {
                    self.kill_running(true);
                }
            }
            if pfds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0
                && pfds[0].revents & libc::POLLIN == 0
            {
                self.shutdown(0);
            }
            if pfds[0].revents & libc::POLLIN != 0 {
                match recv_frame::<ShimReq>(self.ctl) {
                    Ok((req, fds)) => self.handle(req, fds),
                    Err(_) => self.shutdown(0),
                }
            }
        }
    }
}

/// Serve the control channel on `ctl` until the daemon goes away or asks for
/// shutdown. Never returns. The caller must be single-threaded.
pub fn run(ctl: RawFd, cfg: ShimCfg) -> ! {
    // SAFETY: signal setup on a single-threaded process. SIGCHLD and SIGTERM
    // are blocked and read through a signalfd, so they are handled in the
    // loop and never interrupt it. As PID 1, signals without a handler are
    // ignored by the kernel; SIGINT and SIGHUP are ignored explicitly so the
    // shim behaves the same at L0.
    let sigfd = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        for s in [libc::SIGINT, libc::SIGHUP, libc::SIGPIPE] {
            libc::signal(s, libc::SIG_IGN);
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        if fd < 0 {
            libc::_exit(70);
        }
        OwnedFd::from_raw_fd(fd)
    };
    let root = if cfg.root.is_empty() {
        "/".to_string()
    } else {
        cfg.root.clone()
    };
    let Ok(root_fd) = open_dir(&root) else {
        // SAFETY: exiting.
        unsafe { libc::_exit(71) }
    };
    let ws_fd = open_dir(&format!(
        "{}{}",
        root.trim_end_matches('/'),
        sandfork_core::paths::WORKSPACE
    ))
    .ok();
    let mut shim = Shim {
        cfg,
        ctl,
        sigfd,
        root_fd,
        ws_fd,
        started: monotonic_ns(),
        reaped: 0,
        running: None,
        background: Vec::new(),
    };
    shim.run()
}
