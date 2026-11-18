//! The daemon's in-memory state and the async side of the shim protocol.

use crate::admission::Admission;
use crate::scheduler::Scheduler;
use sandfork_cgroup::{Cgroup, CgroupRoot};
use sandfork_core::task::{BaseSpec, LoadedTask};
use sandfork_core::wire::{self, ExecExit, ExecSpec, Limits, ShimReq, ShimResp, Step};
use sandfork_core::{ErrorClass, Level, Result, SfError};
use sandfork_layers::{Manifest, RootHandle, Snapshot, SnapshotFs, Store};
use sandfork_ns::outer::IdPlan;
use sandfork_proto::v1 as pb;
use sandfork_spawner::SpawnerClient;
use std::collections::{BTreeMap, HashMap};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct Config {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    pub level: Level,
    pub wheels_dir: PathBuf,
    pub bin_dir: PathBuf,
    /// Bound read-only at `/.sandfork/rt`: the zygote library and the kernel.
    pub rt_dir: Option<PathBuf>,
    /// Memory set aside for sandboxes, in MB. Admission uses 85% of it.
    pub memory_budget_mb: u64,
    pub max_sandboxes: u32,
    pub spawners: usize,
    pub default_lease: Duration,
    /// Squash a sandbox's checkpoint chain once it is longer than this.
    pub squash_k: usize,
    pub gc_grace_s: u64,
    pub disk_budget_bytes: Option<u64>,
    /// Estimate for a sandbox's memory when there is neither history nor an
    /// enforced limit to go by.
    pub default_mem_mb: u64,
    /// Back writable layers with a per-sandbox tmpfs (a hard disk quota).
    pub rollout_tmpfs: bool,
    /// A zygote is retired after this many forks.
    pub zygote_max_forks: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Rollout,
    Verify,
    Prepare,
    Zygote,
}

