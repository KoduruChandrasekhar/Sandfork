//! Creating and destroying sandboxes and groups.
//!
//! Teardown order, which is not negotiable:
//!
//! 1. Kill: `SIGTERM` then `SIGKILL` to PID 1 through its pidfd, and
//!    `cgroup.kill` where there is a cgroup.
//! 2. Wait until PID 1 is gone (the pidfd becomes readable) and the cgroup
//!    reports `populated 0`. Poll; never sleep and hope.
//! 3. Read the cgroup's statistics before it disappears.
//! 4. Remove the writable layer. Doing this before the processes are gone
//!    gives `EBUSY` at best and stray writes at worst.
//! 5. Remove the cgroup directory.
//! 6. Return the sandbox's share to the admission controller.

use crate::admission::Priority;
use crate::state::{
    lock, log, CkptState, Daemon, Group, Purpose, Sandbox, SbxState, ShimConn, TaskEntry,
};
use sandfork_core::task::BaseSpec;
use sandfork_core::wire::{Bind, ExecSpec, Flavor, Limits, ShimReq, ShimResp};
use sandfork_core::{ids, paths, Ctx, Result, SfError};
use sandfork_layers::tree::{Copier, Owner};
use sandfork_layers::{base, RootHandle, Snapshot};
use sandfork_proto::v1 as pb;
use sandfork_spawner::{sandbox_cfg, CfgParams};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub type PreSpawn = Box<dyn FnOnce(&RootHandle) -> Result<()> + Send>;

pub struct NewSbx {
    pub group_id: String,
    pub task_id: String,
    pub family: String,
    pub snapshot: Snapshot,
    pub base: Arc<BaseSpec>,
    pub limits: Limits,
    pub flavor: Flavor,
    pub extra_binds: Vec<Bind>,
    pub mode: pb::ForkMode,
    pub purpose: Purpose,
    pub mem_est_mb: u64,
    /// Whether admission was charged for this sandbox.
    pub counted: bool,
    /// Runs after the writable layer exists and before the sandbox starts.
    pub pre_spawn: Option<PreSpawn>,
}

fn set_env(env: &mut Vec<(String, String)>, k: &str, v: String) {
    match env.iter_mut().find(|(ek, _)| ek == k) {
        Some(e) => e.1 = v,
        None => env.push((k.to_string(), v)),
    }
}

/// The environment every command in a sandbox starts from.
pub fn sandbox_env(d: &Daemon, base: &BaseSpec, root: &RootHandle) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = [
        ("PATH", "/opt/venv/bin:/usr/local/bin:/usr/bin:/bin"),
        ("HOME", "/home/agent"),
        ("LANG", "C.UTF-8"),
        ("PYTHONUNBUFFERED", "1"),
        // One thread per numeric library: a thread pool that exists at fork
        // time is how zygote children deadlock on an inherited mutex.
        ("OMP_NUM_THREADS", "1"),
        ("OPENBLAS_NUM_THREADS", "1"),
        ("MKL_NUM_THREADS", "1"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    if d.cfg.rt_dir.is_some() {
        set_env(&mut env, "PYTHONPATH", paths::RT.to_string());
    }
    for (k, v) in &base.env {
        set_env(&mut env, k, v.clone());
    }
    if !d.namespaced() {
        let r = root.rootfs.to_string_lossy().into_owned();
        let rt = d
            .cfg
            .rt_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        for e in env.iter_mut() {
            e.1 = crate::state::l0_rewrite(&e.1, &r, &rt);
        }
        set_env(&mut env, "HOME", format!("{r}/home/agent"));
        set_env(&mut env, "TMPDIR", format!("{r}/tmp"));
    }
    env
}

/// Build a shell command for a sandbox, rewriting paths at L0.
pub fn shell_spec(
    d: &Daemon,
    sbx: &Sandbox,
    cmd: &str,
    timeout_ms: u64,
    as_root: bool,
) -> ExecSpec {
    let cmd = if d.namespaced() {
        cmd.to_string()
    } else {
        let rt = d
            .cfg
            .rt_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        crate::state::l0_rewrite(cmd, &sbx.root.rootfs.to_string_lossy(), &rt)
    };
    ExecSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), cmd],
        env: sbx.env.clone(),
        cwd: paths::WORKSPACE.into(),
        timeout_ms,
        max_output: 256 << 10,
        as_root,
        stdin: vec![],
    }
}

