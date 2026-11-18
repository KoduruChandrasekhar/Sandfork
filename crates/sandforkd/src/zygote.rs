//! The zygote pool: one live prepared process per prepared state, forked
//! copy-on-write into every sandbox that starts from it (fork mode F3).
//!
//! * One zygote per prepared state per host, while it is being scheduled. It
//!   serves many groups, and every task with identical prepared layers and
//!   warm process.
//! * A zygote is retired after a fixed number of forks. Its children live in
//!   its PID namespace, so a retired zygote is only destroyed once its last
//!   child has been released.
//! * A zygote that has not been used for a few minutes is destroyed.
//!
//! cgroups charge a page to the cgroup that first faulted it in. The shared
//! pages therefore belong to the zygote's cgroup, and each child's cgroup
//! sees only its private pages: a child's `memory.max` limits its private
//! growth, not the shared base.

use crate::admission::Priority;
use crate::lifecycle::{create_sandbox, destroy_sandbox, register, shell_spec, NewSbx};
use crate::state::{lock, CkptState, Daemon, Purpose, Sandbox, SbxState, ShimConn, TaskEntry};
use sandfork_core::wire::{
    recv_frame, send_frame, Bind, Flavor, ForkGo, ForkHello, Limits, ShimReq, ShimResp, ZygoteReq,
    ZygoteResp,
};
use sandfork_core::{ids, paths, Ctx, Result, SfError};
use sandfork_layers::base;
use sandfork_ns::outer::IdPlan;
use sandfork_proto::v1 as pb;
use sandfork_spawner::{sandbox_cfg, CfgParams};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Zygote {
    pub task_id: String,
    pub host: Arc<Sandbox>,
    chan: Arc<Mutex<OwnedFd>>,
    pub forks: AtomicU64,
    /// Sandboxes forked from this zygote that are still alive.
    pub live: AtomicU64,
    pub rss_kb: u64,
    pub last_used: Mutex<Instant>,
}

#[derive(Default)]
pub struct ZygotePool {
    pub active: HashMap<String, Arc<Zygote>>,
    /// Retired zygotes waiting for their last child to be released.
    pub retired: Vec<Arc<Zygote>>,
}

/// How long an unused zygote is kept.
const IDLE: Duration = Duration::from_secs(300);
const WAIT_MS: i32 = 60_000;

fn recv_blocking<T: serde::de::DeserializeOwned>(
    fd: i32,
    what: &str,
    timeout_ms: i32,
) -> Result<(T, Vec<OwnedFd>)> {
    if !sandfork_ns::sys::poll_readable(fd, timeout_ms) {
        return Err(SfError::infra(
            "zygote_timeout",
            format!("{what}: no reply within {timeout_ms} ms"),
        ));
    }
    recv_frame::<T>(fd).map_err(|e| SfError::infra("zygote", format!("{what}: {e}")))
}