impl Purpose {
    pub fn as_str(&self) -> &'static str {
        match self {
            Purpose::Rollout => "rollout",
            Purpose::Verify => "verify",
            Purpose::Prepare => "prepare",
            Purpose::Zygote => "zygote",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbxState {
    Live,
    /// PID 1 is gone. `oom` distinguishes the task's outcome (its code used
    /// too much memory) from an infrastructure failure.
    Dead {
        oom: bool,
    },
}

/// The daemon's end of a shim's control channel.
pub struct ShimConn {
    stream: tokio::net::UnixStream,
}

impl ShimConn {
    pub fn from_fd(fd: OwnedFd) -> std::io::Result<Self> {
        let s = std::os::unix::net::UnixStream::from(fd);
        s.set_nonblocking(true)?;
        Ok(ShimConn {
            stream: tokio::net::UnixStream::from_std(s)?,
        })
    }

    pub async fn send(&mut self, req: &ShimReq) -> std::io::Result<()> {
        self.stream.write_all(&wire::encode(req)?).await
    }

    /// Send a request with one descriptor attached to its first byte.
    pub async fn send_with_fd(&mut self, req: &ShimReq, fd: &OwnedFd) -> std::io::Result<()> {
        let buf = wire::encode(req)?;
        let mut off = 0;
        while off < buf.len() {
            self.stream.writable().await?;
            let fds: &[i32] = if off == 0 { &[fd.as_raw_fd()] } else { &[] };
            match self.stream.try_io(tokio::io::Interest::WRITABLE, || {
                sandfork_core::fdpass::send_with_fds(self.stream.as_raw_fd(), &buf[off..], fds)
            }) {
                Ok(n) => off += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Receive one frame. Everything from the shim is untrusted data: the
    /// length is bounded and the body is only ever decoded, never executed
    /// or used as a host path.
    pub async fn recv(&mut self) -> std::io::Result<ShimResp> {
        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr).await?;
        let len = u32::from_le_bytes(hdr) as usize;
        if len > wire::MAX_FRAME {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).await?;
        wire::decode(&body)
    }
}

/// One Exec call: its events so far, shared by the original caller and any
/// retry that attaches to it.
#[derive(Default)]
pub struct ExecEntry {
    pub events: Mutex<Vec<pb::ExecEvent>>,
    pub done: AtomicBool,
    pub error: Mutex<Option<SfError>>,
    pub notify: tokio::sync::Notify,
}

#[derive(Default)]
pub struct CkptState {
    /// Delta layers sealed since the fork, bottom to top.
    pub chain: Vec<String>,
    /// Manifest of the whole writable layer at the last seal.
    pub manifest: Option<Manifest>,
}

pub struct Sandbox {
    pub id: String,
    pub group_id: String,
    pub task_id: String,
    pub family: String,
    pub mode: pb::ForkMode,
    pub purpose: Purpose,
    pub root: RootHandle,
    pub snapshot: Snapshot,
    pub base: Arc<BaseSpec>,
    pub shim: tokio::sync::Mutex<ShimConn>,
    pub pidfd: OwnedFd,
    pub cgroup: Option<Cgroup>,
    pub limits: Limits,
    /// What admission was charged for this sandbox.
    pub mem_est_mb: u64,
    pub counted: bool,
    pub created: Instant,
    pub state: Mutex<SbxState>,
    pub ckpt: tokio::sync::Mutex<CkptState>,
    pub execs: AtomicU64,
    pub max_rss_kb: AtomicU64,
    pub exec_cache: Mutex<HashMap<String, Arc<ExecEntry>>>,
    pub env: Vec<(String, String)>,
    pub fork_micros: AtomicU64,
    pub steps: Vec<Step>,
    pub zygote: Option<Arc<crate::zygote::Zygote>>,
    /// True once the task's warm process is running in this sandbox.
    pub has_kernel: AtomicBool,
}

impl Sandbox {
    pub fn is_live(&self) -> bool {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) == SbxState::Live
    }

    pub fn state(&self) -> SbxState {
        *self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// True if PID 1 has exited (its pidfd is readable).
    pub fn pid1_dead(&self) -> bool {
        sandfork_ns::sys::poll_readable(self.pidfd.as_raw_fd(), 0)
    }

    /// Record that the sandbox died, classifying an out-of-memory kill as the
    /// task's outcome and anything else as an infrastructure failure.
    pub fn mark_dead(&self) -> SbxState {
        let oom = self
            .cgroup
            .as_ref()
            .is_some_and(|c| c.stats().oom_kills > 0);
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if *st == SbxState::Live {
            *st = SbxState::Dead { oom };
        }
        *st
    }

    fn dead_error(&self) -> SfError {
        match self.mark_dead() {
            SbxState::Dead { oom: true } => SfError::task(
                "oom",
                format!(
                    "sandbox {} was killed for exceeding its memory limit",
                    self.id
                ),
            ),
            _ => SfError::infra("sandbox_dead", format!("sandbox {} died", self.id)),
        }
    }

    /// One request, one reply.
    pub async fn call(&self, req: ShimReq, timeout: Duration) -> Result<ShimResp> {
        if !self.is_live() {
            return Err(self.dead_error());
        }
        let mut shim = self.shim.lock().await;
        let r = tokio::time::timeout(timeout, async {
            shim.send(&req).await?;
            shim.recv().await
        })
        .await;
        match r {
            Ok(Ok(ShimResp::Err(e))) => Err(e),
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err(self.dead_error()),
            Err(_) => {
                // A shim that misses its deadline is treated as dead; a late
                // reply would desynchronise the channel.
                self.mark_dead();
                Err(SfError::infra(
                    "shim_timeout",
                    format!(
                        "sandbox {} did not answer within {} ms",
                        self.id,
                        timeout.as_millis()
                    ),
                ))
            }
        }
    }

    /// Run a command to completion, collecting its output. Used for
    /// preparation steps and verification, never for streaming.
    pub async fn exec_collect(&self, spec: ExecSpec) -> Result<(ExecExit, Vec<u8>, Vec<u8>)> {
        if !self.is_live() {
            return Err(self.dead_error());
        }
        let limit = Duration::from_millis(spec.timeout_ms.max(1) + 15_000);
        let mut shim = self.shim.lock().await;
        let r = tokio::time::timeout(limit, async {
            shim.send(&ShimReq::Exec(spec)).await?;
            let (mut out, mut err) = (Vec::new(), Vec::new());
            loop {
                match shim.recv().await? {
                    ShimResp::Stdout(d) => out.extend(d),
                    ShimResp::Stderr(d) => err.extend(d),
                    ShimResp::Exit(e) => return Ok(Ok((e, out, err))),
                    ShimResp::Err(e) => return Ok(Err(e)),
                    other => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("unexpected shim reply {other:?}"),
                        ))
                    }
                }
            }
        })
        .await;
        self.execs.fetch_add(1, Ordering::Relaxed);
        match r {
            Ok(Ok(Ok((exit, out, err)))) => {
                self.max_rss_kb
                    .fetch_max(exit.max_rss_kb, Ordering::Relaxed);
                Ok((exit, out, err))
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(self.dead_error()),
            Err(_) => {
                self.mark_dead();
                Err(SfError::infra(
                    "shim_timeout",
                    format!("sandbox {} stopped answering during a command", self.id),
                ))
            }
        }
    }
}

pub struct Group {
    pub id: String,
    pub task_id: String,
    pub mode: pb::ForkMode,
    pub sandboxes: Mutex<Vec<String>>,
    pub lease_expires_ms: AtomicU64,
    pub token: u64,
    pub verified: AtomicU32,
    pub labels: HashMap<String, String>,
}

/// A task that has been prepared in this daemon run.
pub struct TaskEntry {
    pub task: LoadedTask,
    pub snapshot: Snapshot,
    pub base_snapshot: Snapshot,
    pub base: Arc<BaseSpec>,
    /// The layer holding the task's files, for cold (F1) preparation.
    pub files_layer: String,
}

#[derive(Default)]
pub struct Metrics {
    values: Mutex<BTreeMap<String, f64>>,
}

impl Metrics {
    pub fn inc(&self, name: &str, by: f64) {
        *self
            .values
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(name.to_string())
            .or_insert(0.0) += by;
    }
    pub fn set(&self, name: &str, v: f64) {
        self.values
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.to_string(), v);
    }
    pub fn snapshot(&self) -> BTreeMap<String, f64> {
        self.values
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

pub struct Daemon {
    pub cfg: Config,
    pub store: Arc<Store>,
    pub backend: Arc<dyn SnapshotFs>,
    pub spawners: RwLock<Vec<Arc<SpawnerClient>>>,
    pub next_spawner: AtomicUsize,
    pub cg: Option<CgroupRoot>,
    pub plan: IdPlan,
    pub sandboxes: Mutex<HashMap<String, Arc<Sandbox>>>,
    pub groups: Mutex<HashMap<String, Arc<Group>>>,
    pub tasks: Mutex<HashMap<String, Arc<TaskEntry>>>,
    pub admission: Admission,
    pub sched: Mutex<Scheduler>,
    pub idem: crate::idem::IdemCache,
    pub metrics: Metrics,
    pub zygotes: tokio::sync::Mutex<crate::zygote::ZygotePool>,
    /// Peak private memory per task family, most recent last (MB).
    pub mem_hist: Mutex<HashMap<String, Vec<u64>>>,
    pub draining: AtomicBool,
    /// Serialises preparation per key, so two requests for one task build once.
    pub prep_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Snapshots this daemon run has pinned.
    pub pinned: Mutex<std::collections::HashSet<String>>,
    pub started: Instant,
}

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Daemon {
    pub fn sandbox(&self, id: &str) -> Result<Arc<Sandbox>> {
        lock(&self.sandboxes)
            .get(id)
            .cloned()
            .ok_or_else(|| SfError::client("not_found", format!("sandbox {id} does not exist")))
    }

    pub fn group(&self, id: &str) -> Result<Arc<Group>> {
        lock(&self.groups)
            .get(id)
            .cloned()
            .ok_or_else(|| SfError::client("not_found", format!("group {id} does not exist")))
    }

    pub fn task(&self, id: &str) -> Result<Arc<TaskEntry>> {
        lock(&self.tasks).get(id).cloned().ok_or_else(|| {
            SfError::client(
                "task_not_prepared",
                format!(
                    "task {id} has not been prepared in this daemon run: call PrepareTask first"
                ),
            )
        })
    }

    pub fn spawner(&self) -> Arc<SpawnerClient> {
        let v = self.spawners.read().unwrap_or_else(|p| p.into_inner());
        v[self.next_spawner.fetch_add(1, Ordering::Relaxed) % v.len()].clone()
    }

    pub fn namespaced(&self) -> bool {
        self.cfg.level != Level::L0
    }

    /// Daemon-side owner of the sandbox's root user, if ownership is tracked.
    pub fn root_owner(&self) -> Option<(u32, u32)> {
        self.namespaced()
            .then(|| (self.plan.outer(0), self.plan.outer(0)))
    }

    pub fn agent_owner(&self) -> Option<(u32, u32)> {
        self.namespaced().then(|| {
            (
                self.plan.outer(self.plan.agent()),
                self.plan.outer(self.plan.agent()),
            )
        })
    }

    /// Estimated memory per sandbox for admission: the 95th percentile of
    /// peak private memory for the task family, from history; otherwise the
    /// task's limit when a cgroup enforces it, or a configured default.
    pub fn mem_estimate(&self, family: &str, limits: &Limits) -> u64 {
        let hist = lock(&self.mem_hist);
        if let Some(h) = hist.get(family) {
            if h.len() >= 8 {
                let mut v = h.clone();
                v.sort_unstable();
                return v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)].max(8);
            }
        }
        if self.cg.is_some() {
            limits.memory_mb
        } else {
            limits.memory_mb.min(self.cfg.default_mem_mb)
        }
    }

    pub fn record_memory(&self, family: &str, mb: u64) {
        if mb == 0 || family.is_empty() {
            return;
        }
        let mut hist = lock(&self.mem_hist);
        let h = hist.entry(family.to_string()).or_default();
        h.push(mb);
        if h.len() > 200 {
            h.remove(0);
        }
    }

    pub fn prep_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock(&self.prep_locks)
            .entry(key.to_string())
            .or_default()
            .clone()
    }
}