fn tail(bytes: &[u8], n: usize) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(n)..]).into_owned()
}

/// Run preparation steps; a failing step is the task author's error.
pub async fn run_steps(
    d: &Daemon,
    sbx: &Sandbox,
    steps: &[String],
    as_root: bool,
    what: &str,
) -> Result<()> {
    for step in steps {
        let (exit, out, err) = sbx
            .exec_collect(shell_spec(d, sbx, step, 900_000, as_root))
            .await?;
        if exit.code != 0 || exit.timed_out {
            return Err(SfError::client(
                "prepare_step_failed",
                format!(
                    "{what}: step `{step}` exited with {} (timed out: {}):\n{}{}",
                    exit.code,
                    exit.timed_out,
                    tail(&out, 2000),
                    tail(&err, 2000)
                ),
            ));
        }
    }
    Ok(())
}

pub async fn create_sandbox(d: &Arc<Daemon>, p: NewSbx) -> Result<Arc<Sandbox>> {
    let t0 = Instant::now();
    let id = ids::sandbox_id();
    let NewSbx {
        group_id,
        task_id,
        family,
        snapshot,
        base,
        limits,
        flavor,
        extra_binds,
        mode,
        purpose,
        mem_est_mb,
        counted,
        pre_spawn,
    } = p;
    let (d2, id2, snap2, base2, lim2, group2) = (
        d.clone(),
        id.clone(),
        snapshot.clone(),
        base.clone(),
        limits.clone(),
        group_id.clone(),
    );
    let built = tokio::task::spawn_blocking(move || {
        let d = d2;
        let root = d
            .backend
            .fork_root(&d.store, &snap2, &id2, lim2.disk_mb.max(1) << 20)?;
        let undo = |e: SfError, cg: Option<&sandfork_cgroup::Cgroup>| {
            if let Some(c) = cg {
                let _ = c.remove();
            }
            let _ = d.backend.release(&d.store, &root);
            e
        };
        if let Some(f) = pre_spawn {
            f(&root).map_err(|e| undo(e, None))?;
        }
        let cg = match &d.cg {
            Some(c) if d.namespaced() => Some(
                c.create_sandbox(&group2, &id2, &lim2)
                    .map_err(|e| undo(e, None))?,
            ),
            _ => None,
        };
        let mut binds: Vec<Bind> = if d.namespaced() {
            base::effective_binds(&base2)
                .into_iter()
                .map(|b| Bind {
                    src: b.clone(),
                    dst: b,
                    readonly: true,
                    recursive: true,
                })
                .collect()
        } else {
            vec![]
        };
        binds.extend(extra_binds);
        let cfg = sandbox_cfg(CfgParams {
            id: &id2,
            level: d.cfg.level,
            flavor,
            state_dir: d.store.root(),
            lowers: root
                .layers
                .iter()
                .rev()
                .map(|l| d.store.layer_rel(l))
                .collect(),
            rollout: format!("rollouts/{id2}"),
            xattr_user: d.store.cfg.xattr_ns == sandfork_layers::XattrNs::User,
            shim: &d.cfg.bin_dir.join("sandfork-shim"),
            rt_dir: d.cfg.rt_dir.as_deref(),
            binds,
            limits: lim2,
            cgroup: cg.as_ref().map(|c| c.path.as_path()),
            plan: d.plan,
            seccomp: flavor == Flavor::Normal,
            env: vec![],
            l0_root: (!d.namespaced()).then_some(root.rootfs.as_path()),
        });
        match d.spawner().spawn(&cfg) {
            Ok(s) => Ok((root, cg, s)),
            Err(e) => Err(undo(e, cg.as_ref())),
        }
    })
    .await
    .map_err(|e| SfError::infra("task_aborted", e.to_string()))?;
    let (root, cgroup, spawned) = built?;

    let env = sandbox_env(d, &base, &root);
    let shim = ShimConn::from_fd(spawned.ctl).ctx("wrap control socket")?;
    let sbx = Arc::new(Sandbox {
        id: id.clone(),
        group_id,
        task_id,
        family,
        mode,
        purpose,
        root,
        snapshot,
        base,
        shim: tokio::sync::Mutex::new(shim),
        pidfd: spawned.pidfd,
        cgroup,
        limits,
        mem_est_mb,
        counted,
        created: Instant::now(),
        state: Mutex::new(SbxState::Live),
        ckpt: tokio::sync::Mutex::new(CkptState::default()),
        execs: AtomicU64::new(0),
        max_rss_kb: AtomicU64::new(0),
        exec_cache: Mutex::new(HashMap::new()),
        env,
        fork_micros: AtomicU64::new(0),
        steps: spawned.steps,
        zygote: None,
        has_kernel: AtomicBool::new(false),
    });
    register(d, sbx, t0).await
}

