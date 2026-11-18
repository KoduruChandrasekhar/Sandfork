//! The daemon's handle on a `sandfork-spawner` process.
//!
//! Trust boundary: the daemon and the spawner trust each other completely.
//! The spawner exists for process-model reasons, not for privilege
//! separation: it is the one process that is allowed to fork sandboxes,
//! because it is the one process guaranteed to be single-threaded.

use sandfork_core::wire::{
    recv_frame, send_frame, Bind, Flavor, Limits, SandboxCfg, SpawnReq, SpawnResp, Step,
};
use sandfork_core::Level;
use sandfork_core::{Ctx, Result, SfError};
use sandfork_ns::outer::IdPlan;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::Mutex;

pub struct Spawned {
    pub pid: i32,
    /// The daemon's end of the shim's control channel.
    pub ctl: OwnedFd,
    /// A race-free handle on the sandbox's PID 1.
    pub pidfd: OwnedFd,
    pub steps: Vec<Step>,
}

pub struct SpawnerClient {
    sock: Mutex<OwnedFd>,
    child: Mutex<std::process::Child>,
}

impl SpawnerClient {
    /// Start a spawner process with its control socket on descriptor 3.
    pub fn start(binary: &Path) -> Result<SpawnerClient> {
        let (mine, theirs) = sandfork_core::fdpass::socketpair().ctx("socketpair for spawner")?;
        let fd = theirs.as_raw_fd();
        let mut cmd = std::process::Command::new(binary);
        // SAFETY: the closure runs between fork and exec and calls only
        // dup2, which is async-signal-safe. dup2 clears close-on-exec on the
        // target, so descriptor 3 survives the exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .ctx_with(|| format!("start {}", binary.display()))?;
        drop(theirs);
        let client = SpawnerClient {
            sock: Mutex::new(mine),
            child: Mutex::new(child),
        };
        let threads = client.ping()?;
        if threads != 1 {
            return Err(SfError::infra(
                "spawner",
                format!("spawner reports {threads} threads; it must have exactly one"),
            ));
        }
        Ok(client)
    }

    fn call(&self, req: &SpawnReq) -> Result<(SpawnResp, Vec<OwnedFd>)> {
        let sock = self.sock.lock().unwrap_or_else(|p| p.into_inner());
        send_frame(sock.as_raw_fd(), req, &[])
            .map_err(|e| SfError::infra("spawner_dead", format!("send to spawner: {e}")))?;
        recv_frame::<SpawnResp>(sock.as_raw_fd())
            .map_err(|e| SfError::infra("spawner_dead", format!("receive from spawner: {e}")))
    }

    /// Number of threads in the spawner; must be 1.
    pub fn ping(&self) -> Result<u32> {
        match self.call(&SpawnReq::Ping)?.0 {
            SpawnResp::Pong { threads } => Ok(threads),
            other => Err(SfError::infra(
                "spawner",
                format!("unexpected reply to ping: {other:?}"),
            )),
        }
    }

    /// Create one sandbox. Blocking; call from a blocking context.
    pub fn spawn(&self, cfg: &SandboxCfg) -> Result<Spawned> {
        let (resp, mut fds) = self.call(&SpawnReq::Spawn(Box::new(cfg.clone())))?;
        match resp {
            SpawnResp::Spawned { pid, steps } => {
                if fds.len() != 2 {
                    return Err(SfError::infra(
                        "spawner",
                        format!(
                            "expected 2 descriptors with a spawn reply, got {}",
                            fds.len()
                        ),
                    ));
                }
                let pidfd = fds
                    .pop()
                    .ok_or_else(|| SfError::infra("spawner", "missing pidfd"))?;
                let ctl = fds
                    .pop()
                    .ok_or_else(|| SfError::infra("spawner", "missing control socket"))?;
                Ok(Spawned {
                    pid,
                    ctl,
                    pidfd,
                    steps,
                })
            }
            SpawnResp::Err(e) => Err(e),
            other => Err(SfError::infra(
                "spawner",
                format!("unexpected reply to spawn: {other:?}"),
            )),
        }
    }

    pub fn is_alive(&self) -> bool {
        let mut c = self.child.lock().unwrap_or_else(|p| p.into_inner());
        matches!(c.try_wait(), Ok(None))
    }

    pub fn pid(&self) -> u32 {
        self.child.lock().unwrap_or_else(|p| p.into_inner()).id()
    }
}

impl Drop for SpawnerClient {
    fn drop(&mut self) {
        if let Ok(sock) = self.sock.lock() {
            let _ = send_frame(sock.as_raw_fd(), &SpawnReq::Shutdown, &[]);
        }
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Everything that varies between sandboxes, in the daemon's terms.
pub struct CfgParams<'a> {
    pub id: &'a str,
    pub level: Level,
    pub flavor: Flavor,
    /// The store root. Overlay paths are given relative to it.
    pub state_dir: &'a Path,
    /// Lower layers, top first, relative to `state_dir`.
    pub lowers: Vec<String>,
    /// The rollout directory, relative to `state_dir`.
    pub rollout: String,
    pub xattr_user: bool,
    pub shim: &'a Path,
    pub rt_dir: Option<&'a Path>,
    pub binds: Vec<Bind>,
    pub limits: Limits,
    pub cgroup: Option<&'a Path>,
    pub plan: IdPlan,
    pub seccomp: bool,
    pub env: Vec<(String, String)>,
    /// L0 only: the directory that plays the sandbox root.
    pub l0_root: Option<&'a Path>,
}

/// Build the configuration the spawner (or a zygote) needs for one sandbox.
pub fn sandbox_cfg(p: CfgParams) -> SandboxCfg {
    let s = |path: &Path| path.to_string_lossy().into_owned();
    SandboxCfg {
        id: p.id.to_string(),
        level: p.level,
        flavor: p.flavor,
        layers_dir: s(p.state_dir),
        lowers: p.lowers,
        upper: format!("{}/upper", p.rollout),
        work: format!("{}/work", p.rollout),
        rootfs: format!("{}/root", p.rollout),
        xattr_user: p.xattr_user,
        // Rollout layers are thrown away, so skip their fsync cost.
        volatile: true,
        shim: s(p.shim),
        rt_dir: p.rt_dir.map(s).unwrap_or_default(),
        binds: p.binds,
        hostname: "sandbox".to_string(),
        tmp_bytes: (p.limits.disk_mb.max(16)) << 20,
        cgroup: p.cgroup.map(s).unwrap_or_default(),
        uid_map: p.plan.map_line(),
        gid_map: p.plan.map_line(),
        agent_uid: p.plan.agent(),
        agent_gid: p.plan.agent(),
        seccomp: p.seccomp,
        env: p.env,
        l0_root: p.l0_root.map(s).unwrap_or_default(),
        limits: p.limits,
    }
}