/// Map an internal error to a gRPC status. The class and code travel in
/// metadata so the client never has to guess.
pub fn status(e: SfError) -> tonic::Status {
    let code = match (e.class, e.code.as_str()) {
        (ErrorClass::Client, "not_found") => tonic::Code::NotFound,
        (ErrorClass::Client, _) => tonic::Code::InvalidArgument,
        (ErrorClass::Task, _) => tonic::Code::FailedPrecondition,
        (ErrorClass::Infra, "admission_deadline") => tonic::Code::ResourceExhausted,
        (ErrorClass::Infra, "draining") => tonic::Code::Unavailable,
        (ErrorClass::Infra, _) => tonic::Code::Internal,
    };
    let mut st = tonic::Status::new(code, e.message.clone());
    if let Ok(v) = e.class.as_str().parse() {
        st.metadata_mut().insert("sandfork-error-class", v);
    }
    if let Ok(v) = e.code.parse() {
        st.metadata_mut().insert("sandfork-error-code", v);
    }
    st
}

/// One JSON log line on stderr.
pub fn log(level: &str, msg: &str, fields: &[(&str, String)]) {
    let mut o = serde_json::Map::new();
    o.insert("ts".into(), serde_json::json!(sandfork_core::unix_time()));
    o.insert("level".into(), serde_json::json!(level));
    o.insert("msg".into(), serde_json::json!(msg));
    for (k, v) in fields {
        o.insert((*k).into(), serde_json::json!(v));
    }
    eprintln!("{}", serde_json::Value::Object(o));
}