/// Wait for the first command to be answered, then publish the sandbox.
pub async fn register(d: &Arc<Daemon>, sbx: Arc<Sandbox>, t0: Instant) -> Result<Arc<Sandbox>> {
    lock(&d.sandboxes).insert(sbx.id.clone(), sbx.clone());
    match sbx.call(ShimReq::Ping, Duration::from_secs(15)).await {
        Ok(ShimResp::Pong { .. }) => {}
        Ok(other) => {
            destroy_sandbox(d, sbx).await;
            return Err(SfError::infra(
                "spawn",
                format!("unexpected first reply from the shim: {other:?}"),
            ));
        }
        Err(e) => {
            destroy_sandbox(d, sbx).await;
            return Err(SfError::infra(
                "spawn",
                format!("sandbox never answered its first command: {}", e.message),
            ));
        }
    }
    sbx.fork_micros
        .store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    d.metrics.inc("sandboxes_created_total", 1.0);
    Ok(sbx)
}

fn teardown(d: &Daemon, sbx: &Sandbox) {
    let fd = sbx.pidfd.as_raw_fd();
    // From outside the sandbox's PID namespace a SIGTERM reaches the shim,
    // which kills everything it started. SIGKILL is the backstop.
    sandfork_ns::sys::pidfd_kill(fd, libc::SIGTERM);
    if !sandfork_ns::sys::poll_readable(fd, 2000) {
        sandfork_ns::sys::pidfd_kill(fd, libc::SIGKILL);
    }
    if let Some(cg) = &sbx.cgroup {
        if let Err(e) = cg.kill(10_000) {
            log(
                "error",
                "cgroup kill failed",
                &[("sandbox", sbx.id.clone()), ("error", e.message)],
            );
            d.metrics.inc("teardown_errors_total", 1.0);
        }
    }
    if !sandfork_ns::sys::poll_readable(fd, 10_000) {
        log(
            "error",
            "sandbox PID 1 did not exit",
            &[("sandbox", sbx.id.clone())],
        );
        d.metrics.inc("teardown_errors_total", 1.0);
    }
    let mut peak_mb = sbx.max_rss_kb.load(Ordering::Relaxed) / 1024;
    if let Some(cg) = &sbx.cgroup {
        let st = cg.stats();
        peak_mb = st.memory_peak >> 20;
        if st.oom_kills > 0 {
            d.metrics.inc("oom_kills_total", st.oom_kills as f64);
        }
        d.metrics
            .inc("sandbox_cpu_seconds_total", st.cpu_usec as f64 / 1e6);
    }
    if sbx.purpose == Purpose::Rollout {
        d.record_memory(&sbx.family, peak_mb);
    }
    if let Err(e) = d.backend.release(&d.store, &sbx.root) {
        log(
            "error",
            "release writable layer failed",
            &[("sandbox", sbx.id.clone()), ("error", e.message)],
        );
        d.metrics.inc("teardown_errors_total", 1.0);
    }
    if let Some(cg) = &sbx.cgroup {
        let _ = cg.remove();
    }
    if sbx.counted {
        d.admission.release(1, sbx.mem_est_mb);
    }
    if let Some(z) = &sbx.zygote {
        z.live.fetch_sub(1, Ordering::SeqCst);
    }
    d.metrics.inc("sandboxes_released_total", 1.0);
    d.metrics
        .inc("sandbox_seconds_total", sbx.created.elapsed().as_secs_f64());
}