async fn create(d: &Arc<Daemon>, entry: &Arc<TaskEntry>) -> Result<Arc<Zygote>> {
    let spec = &entry.task.spec;
    let limits = spec.limits.to_limits();
    let mem = d.mem_estimate(&spec.task.family, &limits);
    d.admission
        .acquire(Priority::New, 1, mem, Duration::from_secs(300))
        .await?;
    let state = d.store.root();
    let created = create_sandbox(
        d,
        NewSbx {
            group_id: format!("zygote-{}", ids::new_id("g")),
            task_id: spec.task.id.clone(),
            family: spec.task.family.clone(),
            snapshot: entry.snapshot.clone(),
            base: entry.base.clone(),
            limits,
            flavor: Flavor::Zygote,
            // The layer store and the rollout directory are visible only in
            // the zygote. Its children pivot away from both, so nothing an
            // agent runs can reach them.
            extra_binds: vec![
                Bind {
                    src: state.join("layers").to_string_lossy().into_owned(),
                    dst: format!("{}/layers", paths::STORE),
                    readonly: true,
                    recursive: true,
                },
                Bind {
                    src: state.join("rollouts").to_string_lossy().into_owned(),
                    dst: paths::ROLLOUTS.into(),
                    readonly: false,
                    recursive: true,
                },
            ],
            mode: pb::ForkMode::F3Zygote,
            purpose: Purpose::Zygote,
            mem_est_mb: mem,
            counted: true,
            pre_spawn: None,
        },
    )
    .await;
    let host = match created {
        Ok(h) => h,
        Err(e) => {
            d.admission.release(1, mem);
            return Err(e);
        }
    };
    let started = async {
        let (mine, theirs) = sandfork_core::fdpass::socketpair().ctx("zygote socketpair")?;
        let mut cmd = shell_spec(d, &host, &format!("exec {}", spec.prepare.warm_process.trim()), 0, true);
        cmd.env.push(("SANDFORK_ZYGOTE_FD".into(), "3".into()));
        cmd.env.push(("SANDFORK_ZYGOTE_UPPER".into(), format!("{}/{}/upper", paths::ROLLOUTS, host.id)));
        // Anything the zygote wrote would vanish for its children while its
        // memory still assumed it existed, so it must not even write bytecode.
        cmd.env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
        {
            let mut shim = host.shim.lock().await;
            shim.send_with_fd(&ShimReq::StartZygote(cmd), &theirs).await.ctx("send StartZygote")?;
            match shim.recv().await.ctx("StartZygote reply")? {
                ShimResp::Spawned { .. } => {}
                ShimResp::Err(e) => return Err(e),
                other => return Err(SfError::infra("zygote", format!("unexpected reply {other:?}"))),
            }
        }
        drop(theirs);
        tokio::task::spawn_blocking(move || {
            let (resp, _) = recv_blocking::<ZygoteResp>(mine.as_raw_fd(), "zygote start", 300_000)
                .map_err(|e| SfError::client("warm_process_failed", format!("the task's warm process exited or never called sandfork_zygote.serve(): {}", e.message)))?;
            match resp {
                ZygoteResp::Ready { rss_kb, .. } => Ok((mine, rss_kb)),
                ZygoteResp::HygieneFailed { check, detail } => Err(SfError::client("zygote_hygiene", format!("zygote refused to serve: check `{check}` failed: {detail}"))),
                other => Err(SfError::infra("zygote", format!("unexpected first message {other:?}"))),
            }
        })
        .await
        .map_err(|e| SfError::infra("task_aborted", e.to_string()))?
    }
    .await;
    match started {
        Ok((chan, rss_kb)) => {
            d.metrics.inc("zygotes_started_total", 1.0);
            Ok(Arc::new(Zygote {
                task_id: spec.task.id.clone(),
                host,
                chan: Arc::new(Mutex::new(chan)),
                forks: AtomicU64::new(0),
                live: AtomicU64::new(0),
                rss_kb,
                last_used: Mutex::new(Instant::now()),
            }))
        }
        Err(e) => {
            let group = host.group_id.clone();
            destroy_sandbox(d, host).await;
            if let Some(cg) = &d.cg {
                cg.remove_group(&group);
            }
            Err(e)
        }
    }
}

/// The zygote for a task, started on first use and replaced when retired.
async fn get(d: &Arc<Daemon>, entry: &Arc<TaskEntry>) -> Result<Arc<Zygote>> {
    let mut pool = d.zygotes.lock().await;
    // Tasks whose prepared state is identical (same layers, same warm
    // process) share one zygote: the 200 data-analysis questions over one
    // dataset pay for one loaded kernel, not 200.
    let key = pool_key(entry);
    let id = &key;
    if let Some(z) = pool.active.get(id).cloned() {
        if z.forks.load(Ordering::SeqCst) < d.cfg.zygote_max_forks
            && z.host.is_live()
            && !z.host.pid1_dead()
        {
            *lock(&z.last_used) = Instant::now();
            return Ok(z);
        }
        pool.active.remove(id);
        pool.retired.push(z);
    }
    let z = create(d, entry).await?;
    pool.active.insert(id.clone(), z.clone());
    Ok(z)
}

fn pool_key(entry: &TaskEntry) -> String {
    format!(
        "{}|{}",
        entry.snapshot.layers.join(","),
        entry.task.spec.prepare.warm_process.trim()
    )
}

async fn destroy(d: &Arc<Daemon>, z: Arc<Zygote>) {
    let group = z.host.group_id.clone();
    destroy_sandbox(d, z.host.clone()).await;
    if let Some(cg) = &d.cg {
        cg.remove_group(&group);
    }
    d.metrics.inc("zygotes_stopped_total", 1.0);
}