/// Rewrite sandbox paths in a command for L0, where there is no private root:
/// `/workspace`, `/grader`, `/wheels` and `/opt/venv` live under the
/// sandbox's own directory.
pub fn l0_rewrite(s: &str, root: &str, rt: &str) -> String {
    const BEFORE: &[u8] = b" \t'\"=:;(|&<>";
    const AFTER: &[u8] = b"/ \t'\":;)|&<>";
    let maps: [(&str, String); 5] = [
        ("/workspace", format!("{root}/workspace")),
        ("/grader", format!("{root}/grader")),
        ("/wheels", format!("{root}/wheels")),
        ("/opt/venv", format!("{root}/opt/venv")),
        ("/.sandfork/rt", rt.to_string()),
    ];
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 64);
    let mut i = 0;
    'outer: while i < b.len() {
        if b[i] == b'/' && (i == 0 || BEFORE.contains(&b[i - 1])) {
            for (from, to) in &maps {
                if s[i..].starts_with(from) {
                    let end = i + from.len();
                    if end == b.len() || AFTER.contains(&b[end]) {
                        out.push_str(to);
                        i = end;
                        continue 'outer;
                    }
                }
            }
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l0_paths_are_rewritten_only_at_path_boundaries() {
        let r = |s: &str| l0_rewrite(s, "/r", "/rt");
        assert_eq!(
            r("python -m pytest -q -c /grader/pytest.ini /grader"),
            "python -m pytest -q -c /r/grader/pytest.ini /r/grader"
        );
        assert_eq!(r("cd /workspace && ls"), "cd /r/workspace && ls");
        assert_eq!(
            r("PATH=/opt/venv/bin:/usr/bin"),
            "PATH=/r/opt/venv/bin:/usr/bin"
        );
        assert_eq!(
            r("cat /workspacefoo /x/workspace"),
            "cat /workspacefoo /x/workspace"
        );
        assert_eq!(
            r("PYTHONPATH=/.sandfork/rt python"),
            "PYTHONPATH=/rt python"
        );
        assert_eq!(
            r("echo 'héllo' > /workspace/a"),
            "echo 'héllo' > /r/workspace/a"
        );
    }
}