/// Destroy a sandbox. Idempotent: a second call does nothing.
pub async fn destroy_sandbox(d: &Arc<Daemon>, sbx: Arc<Sandbox>) {
    if lock(&d.sandboxes).remove(&sbx.id).is_none() {
        return;
    }
    let d2 = d.clone();
    let _ = tokio::task::spawn_blocking(move || teardown(&d2, &sbx)).await;
}

pub async fn release_group(d: &Arc<Daemon>, group_id: &str) {
    let Some(group) = lock(&d.groups).remove(group_id) else {
        return;
    };
    let ids: Vec<String> = lock(&group.sandboxes).clone();
    let sandboxes: Vec<Arc<Sandbox>> = ids
        .iter()
        .filter_map(|id| lock(&d.sandboxes).get(id).cloned())
        .collect();
    let mut joins = Vec::new();
    for s in sandboxes {
        let d2 = d.clone();
        joins.push(tokio::spawn(async move { destroy_sandbox(&d2, s).await }));
    }
    for j in joins {
        let _ = j.await;
    }
    if let Some(cg) = &d.cg {
        cg.remove_group(group_id);
    }
    crate::zygote::reap_idle(d).await;
    d.metrics.inc("groups_released_total", 1.0);
}

/// The tail of the stateful process's log, for error messages. Best effort.
pub fn kernel_log_tail(d: &Daemon, sbx: &Sandbox) -> String {
    let path = if d.namespaced() {
        None
    } else {
        Some(sbx.root.rootfs.join("tmp/sandfork-kernel.log"))
    };
    if let Some(p) = path {
        let t = std::fs::read(p).unwrap_or_default();
        return String::from_utf8_lossy(&t[t.len().saturating_sub(2000)..]).into_owned();
    }
    String::new()
}

fn kernel_log_hint(d: &Daemon, sbx: &Sandbox) -> String {
    let t = kernel_log_tail(d, sbx);
    if t.is_empty() {
        String::new()
    } else {
        format!(
            "
--- kernel log ---
{t}"
        )
    }
}

/// Read the kernel log through the shim (namespaced sandboxes), for errors.
pub async fn kernel_log_remote(sbx: &Sandbox) -> String {
    match sbx
        .call(
            ShimReq::ReadFile {
                path: "/tmp/sandfork-kernel.log".into(),
                offset: 0,
                max_len: 1 << 20,
            },
            Duration::from_secs(5),
        )
        .await
    {
        Ok(ShimResp::File { data, .. }) => format!(
            "
--- kernel log ---
{}",
            String::from_utf8_lossy(&data[data.len().saturating_sub(2000)..])
        ),
        _ => String::new(),
    }
}

/// Start the task's warm process cold: the cost a zygote fork avoids.
pub async fn start_kernel_cold(d: &Daemon, sbx: &Sandbox, warm_process: &str) -> Result<()> {
    let mut spec = shell_spec(
        d,
        sbx,
        &format!("exec {warm_process} >>\"${{TMPDIR:-/tmp}}/sandfork-kernel.log\" 2>&1"),
        0,
        false,
    );
    spec.env
        .push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
    if !d.namespaced() {
        // No private root at L0: the kernel's socket lives under the sandbox's
        // own directory, where the shim will look for it.
        spec.env.push((
            "SANDFORK_KERNEL_SOCK".into(),
            format!("{}{}", sbx.root.rootfs.display(), paths::KERNEL_SOCK),
        ));
    }
    match sbx
        .call(ShimReq::Spawn(spec), Duration::from_secs(15))
        .await?
    {
        ShimResp::Spawned { .. } => {}
        other => {
            return Err(SfError::infra(
                "warm_process",
                format!("unexpected reply {other:?}"),
            ))
        }
    }
    if let Err(e) = sbx
        .call(
            ShimReq::WaitPath {
                path: paths::KERNEL_SOCK.into(),
                timeout_ms: 180_000,
            },
            Duration::from_secs(200),
        )
        .await
    {
        let log = if d.namespaced() {
            kernel_log_remote(sbx).await
        } else {
            kernel_log_hint(d, sbx)
        };
        return Err(SfError::client(
            "warm_process_failed",
            format!(
                "the task's warm process never became ready: {}{log}",
                e.message
            ),
        ));
    }
    sbx.has_kernel.store(true, Ordering::SeqCst);
    Ok(())
}

