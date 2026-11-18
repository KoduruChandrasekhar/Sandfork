//! The gRPC service: thin handlers over the lifecycle, prepare, verify and
//! checkpoint logic. Every mutating handler runs through the idempotency
//! cache.

use crate::admission::Priority;
use crate::lifecycle::{destroy_sandbox, make_group, now_ms, release_group, GroupReq};
use crate::state::{lock, status, Daemon, ExecEntry, Sandbox, SbxState};
use sandfork_core::wire::{ExecSpec, Limits, ShimReq, ShimResp, CHUNK};
use sandfork_core::{paths, Result, SfError};
use sandfork_layers::{diff, tree, ChangeKind, CheckpointRow, Snapshot};
use sandfork_proto::v1 as pb;
use sandfork_proto::Sandfork;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

pub struct Service {
    pub d: Arc<Daemon>,
}

fn dur(d: Option<prost_types::Duration>, default: Duration) -> Duration {
    match d {
        Some(x) if x.seconds > 0 || x.nanos > 0 => {
            Duration::new(x.seconds.max(0) as u64, x.nanos.max(0) as u32)
        }
        _ => default,
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SfError::infra("task_aborted", e.to_string()))?
}

fn snapshot_pb(s: &Snapshot, micros: u64, cached: bool, bytes: u64) -> pb::Snapshot {
    pb::Snapshot {
        id: s.id.clone(),
        task_id: s.task.clone(),
        kind: s.kind.clone(),
        layers: s.layers.clone(),
        parent: s.parent.clone(),
        created_unix: s.created,
        prepare_micros: micros,
        cached,
        bytes_written: bytes,
        spec_hash: s.key.split(':').nth(1).unwrap_or_default().to_string(),
    }
}

/// The request's limits where set, the task's otherwise.
fn merge_limits(req: Option<pb::Limits>, task: Limits) -> Limits {
    let Some(r) = req else { return task };
    Limits {
        memory_mb: if r.memory_mb > 0 {
            r.memory_mb
        } else {
            task.memory_mb
        },
        pids: if r.pids > 0 { r.pids } else { task.pids },
        cpu: if r.cpu > 0.0 { r.cpu } else { task.cpu },
        disk_mb: if r.disk_mb > 0 {
            r.disk_mb
        } else {
            task.disk_mb
        },
        nofile: task.nofile,
    }
}

/// Seal what a sandbox has written since its last seal and record a snapshot
/// of its current state. Only between turns: holding the control channel
/// guarantees no agent command is running, and the cgroup freeze stops
/// background processes while the layer is copied.
pub async fn seal_turn(
    d: &Arc<Daemon>,
    sbx: &Arc<Sandbox>,
    kind: &str,
) -> Result<(Snapshot, u64, u64)> {
    let t0 = Instant::now();
    let mut ck = sbx.ckpt.lock().await;
    let _shim = sbx.shim.lock().await;
    let (d2, s2, prev) = (d.clone(), sbx.clone(), ck.manifest.take());
    let (delta, manifest) = blocking(move || {
        let frozen = s2.is_live() && s2.cgroup.as_ref().is_some_and(|c| c.freeze(5000).is_ok());
        let r = d2
            .backend
            .upper_view(&d2.store, &s2.root)
            .and_then(|view| d2.store.seal_delta(&view.path, prev.as_ref()));
        if frozen {
            if let Some(c) = &s2.cgroup {
                let _ = c.thaw();
            }
        }
        r
    })
    .await?;
    let bytes = delta.as_ref().map_or(0, |l| l.bytes);
    if let Some(l) = delta {
        ck.chain.push(l.id);
    }
    ck.manifest = Some(manifest);
    // Deep stacks hurt: a lookup for a missing file walks every lower layer,
    // and the mount option string has a length limit.
    if ck.chain.len() > d.cfg.squash_k {
        let (d2, chain) = (d.clone(), ck.chain.clone());
        let squashed = blocking(move || d2.store.squash(&chain)).await?;
        ck.chain = vec![squashed.id];
        d.metrics.inc("squashes_total", 1.0);
    }
    let mut layers = sbx.snapshot.layers.clone();
    layers.extend(ck.chain.iter().cloned());
    let snap = d
        .store
        .new_snapshot(&sbx.task_id, &sbx.snapshot.id, kind, layers, "")?;
    d.metrics.inc("seals_total", 1.0);
    d.metrics.inc("seal_bytes_total", bytes as f64);
    Ok((snap, bytes, t0.elapsed().as_micros() as u64))
}

