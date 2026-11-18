//! The control protocol shared by the daemon, spawner, shim and zygote.
//!
//! A frame is a little-endian `u32` body length followed by a `postcard`
//! body. Frames are capped at [`MAX_FRAME`]; larger payloads (file contents,
//! command output) are chunked by the sender.
//!
//! Trust boundaries:
//! * daemon -> spawner / shim / zygote: the daemon is trusted by all three.
//! * shim -> daemon: the shim runs as PID 1 *inside* a sandbox. The daemon
//!   treats everything it receives as untrusted data: lengths are bounded here
//!   and nothing received is ever used as a host path.

use crate::{Level, SfError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io;
use std::os::fd::{OwnedFd, RawFd};

pub const MAX_FRAME: usize = 16 << 20;
/// Largest data chunk carried by one frame.
pub const CHUNK: usize = 256 << 10;

pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let body = postcard::to_allocvec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    postcard::from_bytes(body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Send one frame, attaching `fds` to its first byte.
pub fn send_frame<T: Serialize>(fd: RawFd, msg: &T, fds: &[RawFd]) -> io::Result<()> {
    let buf = encode(msg)?;
    let mut off = crate::fdpass::send_with_fds(fd, &buf, fds)?;
    while off < buf.len() {
        let n = crate::fdpass::send_with_fds(fd, &buf[off..], &[])?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        off += n;
    }
    Ok(())
}

fn recv_exact(fd: RawFd, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let (n, mut got) = crate::fdpass::recv_with_fds(fd, &mut buf[off..])?;
        fds.append(&mut got);
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        off += n;
    }
    Ok(())
}

/// Receive one frame and any descriptors that arrived with it.
pub fn recv_frame<T: DeserializeOwned>(fd: RawFd) -> io::Result<(T, Vec<OwnedFd>)> {
    let mut fds = Vec::new();
    let mut hdr = [0u8; 4];
    recv_exact(fd, &mut hdr, &mut fds)?;
    let len = u32::from_le_bytes(hdr) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    recv_exact(fd, &mut body, &mut fds)?;
    Ok((decode(&body)?, fds))
}

// ---------------------------------------------------------------- sandbox configuration

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Limits {
    pub memory_mb: u64,
    pub pids: u64,
    /// Cores. 1.0 means `cpu.max = 100000 100000`.
    pub cpu: f64,
    /// Size of the rollout's writable layer.
    pub disk_mb: u64,
    pub nofile: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memory_mb: 1024,
            pids: 256,
            cpu: 1.0,
            disk_mb: 512,
            nofile: 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bind {
    pub src: String,
    pub dst: String,
    pub readonly: bool,
    pub recursive: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Flavor {
    /// An agent sandbox: reduced capabilities and the seccomp filter.
    Normal,
    /// A zygote host: keeps capabilities and has no seccomp filter, because
    /// filters are inherited and can never be removed. It runs task-author
    /// code only, never agent code.
    Zygote,
}

/// Everything the spawner (or a zygote) needs to build one sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SandboxCfg {
    pub id: String,
    pub level: Level,
    pub flavor: Flavor,
    /// Directory that relative `lowers` are resolved against (keeps the mount
    /// option string short).
    pub layers_dir: String,
    /// Lower layers, top first, relative to `layers_dir`.
    pub lowers: Vec<String>,
    pub upper: String,
    pub work: String,
    /// Mount point for the overlay; becomes `/` after `pivot_root`.
    pub rootfs: String,
    /// `userxattr` (rootless) or `trusted.overlay.*` (real root).
    pub xattr_user: bool,
    pub volatile: bool,
    pub shim: String,
    /// Host directory bound read-only at `/.sandfork/rt`; empty for none.
    pub rt_dir: String,
    pub binds: Vec<Bind>,
    pub hostname: String,
    pub tmp_bytes: u64,
    pub limits: Limits,
    /// Absolute path of this sandbox's cgroup leaf; empty for none.
    pub cgroup: String,
    /// Contents written to `uid_map` / `gid_map`, e.g. `0 1 65536`.
    pub uid_map: String,
    pub gid_map: String,
    pub agent_uid: u32,
    pub agent_gid: u32,
    pub seccomp: bool,
    pub env: Vec<(String, String)>,
    /// L0 only: the directory that plays the role of the sandbox root.
    pub l0_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub name: String,
    pub ns: u64,
}

// ---------------------------------------------------------------- daemon <-> spawner

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpawnReq {
    Ping,
    Spawn(Box<SandboxCfg>),
    Shutdown,
}

/// A successful `Spawned` reply carries two descriptors: the control socket
/// and a pidfd for the sandbox's PID 1, in that order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpawnResp {
    Pong { threads: u32 },
    Spawned { pid: i32, steps: Vec<Step> },
    Err(SfError),
}

// ---------------------------------------------------------------- daemon <-> shim

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub timeout_ms: u64,
    /// Bytes of each stream that are forwarded before truncation starts.
    pub max_output: u32,
    /// Run as the sandbox's root instead of the agent user. Used only for
    /// preparation steps written by task authors.
    pub as_root: bool,
    pub stdin: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ExecExit {
    pub code: i32,
    pub signal: i32,
    pub timed_out: bool,
    pub cancelled: bool,
    pub wall_ms: u64,
    pub user_ms: u64,
    pub sys_ms: u64,
    pub max_rss_kb: u64,
    pub truncated: bool,
    /// Bytes that were neither in the forwarded head nor in the kept tail.
    pub dropped_bytes: u64,
    pub tail_stdout: Vec<u8>,
    pub tail_stderr: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirEnt {
    pub name: String,
    pub kind: u8, // b'f' file, b'd' directory, b'l' symlink, b'o' other
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShimReq {
    Ping,
    Exec(ExecSpec),
    /// Kill the running command's process group.
    Cancel,
    ReadFile {
        path: String,
        offset: u64,
        max_len: u32,
    },
    WriteFile {
        path: String,
        data: Vec<u8>,
        mode: u32,
        append: bool,
    },
    List {
        path: String,
    },
    /// Start a detached background process (a cold warm_process).
    Spawn(ExecSpec),
    /// Send `code` to the stateful kernel listening on `socket`.
    Eval {
        socket: String,
        code: String,
        timeout_ms: u64,
    },
    /// Start the task's warm process as a zygote. One descriptor is attached:
    /// the zygote's channel to the daemon.
    StartZygote(ExecSpec),
    /// Block until `path` exists or the timeout passes.
    WaitPath {
        path: String,
        timeout_ms: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShimResp {
    Pong { uptime_ms: u64, reaped: u64 },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(ExecExit),
    File { data: Vec<u8>, eof: bool, size: u64 },
    Written { bytes: u64 },
    Listing(Vec<DirEnt>),
    Spawned { pid: i32 },
    Eval { ok: bool, output: String },
    Ok,
    Err(SfError),
}

// ---------------------------------------------------------------- daemon <-> zygote

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ZygoteReq {
    Ping,
    /// Fork one sandbox. One descriptor is attached: that sandbox's control socket.
    Fork(Box<SandboxCfg>),
    Exit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ZygoteResp {
    /// Sent once, after the hygiene checks pass.
    Ready {
        rss_kb: u64,
        pid: i32,
    },
    /// Sent instead of `Ready`; names the failed check.
    HygieneFailed {
        check: String,
        detail: String,
    },
    Pong {
        forks: u64,
        rss_kb: u64,
    },
    Forked {
        ok: bool,
        detail: String,
    },
}

/// First message a forked sandbox sends on its control socket. A pidfd for the
/// sandbox's PID 1 is attached, so the daemon gets a race-free handle without
/// translating PIDs between namespaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ForkHello {
    Hello { steps: Vec<Step> },
    Failed(SfError),
}

/// The daemon's answer to [`ForkHello`], sent after it has moved the sandbox
/// into its cgroup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkGo;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn frame_roundtrip_with_fd() {
        let (a, b) = crate::fdpass::socketpair().unwrap();
        let (r, _w) = crate::fdpass::pipe().unwrap();
        let req = ShimReq::Exec(ExecSpec {
            argv: vec!["true".into()],
            timeout_ms: 5,
            ..Default::default()
        });
        send_frame(a.as_raw_fd(), &req, &[r.as_raw_fd()]).unwrap();
        let (got, fds): (ShimReq, _) = recv_frame(b.as_raw_fd()).unwrap();
        assert_eq!(fds.len(), 1);
        match got {
            ShimReq::Exec(s) => assert_eq!(s.argv, vec!["true".to_string()]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let (a, b) = crate::fdpass::socketpair().unwrap();
        let hdr = ((MAX_FRAME + 1) as u32).to_le_bytes();
        crate::fdpass::send_with_fds(a.as_raw_fd(), &hdr, &[]).unwrap();
        let r: io::Result<(ShimReq, _)> = recv_frame(b.as_raw_fd());
        assert!(r.is_err());
    }

    #[test]
    fn truncated_frame_is_an_error() {
        let (a, b) = crate::fdpass::socketpair().unwrap();
        crate::fdpass::send_with_fds(a.as_raw_fd(), &[10, 0, 0, 0, 1, 2], &[]).unwrap();
        drop(a);
        let r: io::Result<(ShimReq, _)> = recv_frame(b.as_raw_fd());
        assert!(r.is_err());
    }
}