/// Copy a layer's `workspace/` into a fresh sandbox's writable layer: the
/// per-sandbox copy that cold (F1) preparation pays every time.
fn copy_workspace(d: &Daemon, layer: &str, root: &RootHandle) -> Result<()> {
    let src = d.store.layer_root(layer);
    let dst = if root.copy_base.is_some() {
        root.rootfs.clone()
    } else {
        root.upper.clone()
    };
    let owner = if d.namespaced() {
        Owner::Preserve
    } else {
        Owner::Skip
    };
    Copier::new(owner)
        .copy_tree(&src, &dst, &[])
        .ctx("copy task files")
}

/// Bring one sandbox of a group to a usable state in the given mode.
async fn start_one(
    d: &Arc<Daemon>,
    group_id: &str,
    entry: &Arc<TaskEntry>,
    snapshot: &Snapshot,
    mode: pb::ForkMode,
    limits: &Limits,
    mem: u64,
) -> Result<Arc<Sandbox>> {
    let t0 = Instant::now();
    let spec = &entry.task.spec;
    let new = |snapshot: Snapshot, pre_spawn: Option<PreSpawn>| NewSbx {
        group_id: group_id.to_string(),
        task_id: spec.task.id.clone(),
        family: spec.task.family.clone(),
        snapshot,
        base: entry.base.clone(),
        limits: limits.clone(),
        flavor: Flavor::Normal,
        extra_binds: vec![],
        mode,
        purpose: Purpose::Rollout,
        mem_est_mb: mem,
        counted: true,
        pre_spawn,
    };
    let warm = spec.prepare.warm_process.trim();
    let sbx = match mode {
        pb::ForkMode::F3Zygote => crate::zygote::fork(d, entry, group_id, limits, mem).await?,
        pb::ForkMode::F1Cold => {
            // Everything is copied and every preparation step runs again.
            let (d2, layer) = (d.clone(), entry.files_layer.clone());
            let pre: PreSpawn = Box::new(move |root| copy_workspace(&d2, &layer, root));
            let sbx = create_sandbox(d, new(entry.base_snapshot.clone(), Some(pre))).await?;
            let r = async {
                run_steps(d, &sbx, &spec.prepare.steps, false, &spec.task.id).await?;
                if !warm.is_empty() {
                    start_kernel_cold(d, &sbx, warm).await?;
                }
                Ok::<_, SfError>(())
            }
            .await;
            if let Err(e) = r {
                destroy_sandbox(d, sbx).await;
                return Err(e);
            }
            sbx
        }
        pb::ForkMode::F4Restore => {
            return Err(SfError::client("f4_unavailable", "F4 (restore fork) needs CRIU through sandfork-priv, which this daemon was started without; see docs/checkpoint.md"));
        }
        _ => {
            let sbx = create_sandbox(d, new(snapshot.clone(), None)).await?;
            if !warm.is_empty() {
                if let Err(e) = start_kernel_cold(d, &sbx, warm).await {
                    destroy_sandbox(d, sbx).await;
                    return Err(e);
                }
            }
            sbx
        }
    };
    sbx.fork_micros
        .store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    Ok(sbx)
}

pub fn sandbox_pb(d: &Daemon, s: &Sandbox) -> pb::Sandbox {
    pb::Sandbox {
        id: s.id.clone(),
        group_id: s.group_id.clone(),
        task_id: s.task_id.clone(),
        mode: s.mode as i32,
        level: d.cfg.level.as_str().to_string(),
        fork_micros: s.fork_micros.load(Ordering::Relaxed),
        steps: s
            .steps
            .iter()
            .map(|st| pb::StepTiming {
                name: st.name.clone(),
                micros: st.ns / 1000,
            })
            .collect(),
        snapshot_id: s.snapshot.id.clone(),
    }
}