/// Destroy retired zygotes whose children are all gone, and zygotes of tasks
/// that have not been scheduled for a while.
pub async fn reap_idle(d: &Arc<Daemon>) {
    let mut doomed = Vec::new();
    {
        let mut pool = d.zygotes.lock().await;
        let (gone, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut pool.retired)
            .into_iter()
            .partition(|z| z.live.load(Ordering::SeqCst) == 0);
        pool.retired = keep;
        doomed.extend(gone);
        let idle: Vec<String> = pool
            .active
            .iter()
            .filter(|(_, z)| {
                z.live.load(Ordering::SeqCst) == 0
                    && (lock(&z.last_used).elapsed() > IDLE
                        || !z.host.is_live()
                        || z.host.pid1_dead())
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in idle {
            if let Some(z) = pool.active.remove(&k) {
                doomed.push(z);
            }
        }
    }
    for z in doomed {
        destroy(d, z).await;
    }
}

pub async fn stop_all(d: &Arc<Daemon>) {
    let all: Vec<Arc<Zygote>> = {
        let mut pool = d.zygotes.lock().await;
        let mut v: Vec<_> = pool.active.drain().map(|(_, z)| z).collect();
        v.append(&mut pool.retired);
        v
    };
    for z in all {
        destroy(d, z).await;
    }
}

/// The PID a pidfd refers to, as seen from this process's PID namespace.
fn pid_of(pidfd: &OwnedFd) -> Result<i32> {
    let text = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))
        .ctx("read pidfd fdinfo")?;
    text.lines()
        .find_map(|l| l.strip_prefix("Pid:"))
        .and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|p| *p > 0)
        .ok_or_else(|| SfError::infra("zygote", "pidfd has no live process"))
}