fn exit_pb(e: sandfork_core::wire::ExecExit, oom: bool) -> pb::ExecExit {
    pb::ExecExit {
        exit_code: e.code,
        signal: e.signal,
        timed_out: e.timed_out,
        wall_ms: e.wall_ms,
        user_ms: e.user_ms,
        sys_ms: e.sys_ms,
        max_rss_kb: e.max_rss_kb,
        truncated: e.truncated,
        dropped_bytes: e.dropped_bytes,
        tail_stdout: e.tail_stdout,
        tail_stderr: e.tail_stderr,
        oom_killed: oom,
        cancelled: e.cancelled,
    }
}

/// Drive one command to completion, appending its events to `entry`. Runs in
/// its own task, so the command finishes whether or not a client is still
/// connected.
async fn drive_exec(d: Arc<Daemon>, sbx: Arc<Sandbox>, spec: ExecSpec, entry: Arc<ExecEntry>) {
    let push = |ev: pb::exec_event::Event| {
        lock(&entry.events).push(pb::ExecEvent { event: Some(ev) });
        entry.notify.notify_waiters();
    };
    let ooms_before = sbx.cgroup.as_ref().map_or(0, |c| c.stats().oom_kills);
    let result: Result<()> = async {
        if !sbx.is_live() {
            return Err(SfError::infra(
                "sandbox_dead",
                format!("sandbox {} is not running", sbx.id),
            ));
        }
        let limit = Duration::from_millis(spec.timeout_ms.max(1) + 15_000);
        let mut shim = sbx.shim.lock().await;
        let io = tokio::time::timeout(limit, async {
            shim.send(&ShimReq::Exec(spec)).await?;
            loop {
                match shim.recv().await? {
                    ShimResp::Stdout(b) => push(pb::exec_event::Event::Stdout(b)),
                    ShimResp::Stderr(b) => push(pb::exec_event::Event::Stderr(b)),
                    ShimResp::Exit(e) => return Ok(Ok(e)),
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
        match io {
            Ok(Ok(Ok(exit))) => {
                sbx.max_rss_kb.fetch_max(exit.max_rss_kb, Ordering::Relaxed);
                let oom = sbx
                    .cgroup
                    .as_ref()
                    .is_some_and(|c| c.stats().oom_kills > ooms_before);
                push(pb::exec_event::Event::Exit(exit_pb(exit, oom)));
                Ok(())
            }
            Ok(Ok(Err(e))) => Err(e),
            // The channel broke: the sandbox died under the command. An
            // out-of-memory kill is the task's outcome; anything else is ours.
            Ok(Err(_)) => match sbx.mark_dead() {
                SbxState::Dead { oom: true } => Err(SfError::task(
                    "oom",
                    format!(
                        "sandbox {} was killed for exceeding its memory limit",
                        sbx.id
                    ),
                )),
                _ => Err(SfError::infra(
                    "sandbox_dead",
                    format!("sandbox {} died during the command", sbx.id),
                )),
            },
            Err(_) => {
                sbx.mark_dead();
                Err(SfError::infra(
                    "shim_timeout",
                    format!("sandbox {} stopped answering during a command", sbx.id),
                ))
            }
        }
    }
    .await;
    sbx.execs.fetch_add(1, Ordering::Relaxed);
    d.metrics.inc("exec_total", 1.0);
    if let Err(e) = result {
        *lock(&entry.error) = Some(e);
    }
    entry.done.store(true, Ordering::SeqCst);
    entry.notify.notify_waiters();
}

/// How many completed commands each sandbox remembers for retries.
const EXEC_CACHE: usize = 64;

#[tonic::async_trait]
impl Sandfork for Service {
    async fn prepare_task(
        &self,
        request: Request<pb::PrepareTaskRequest>,
    ) -> std::result::Result<Response<pb::Snapshot>, Status> {
        let req = request.into_inner();
        let d = self.d.clone();
        let (dir, force) = (req.task_dir.clone(), req.force);
        let out = self
            .d
            .idem
            .run("prepare", &req.request_id, async move {
                let p = crate::prepare::prepare_task(&d, Path::new(&dir), force).await?;
                Ok(snapshot_pb(
                    &p.entry.snapshot,
                    if p.cached { 0 } else { p.micros },
                    p.cached,
                    p.bytes_written,
                ))
            })
            .await
            .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn fork_group(
        &self,
        request: Request<pb::ForkGroupRequest>,
    ) -> std::result::Result<Response<pb::Group>, Status> {
        let req = request.into_inner();
        let d = self.d.clone();
        let rid = req.request_id.clone();
        let out = self
            .d
            .idem
            .run("fork_group", &rid, async move {
                let snapshot = d.store.get_snapshot(&req.snapshot_id)?;
                let entry = d.task(&snapshot.task)?;
                let limits = merge_limits(req.limits, entry.task.spec.limits.to_limits());
                make_group(
                    &d,
                    GroupReq {
                        entry,
                        snapshot,
                        n: req.n,
                        mode: pb::ForkMode::try_from(req.mode).unwrap_or(pb::ForkMode::Auto),
                        limits,
                        priority: Priority::New,
                        admission_deadline: dur(req.admission_deadline, Duration::from_secs(60)),
                        lease: dur(req.lease, d.cfg.default_lease),
                        labels: req.labels,
                    },
                )
                .await
            })
            .await
            .map_err(status)?;
        Ok(Response::new(out))
    }

    type ExecStream = ReceiverStream<std::result::Result<pb::ExecEvent, Status>>;

    async fn exec(
        &self,
        request: Request<pb::ExecRequest>,
    ) -> std::result::Result<Response<Self::ExecStream>, Status> {
        let req = request.into_inner();
        let sbx = self.d.sandbox(&req.sandbox_id).map_err(status)?;
        let d = &self.d;
        let argv = match req.command {
            Some(pb::exec_request::Command::Shell(s)) => {
                let s = if d.namespaced() {
                    s
                } else {
                    crate::state::l0_rewrite(
                        &s,
                        &sbx.root.rootfs.to_string_lossy(),
                        &d.cfg
                            .rt_dir
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    )
                };
                vec!["/bin/sh".to_string(), "-c".to_string(), s]
            }
            Some(pb::exec_request::Command::Argv(a)) if !a.args.is_empty() => a.args,
            _ => {
                return Err(status(SfError::client(
                    "bad_request",
                    "Exec needs a shell command or a non-empty argv",
                )))
            }
        };
        let mut env = sbx.env.clone();
        for (k, v) in req.env {
            match env.iter_mut().find(|(ek, _)| *ek == k) {
                Some(e) => e.1 = v,
                None => env.push((k, v)),
            }
        }
        let spec = ExecSpec {
            argv,
            env,
            cwd: if req.cwd.is_empty() {
                paths::WORKSPACE.to_string()
            } else {
                req.cwd
            },
            timeout_ms: dur(req.timeout, Duration::from_secs(60)).as_millis() as u64,
            max_output: if req.max_output_bytes == 0 {
                64 << 10
            } else {
                req.max_output_bytes.min(4 << 20)
            },
            as_root: false,
            stdin: req.stdin,
        };
        // A retry with the same request id attaches to the original command,
        // running or finished, and replays its events from the start.
        let entry = {
            let mut cache = lock(&sbx.exec_cache);
            match (!req.request_id.is_empty())
                .then(|| cache.get(&req.request_id).cloned())
                .flatten()
            {
                Some(e) => e,
                None => {
                    let e = Arc::new(ExecEntry::default());
                    if !req.request_id.is_empty() {
                        if cache.len() >= EXEC_CACHE {
                            let done: Vec<String> = cache
                                .iter()
                                .filter(|(_, v)| v.done.load(Ordering::SeqCst))
                                .map(|(k, _)| k.clone())
                                .take(EXEC_CACHE / 2)
                                .collect();
                            for k in done {
                                cache.remove(&k);
                            }
                        }
                        cache.insert(req.request_id.clone(), e.clone());
                    }
                    tokio::spawn(drive_exec(self.d.clone(), sbx.clone(), spec, e.clone()));
                    e
                }
            }
        };
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            let mut sent = 0usize;
            loop {
                let notified = entry.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let (batch, done) = {
                    let ev = lock(&entry.events);
                    (ev[sent..].to_vec(), entry.done.load(Ordering::SeqCst))
                };
                for e in batch {
                    sent += 1;
                    if tx.send(Ok(e)).await.is_err() {
                        return; // the client went away; the command carries on
                    }
                }
                let drained = sent == lock(&entry.events).len();
                if done && drained {
                    let failure = lock(&entry.error).clone();
                    if let Some(e) = failure {
                        let _ = tx.send(Err(status(e))).await;
                    }
                    return;
                }
                if !done {
                    notified.await;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn read_file(
        &self,
        request: Request<pb::ReadFileRequest>,
    ) -> std::result::Result<Response<pb::FileChunk>, Status> {
        let req = request.into_inner();
        let sbx = self.d.sandbox(&req.sandbox_id).map_err(status)?;
        let max = if req.max_bytes == 0 {
            CHUNK as u32
        } else {
            req.max_bytes
        };
        match sbx
            .call(
                ShimReq::ReadFile {
                    path: req.path,
                    offset: req.offset,
                    max_len: max,
                },
                Duration::from_secs(30),
            )
            .await
            .map_err(status)?
        {
            ShimResp::File { data, eof, size } => {
                Ok(Response::new(pb::FileChunk { data, eof, size }))
            }
            other => Err(status(SfError::infra(
                "shim",
                format!("unexpected reply {other:?}"),
            ))),
        }
    }

    async fn write_file(
        &self,
        request: Request<pb::WriteFileRequest>,
    ) -> std::result::Result<Response<pb::WriteFileResult>, Status> {
        let req = request.into_inner();
        let sbx = self.d.sandbox(&req.sandbox_id).map_err(status)?;
        let scope = format!("write:{}", sbx.id);
        let rid = req.request_id.clone();
        let out = self
            .d
            .idem
            .run(&scope, &rid, async move {
                let mut written = 0u64;
                let mut append = req.append;
                let mut chunks = req.data.chunks(CHUNK).peekable();
                if chunks.peek().is_none() {
                    sbx.call(
                        ShimReq::WriteFile {
                            path: req.path.clone(),
                            data: vec![],
                            mode: req.mode,
                            append,
                        },
                        Duration::from_secs(30),
                    )
                    .await?;
                }
                for c in chunks {
                    match sbx
                        .call(
                            ShimReq::WriteFile {
                                path: req.path.clone(),
                                data: c.to_vec(),
                                mode: req.mode,
                                append,
                            },
                            Duration::from_secs(30),
                        )
                        .await?
                    {
                        ShimResp::Written { bytes } => written += bytes,
                        other => {
                            return Err(SfError::infra(
                                "shim",
                                format!("unexpected reply {other:?}"),
                            ))
                        }
                    }
                    append = true;
                }
                Ok(pb::WriteFileResult {
                    bytes_written: written,
                })
            })
            .await
            .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn eval(
        &self,
        request: Request<pb::EvalRequest>,
    ) -> std::result::Result<Response<pb::EvalResult>, Status> {
        let req = request.into_inner();
        let sbx = self.d.sandbox(&req.sandbox_id).map_err(status)?;
        if !sbx.has_kernel.load(Ordering::SeqCst) {
            return Err(status(SfError::client(
                "no_kernel",
                format!(
                    "sandbox {} has no stateful process: its task names no warm_process",
                    sbx.id
                ),
            )));
        }
        let scope = format!("eval:{}", sbx.id);
        let timeout = dur(req.timeout, Duration::from_secs(30));
        let out = self
            .d
            .idem
            .run(&scope, &req.request_id, async move {
                match sbx
                    .call(
                        ShimReq::Eval {
                            socket: paths::KERNEL_SOCK.into(),
                            code: req.code,
                            timeout_ms: timeout.as_millis() as u64,
                        },
                        timeout + Duration::from_secs(10),
                    )
                    .await?
                {
                    ShimResp::Eval { ok, output } => Ok(pb::EvalResult { ok, output }),
                    other => Err(SfError::infra(
                        "shim",
                        format!("unexpected reply {other:?}"),
                    )),
                }
            })
            .await
            .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn snapshot(
        &self,
        request: Request<pb::SnapshotRequest>,
    ) -> std::result::Result<Response<pb::Snapshot>, Status> {
        let req = request.into_inner();
        let sbx = self.d.sandbox(&req.sandbox_id).map_err(status)?;
        let d = self.d.clone();
        let scope = format!("snapshot:{}", sbx.id);
        let out = self
            .d
            .idem
            .run(&scope, &req.request_id, async move {
                let kind = if req.kind.is_empty() {
                    "turn"
                } else {
                    req.kind.as_str()
                };
                let (snap, bytes, micros) = seal_turn(&d, &sbx, kind).await?;
                Ok(snapshot_pb(&snap, micros, false, bytes))
            })
            .await
            .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn fork(
        &self,
        request: Request<pb::ForkRequest>,
    ) -> std::result::Result<Response<pb::Group>, Status> {
        let req = request.into_inner();
        let d = self.d.clone();
        let rid = req.request_id.clone();
        let out = self
            .d
            .idem
            .run("fork", &rid, async move {
                let snapshot = match req.source {
                    Some(pb::fork_request::Source::SnapshotId(id)) => d.store.get_snapshot(&id)?,
                    Some(pb::fork_request::Source::SandboxId(id)) => seal_turn(&d, &d.sandbox(&id)?, "turn").await?.0,
                    None => return Err(SfError::client("bad_request", "Fork needs a snapshot_id or a sandbox_id")),
                };
                let entry = d.task(&snapshot.task)?;
                if !entry.task.spec.prepare.warm_process.trim().is_empty() {
                    return Err(SfError::client("branch_needs_process_state", "this task keeps state in a process; branching it needs a CRIU checkpoint, which this daemon was started without (docs/checkpoint.md)"));
                }
                let limits = merge_limits(req.limits, entry.task.spec.limits.to_limits());
                d.metrics.inc("branches_total", 1.0);
                make_group(
                    &d,
                    GroupReq {
                        entry,
                        snapshot,
                        n: req.n.max(1),
                        mode: pb::ForkMode::F2Fs,
                        limits,
                        priority: Priority::New,
                        admission_deadline: dur(req.admission_deadline, Duration::from_secs(60)),
                        lease: dur(req.lease, d.cfg.default_lease),
                        labels: req.labels,
                    },
                )
                .await
            })
            .await
            .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn verify(
        &self,
        request: Request<pb::VerifyRequest>,
    ) -> std::result::Result<Response<pb::VerifyResult>, Status> {
        let req = request.into_inner();
        let sbx = self.d.sandbox(&req.sandbox_id).map_err(status)?;
        let d = self.d.clone();
        let scope = format!("verify:{}", sbx.id);
        let out = self
            .d
            .idem
            .run(&scope, &req.request_id, async move {
                Ok(crate::verify::verify(&d, sbx).await)
            })
            .await
            .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn checkpoint(
        &self,
        request: Request<pb::CheckpointRequest>,
    ) -> std::result::Result<Response<pb::Checkpoint>, Status> {
        let req = request.into_inner();
        let sbx = self.d.sandbox(&req.sandbox_id).map_err(status)?;
        let d = self.d.clone();
        let scope = format!("checkpoint:{}", sbx.id);
        let out = self
            .d
            .idem
            .run(&scope, &req.request_id, async move {
                let (snap, bytes, micros) = seal_turn(&d, &sbx, "turn").await?;
                let row = CheckpointRow {
                    id: sandfork_core::ids::checkpoint_id(),
                    sandbox: sbx.id.clone(),
                    snapshot: snap.id.clone(),
                    images: String::new(),
                    turn: req.turn,
                    created: sandfork_core::unix_time(),
                };
                d.store.put_checkpoint(&row)?;
                d.metrics.inc("checkpoints_total", 1.0);
                // Process images need CRIU through sandfork-priv. Without it
                // a checkpoint captures files only, which is the whole state
                // of a sandbox that runs nothing between turns.
                Ok(pb::Checkpoint {
                    id: row.id,
                    sandbox_id: sbx.id.clone(),
                    snapshot_id: snap.id,
                    turn: req.turn,
                    has_process_image: false,
                    bytes_written: bytes,
                    micros,
                })
            })
            .await
            .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn restore(
        &self,
        request: Request<pb::RestoreRequest>,
    ) -> std::result::Result<Response<pb::Group>, Status> {
        let req = request.into_inner();
        let d = self.d.clone();
        let rid = req.request_id.clone();
        let out = self
            .d
            .idem
            .run("restore", &rid, async move {
                let row = d.store.get_checkpoint(&req.checkpoint_id)?;
                let snapshot = d.store.get_snapshot(&row.snapshot)?;
                let entry = d.task(&snapshot.task)?;
                if !entry.task.spec.prepare.warm_process.trim().is_empty() {
                    return Err(SfError::infra("restore_needs_process_state", "this task keeps state in a process and the checkpoint holds files only; the rollout cannot be resumed and must be dropped"));
                }
                // A restore over different files gives a process whose memory
                // assumes files that are not there: check the sealed layer
                // still matches its manifest before anything is mounted.
                if let Some(top) = snapshot.layers.last().cloned() {
                    let d2 = d.clone();
                    if !blocking(move || d2.store.verify_layer(&top)).await? {
                        return Err(SfError::infra("layer_mismatch", format!("checkpoint {} references a layer that no longer matches its manifest", row.id)));
                    }
                }
                d.metrics.inc("restores_total", 1.0);
                let limits = entry.task.spec.limits.to_limits();
                make_group(
                    &d,
                    GroupReq {
                        entry,
                        snapshot,
                        n: req.n.max(1),
                        mode: pb::ForkMode::F2Fs,
                        limits,
                        priority: Priority::Restore,
                        admission_deadline: dur(req.admission_deadline, Duration::from_secs(60)),
                        lease: dur(req.lease, d.cfg.default_lease),
                        labels: req.labels,
                    },
                )
                .await
            })
            .await
            .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn release(
        &self,
        request: Request<pb::ReleaseRequest>,
    ) -> std::result::Result<Response<pb::Empty>, Status> {
        // Release always completes, whatever the client does: it runs in its
        // own task and is awaited here only to report when it is done.
        let d = self.d.clone();
        let target = request.into_inner().target;
        let work = tokio::spawn(async move {
            match target {
                Some(pb::release_request::Target::GroupId(id)) => {
                    let members = d
                        .group(&id)
                        .map(|g| lock(&g.sandboxes).clone())
                        .unwrap_or_default();
                    release_group(&d, &id).await;
                    for m in members {
                        let _ = d.store.delete_checkpoints_for(&m);
                    }
                }
                Some(pb::release_request::Target::SandboxId(id)) => {
                    if let Ok(sbx) = d.sandbox(&id) {
                        if let Ok(g) = d.group(&sbx.group_id) {
                            lock(&g.sandboxes).retain(|s| *s != id);
                        }
                        destroy_sandbox(&d, sbx).await;
                        let _ = d.store.delete_checkpoints_for(&id);
                    }
                }
                None => {}
            }
        });
        let _ = work.await;
        Ok(Response::new(pb::Empty {}))
    }

    async fn renew_lease(
        &self,
        request: Request<pb::RenewLeaseRequest>,
    ) -> std::result::Result<Response<pb::Lease>, Status> {
        let req = request.into_inner();
        let g = self.d.group(&req.group_id).map_err(status)?;
        // The fencing token: a stale orchestrator that lost its lease and
        // came back cannot extend a group it no longer owns.
        if req.token != 0 && req.token != g.token {
            return Err(status(SfError::client(
                "stale_lease",
                "lease token does not match",
            )));
        }
        let expires = now_ms() + dur(req.extend, self.d.cfg.default_lease).as_millis() as u64;
        g.lease_expires_ms.store(expires, Ordering::SeqCst);
        Ok(Response::new(pb::Lease {
            group_id: g.id.clone(),
            expires_unix_ms: expires,
            token: g.token,
        }))
    }

    async fn stats(
        &self,
        _request: Request<pb::StatsRequest>,
    ) -> std::result::Result<Response<pb::Stats>, Status> {
        crate::reconcile::refresh_gauges(&self.d);
        let values = self.d.metrics.snapshot();
        let mut prom = String::new();
        for (k, v) in &values {
            let kind = if k.ends_with("_total") {
                "counter"
            } else {
                "gauge"
            };
            prom.push_str(&format!("# TYPE sandfork_{k} {kind}\nsandfork_{k} {v}\n"));
        }
        Ok(Response::new(pb::Stats {
            values: values.into_iter().collect(),
            prometheus: prom,
            level: self.d.cfg.level.as_str().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    }

    async fn drain(
        &self,
        request: Request<pb::DrainRequest>,
    ) -> std::result::Result<Response<pb::Empty>, Status> {
        let grace = dur(request.into_inner().grace, Duration::from_secs(30));
        let d = self.d.clone();
        d.draining.store(true, Ordering::SeqCst);
        tokio::spawn(async move {
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline && !lock(&d.groups).is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            crate::reconcile::shutdown(&d).await;
            std::process::exit(0);
        });
        Ok(Response::new(pb::Empty {}))
    }

    async fn diff(
        &self,
        request: Request<pb::DiffRequest>,
    ) -> std::result::Result<Response<pb::DiffResult>, Status> {
        let req = request.into_inner();
        let sbx = self.d.sandbox(&req.sandbox_id).map_err(status)?;
        let d = self.d.clone();
        let out = async {
            let _shim = sbx.shim.lock().await;
            let s2 = sbx.clone();
            blocking(move || {
                let ns = d.store.cfg.xattr_ns;
                let view = d.backend.upper_view(&d.store, &s2.root)?;
                let lowers: Vec<_> = s2
                    .root
                    .layers
                    .iter()
                    .rev()
                    .map(|l| d.store.layer_root(l))
                    .collect();
                let changes = diff::changes(&lowers, &view.path, ns)
                    .map_err(|e| SfError::infra("diff", e.to_string()))?;
                let mut out = pb::DiffResult::default();
                for c in changes {
                    let mut fc = pb::FileChange {
                        path: c.path.clone(),
                        kind: match c.kind {
                            ChangeKind::Added => "added",
                            ChangeKind::Modified => "modified",
                            ChangeKind::Deleted => "deleted",
                        }
                        .to_string(),
                        is_dir: c.is_dir,
                        size: c.size,
                        text_diff: String::new(),
                    };
                    if !c.is_dir {
                        let lower = tree::lower_lookup(&lowers, &c.path, ns).map(|(p, _)| p);
                        let upper =
                            (c.kind != ChangeKind::Deleted).then(|| view.path.join(&c.path));
                        if let Some((text, a, r)) =
                            diff::text_diff(lower.as_deref(), upper.as_deref(), &c.path)
                        {
                            out.lines_added += a as u32;
                            out.lines_removed += r as u32;
                            if req.text {
                                fc.text_diff = text;
                            }
                        }
                    }
                    out.changes.push(fc);
                }
                Ok(out)
            })
            .await
        }
        .await
        .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn list_sandboxes(
        &self,
        _request: Request<pb::ListSandboxesRequest>,
    ) -> std::result::Result<Response<pb::SandboxList>, Status> {
        let all: Vec<Arc<Sandbox>> = lock(&self.d.sandboxes).values().cloned().collect();
        let mut sandboxes: Vec<pb::SandboxInfo> = all
            .iter()
            .map(|s| pb::SandboxInfo {
                id: s.id.clone(),
                group_id: s.group_id.clone(),
                task_id: s.task_id.clone(),
                mode: s.mode as i32,
                state: match s.state() {
                    SbxState::Live => "live",
                    SbxState::Dead { oom: true } => "oom",
                    SbxState::Dead { oom: false } => "dead",
                }
                .to_string(),
                age_ms: s.created.elapsed().as_millis() as u64,
                memory_bytes: s
                    .cgroup
                    .as_ref()
                    .map_or(s.max_rss_kb.load(Ordering::Relaxed) * 1024, |c| {
                        c.stats().memory_current
                    }),
                purpose: s.purpose.as_str().to_string(),
                execs: s.execs.load(Ordering::Relaxed),
            })
            .collect();
        sandboxes.sort_by(|a, b| b.age_ms.cmp(&a.age_ms));
        Ok(Response::new(pb::SandboxList { sandboxes }))
    }

    async fn register_tasks(
        &self,
        request: Request<pb::RegisterTasksRequest>,
    ) -> std::result::Result<Response<pb::RegisterTasksResult>, Status> {
        let dirs = request.into_inner().task_dirs;
        let d = self.d.clone();
        let out = blocking(move || {
            let mut out = pb::RegisterTasksResult::default();
            for dir in dirs {
                match crate::prepare::register_task(&d, Path::new(&dir)) {
                    Ok(_) => out.registered += 1,
                    Err(e) => out.errors.push(pb::Error {
                        class: pb::ErrorClass::Client as i32,
                        code: e.code,
                        message: format!("{dir}: {}", e.message),
                    }),
                }
            }
            Ok(out)
        })
        .await
        .map_err(status)?;
        Ok(Response::new(out))
    }

    async fn pick_tasks(
        &self,
        request: Request<pb::PickTasksRequest>,
    ) -> std::result::Result<Response<pb::PickTasksResult>, Status> {
        let req = request.into_inner();
        let mut rng = if req.seed == 0 {
            sandfork_core::rng::Rng::from_entropy()
        } else {
            sandfork_core::rng::Rng::new(req.seed)
        };
        let mut sched = lock(&self.d.sched);
        if req.new_epoch {
            sched.decay();
        }
        let strategy = if req.strategy.is_empty() {
            "band"
        } else {
            req.strategy.as_str()
        };
        let picks = sched.pick(
            req.n as usize,
            req.group_size.max(2),
            strategy,
            &req.family,
            &mut rng,
        );
        Ok(Response::new(pb::PickTasksResult {
            tasks: picks
                .into_iter()
                .map(|p| pb::TaskPick {
                    task_id: p.task_id,
                    task_dir: p.dir,
                    pass_estimate: p.pass_estimate,
                    usefulness: p.usefulness,
                })
                .collect(),
        }))
    }

    async fn gc(
        &self,
        request: Request<pb::GcRequest>,
    ) -> std::result::Result<Response<pb::GcResult>, Status> {
        let req = request.into_inner();
        let d = self.d.clone();
        let r = blocking(move || {
            d.store.gc(
                req.grace_seconds,
                (req.budget_bytes > 0).then_some(req.budget_bytes),
            )
        })
        .await
        .map_err(status)?;
        Ok(Response::new(pb::GcResult {
            snapshots_removed: r.snapshots_removed,
            layers_removed: r.layers_removed,
            orphans_removed: r.orphans_removed,
            bytes_freed: r.bytes_freed,
            bytes_total: r.bytes_total,
        }))
    }
}