pub struct GroupReq {
    pub entry: Arc<TaskEntry>,
    pub snapshot: Snapshot,
    pub n: u32,
    pub mode: pb::ForkMode,
    pub limits: Limits,
    pub priority: Priority,
    pub admission_deadline: Duration,
    pub lease: Duration,
    pub labels: HashMap<String, String>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Gang-admit and start a whole group: all `n` sandboxes or none.
pub async fn make_group(d: &Arc<Daemon>, req: GroupReq) -> Result<pb::Group> {
    if d.draining.load(Ordering::SeqCst) {
        return Err(SfError::infra(
            "draining",
            "the daemon is draining and admits no new groups",
        ));
    }
    let spec = &req.entry.task.spec;
    let warm = !spec.prepare.warm_process.trim().is_empty();
    let mode = match req.mode {
        pb::ForkMode::Auto if warm && d.namespaced() && d.cfg.rt_dir.is_some() => {
            pb::ForkMode::F3Zygote
        }
        pb::ForkMode::Auto => pb::ForkMode::F2Fs,
        pb::ForkMode::F3Zygote if !warm => {
            return Err(SfError::client(
                "bad_mode",
                "F3 needs a task with a warm_process",
            ))
        }
        pb::ForkMode::F3Zygote if !d.namespaced() => {
            return Err(SfError::client(
                "bad_mode",
                "F3 needs namespaces (level l1 or l2)",
            ))
        }
        m => m,
    };
    let mem = d.mem_estimate(&spec.task.family, &req.limits);
    let queued = d
        .admission
        .acquire(req.priority, req.n, mem, req.admission_deadline)
        .await?;
    d.metrics
        .inc("admission_wait_seconds_total", queued.as_secs_f64());
    d.metrics.inc("groups_admitted_total", 1.0);

    let group_id = ids::group_id();
    let t0 = Instant::now();
    let mut joins = Vec::new();
    for _ in 0..req.n {
        let (d2, gid, entry, snap, limits) = (
            d.clone(),
            group_id.clone(),
            req.entry.clone(),
            req.snapshot.clone(),
            req.limits.clone(),
        );
        joins.push(tokio::spawn(async move {
            start_one(&d2, &gid, &entry, &snap, mode, &limits, mem).await
        }));
    }
    let mut started = Vec::new();
    let mut failure: Option<SfError> = None;
    for j in joins {
        match j.await {
            Ok(Ok(s)) => started.push(s),
            Ok(Err(e)) => failure = Some(e),
            Err(e) => failure = Some(SfError::infra("task_aborted", e.to_string())),
        }
    }
    if let Some(e) = failure {
        // A partial group is useless: tear down what started and give back
        // the share of the sandboxes that never did.
        let missing = req.n - started.len() as u32;
        for s in started {
            destroy_sandbox(d, s).await;
        }
        d.admission.release(missing, mem);
        if let Some(cg) = &d.cg {
            cg.remove_group(&group_id);
        }
        d.metrics.inc("group_start_failures_total", 1.0);
        return Err(e);
    }
    let fork = t0.elapsed();
    let token = sandfork_core::rng::Rng::from_entropy().next_u64() | 1;
    let expires = now_ms() + req.lease.as_millis() as u64;
    let group = Arc::new(Group {
        id: group_id.clone(),
        task_id: spec.task.id.clone(),
        mode,
        sandboxes: Mutex::new(started.iter().map(|s| s.id.clone()).collect()),
        lease_expires_ms: AtomicU64::new(expires),
        token,
        verified: AtomicU32::new(0),
        labels: req.labels,
    });
    lock(&d.groups).insert(group_id.clone(), group);
    let _ = d.store.touch_snapshot(&req.entry.snapshot.id);
    Ok(pb::Group {
        id: group_id.clone(),
        sandboxes: started.iter().map(|s| sandbox_pb(d, s)).collect(),
        lease: Some(pb::Lease {
            group_id,
            expires_unix_ms: expires,
            token,
        }),
        queued_micros: queued.as_micros() as u64,
        fork_micros: fork.as_micros() as u64,
        mode: mode as i32,
        task_id: spec.task.id.clone(),
    })
}