/// Fork one sandbox from the task's zygote.
pub async fn fork(
    d: &Arc<Daemon>,
    entry: &Arc<TaskEntry>,
    group_id: &str,
    limits: &Limits,
    mem: u64,
) -> Result<Arc<Sandbox>> {
    let z = get(d, entry).await?;
    let t0 = Instant::now();
    let id = ids::sandbox_id();
    let (d2, z2, e2, id2, lim2, gid2) = (
        d.clone(),
        z.clone(),
        entry.clone(),
        id.clone(),
        limits.clone(),
        group_id.to_string(),
    );
    let built = tokio::task::spawn_blocking(move || {
        let d = d2;
        // The tmpfs is mounted in the daemon's namespace and propagates into
        // the zygote's, where the child will use it as its writable layer.
        let root = d
            .backend
            .fork_root(&d.store, &e2.snapshot, &id2, lim2.disk_mb.max(1) << 20)?;
        let cg = match &d.cg {
            Some(c) => match c.create_sandbox(&gid2, &id2, &lim2) {
                Ok(cg) => Some(cg),
                Err(e) => {
                    let _ = d.backend.release(&d.store, &root);
                    return Err(e);
                }
            },
            None => None,
        };
        let r = (|| {
            // Paths are the zygote's view: the store at /.sandfork/store, the
            // rollout directory at /.sandfork/rollouts.
            let cfg = sandbox_cfg(CfgParams {
                id: &id2,
                level: d.cfg.level,
                flavor: Flavor::Normal,
                state_dir: Path::new("/.sandfork"),
                lowers: root
                    .layers
                    .iter()
                    .rev()
                    .map(|l| format!("store/{}", d.store.layer_rel(l)))
                    .collect(),
                rollout: format!("rollouts/{id2}"),
                xattr_user: d.store.cfg.xattr_ns == sandfork_layers::XattrNs::User,
                shim: Path::new(paths::SHIM),
                rt_dir: d.cfg.rt_dir.as_ref().map(|_| Path::new(paths::RT)),
                binds: base::effective_binds(&e2.base)
                    .into_iter()
                    .map(|b| Bind {
                        src: b.clone(),
                        dst: b,
                        readonly: true,
                        recursive: true,
                    })
                    .collect(),
                limits: lim2.clone(),
                cgroup: None,
                // The child's user namespace is nested in the zygote's and
                // maps the same IDs one to one.
                plan: IdPlan {
                    base: 0,
                    count: d.plan.count,
                },
                seccomp: true,
                env: vec![],
                l0_root: None,
            });
            let (ctl, theirs) = sandfork_core::fdpass::socketpair().ctx("fork socketpair")?;
            {
                let chan = lock(&z2.chan);
                send_frame(
                    chan.as_raw_fd(),
                    &ZygoteReq::Fork(Box::new(cfg)),
                    &[theirs.as_raw_fd()],
                )
                .map_err(|e| SfError::infra("zygote_dead", format!("send fork request: {e}")))?;
                match recv_blocking::<ZygoteResp>(chan.as_raw_fd(), "fork reply", WAIT_MS)?.0 {
                    ZygoteResp::Forked { ok: true, .. } => {}
                    ZygoteResp::Forked { detail, .. } => {
                        return Err(SfError::infra("zygote_fork", detail))
                    }
                    other => {
                        return Err(SfError::infra(
                            "zygote",
                            format!("unexpected fork reply {other:?}"),
                        ))
                    }
                }
            }
            drop(theirs);
            let (hello, mut fds) =
                recv_blocking::<ForkHello>(ctl.as_raw_fd(), "child hello", WAIT_MS)?;
            let steps = match hello {
                ForkHello::Hello { steps } => steps,
                ForkHello::Failed(e) => {
                    return Err(SfError::infra(
                        &e.code,
                        format!("zygote child setup failed: {}", e.message),
                    ))
                }
            };
            let pidfd = fds
                .pop()
                .ok_or_else(|| SfError::infra("zygote", "child hello arrived without a pidfd"))?;
            // Move the child into its own cgroup before it forks the kernel
            // process, so that process starts inside the leaf with limits set.
            if let Some(cg) = &cg {
                cg.add_pid(pid_of(&pidfd)?)?;
            }
            send_frame(ctl.as_raw_fd(), &ForkGo, &[])
                .map_err(|e| SfError::infra("zygote", format!("send go: {e}")))?;
            Ok((ctl, pidfd, steps))
        })();
        match r {
            Ok((ctl, pidfd, steps)) => Ok((root, cg, ctl, pidfd, steps)),
            Err(e) => {
                if let Some(c) = &cg {
                    let _ = c.kill(5000);
                    let _ = c.remove();
                }
                let _ = d.backend.release(&d.store, &root);
                Err(e)
            }
        }
    })
    .await
    .map_err(|e| SfError::infra("task_aborted", e.to_string()))?;
    let (root, cgroup, ctl, pidfd, steps) = built?;

    z.forks.fetch_add(1, Ordering::SeqCst);
    z.live.fetch_add(1, Ordering::SeqCst);
    d.metrics.inc("zygote_forks_total", 1.0);
    let spec = &entry.task.spec;
    let env = crate::lifecycle::sandbox_env(d, &entry.base, &root);
    let sbx = Arc::new(Sandbox {
        id,
        group_id: group_id.to_string(),
        task_id: spec.task.id.clone(),
        family: spec.task.family.clone(),
        mode: pb::ForkMode::F3Zygote,
        purpose: Purpose::Rollout,
        root,
        snapshot: entry.snapshot.clone(),
        base: entry.base.clone(),
        shim: tokio::sync::Mutex::new(ShimConn::from_fd(ctl).ctx("wrap control socket")?),
        pidfd,
        cgroup,
        limits: limits.clone(),
        mem_est_mb: mem,
        counted: true,
        created: Instant::now(),
        state: Mutex::new(SbxState::Live),
        ckpt: tokio::sync::Mutex::new(CkptState::default()),
        execs: AtomicU64::new(0),
        max_rss_kb: AtomicU64::new(0),
        exec_cache: Mutex::new(HashMap::new()),
        env,
        fork_micros: AtomicU64::new(0),
        steps,
        zygote: Some(z),
        has_kernel: AtomicBool::new(false),
    });
    let sbx = register(d, sbx, t0).await?;
    // The forked kernel announces itself by creating its socket.
    if let Err(e) = sbx
        .call(
            ShimReq::WaitPath {
                path: paths::KERNEL_SOCK.into(),
                timeout_ms: 30_000,
            },
            Duration::from_secs(40),
        )
        .await
    {
        let log = crate::lifecycle::kernel_log_remote(&sbx).await;
        destroy_sandbox(d, sbx).await;
        return Err(SfError::infra(
            "zygote_child",
            format!("forked kernel never became ready: {}{log}", e.message),
        ));
    }
    sbx.has_kernel.store(true, Ordering::SeqCst);
    Ok(sbx)
}
