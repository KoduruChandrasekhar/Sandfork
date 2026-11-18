//! Building base and prepared snapshots.
//!
//! A task is prepared once: its files become a layer, its preparation steps
//! run in a throwaway sandbox whose writable layer is then sealed, and the
//! result is pinned as the task's prepared snapshot. `PrepareTask` is
//! idempotent by spec hash: the same task directory on the same base always
//! resolves to the same snapshot, and identical layers deduplicate.

use crate::admission::Priority;
use crate::lifecycle::{create_sandbox, destroy_sandbox, run_steps, NewSbx, PreSpawn};
use crate::state::{lock, Daemon, Purpose, TaskEntry};
use sandfork_core::task::{BaseSpec, LoadedTask};
use sandfork_core::wire::{Bind, Flavor, Limits};
use sandfork_core::{paths, Ctx, Result, SfError};
use sandfork_layers::tree::{self, Copier, Owner};
use sandfork_layers::{base, LayerInfo, Snapshot};
use sandfork_proto::v1 as pb;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SfError::infra("task_aborted", e.to_string()))?
}

fn pin_once(d: &Daemon, snapshot: &str) -> Result<()> {
    if lock(&d.pinned).insert(snapshot.to_string()) {
        d.store.pin(snapshot, 1)?;
    }
    Ok(())
}

/// Run `steps` in a throwaway sandbox forked from `layers` and seal what
/// they wrote as a new layer.
async fn run_and_seal(
    d: &Arc<Daemon>,
    layers: Vec<String>,
    base: Arc<BaseSpec>,
    steps: Vec<String>,
    as_root: bool,
    what: &str,
    limits: Limits,
) -> Result<LayerInfo> {
    let tmp = d.store.new_snapshot("", "", "build", layers, "")?;
    let mem = d.cfg.default_mem_mb.max(256);
    d.admission
        .acquire(Priority::New, 1, mem, Duration::from_secs(600))
        .await?;
    let wheels = d.cfg.wheels_dir.clone();
    let mut extra_binds = vec![];
    let mut pre_spawn: Option<PreSpawn> = None;
    if wheels.is_dir() {
        if d.namespaced() {
            extra_binds.push(Bind {
                src: wheels.to_string_lossy().into_owned(),
                dst: paths::WHEELS.into(),
                readonly: true,
                recursive: true,
            });
        } else {
            // L0 has no mounts: a link stands in for the read-only bind.
            pre_spawn = Some(Box::new(move |root| {
                let link = root.rootfs.join("wheels");
                let _ = tree::remove_all(&link);
                std::os::unix::fs::symlink(&wheels, &link).ctx("link wheel cache")
            }));
        }
    }
    let created = create_sandbox(
        d,
        NewSbx {
            group_id: format!("prep-{}", sandfork_core::ids::new_id("g")),
            task_id: String::new(),
            family: String::new(),
            snapshot: tmp,
            base,
            limits,
            flavor: Flavor::Normal,
            extra_binds,
            mode: pb::ForkMode::F2Fs,
            purpose: Purpose::Prepare,
            mem_est_mb: mem,
            counted: true,
            pre_spawn,
        },
    )
    .await;
    let sbx = match created {
        Ok(s) => s,
        Err(e) => {
            d.admission.release(1, mem);
            return Err(e);
        }
    };
    let result = async {
        run_steps(d, &sbx, &steps, as_root, what).await?;
        // Nothing is running in a preparation sandbox between steps, so its
        // writable layer is quiescent.
        let (d2, s2) = (d.clone(), sbx.clone());
        blocking(move || {
            if !d2.namespaced() {
                // The link is not part of the prepared state.
                let _ = tree::remove_all(&s2.root.rootfs.join("wheels"));
                let _ = std::fs::create_dir(s2.root.rootfs.join("wheels"));
            }
            d2.backend.seal(&d2.store, &s2.root)
        })
        .await
    }
    .await;
    let group = sbx.group_id.clone();
    destroy_sandbox(d, sbx).await;
    if let Some(cg) = &d.cg {
        cg.remove_group(&group);
    }
    result
}

/// The base snapshot for a task family, built on first use.
pub async fn ensure_base(
    d: &Arc<Daemon>,
    tasks_root: &Path,
    name: &str,
) -> Result<(Snapshot, Arc<BaseSpec>)> {
    let spec = Arc::new(BaseSpec::load(tasks_root, name)?);
    // Ownership inside layers depends on the ID mapping, so it is part of the key.
    let key = format!(
        "base:{}:skel{}:{}:{}:{}",
        spec.hash(),
        base::SKELETON_VERSION,
        d.cfg.level.as_str(),
        d.plan.base,
        d.plan.count
    );
    let lk = d.prep_lock(&key);
    let _guard = lk.lock().await;
    if let Some(s) = d.store.find_snapshot_by_key(&key)? {
        pin_once(d, &s.id)?;
        return Ok((s, spec));
    }
    let (d2, spec2) = (d.clone(), spec.clone());
    let mut layers = blocking(move || {
        let plan = d2.plan;
        let outer = move |id: u32| plan.outer(id);
        let outer_ref: Option<&dyn Fn(u32) -> u32> =
            if d2.namespaced() { Some(&outer) } else { None };
        let skel =
            base::build_skeleton(&d2.store, &spec2, outer_ref, (plan.agent(), plan.agent()))?;
        let mut layers = Vec::new();
        if spec2.kind == "rootfs" {
            let rootfs = d2
                .store
                .find_snapshot_by_key(&format!("rootfs:{}", spec2.name))?
                .ok_or_else(|| {
                    SfError::client(
                        "base",
                        format!(
                            "base {} needs a root filesystem: run `sf base import {}` first",
                            spec2.name, spec2.name
                        ),
                    )
                })?;
            layers.extend(rootfs.layers);
        }
        layers.push(skel.id);
        Ok(layers)
    })
    .await?;
    if !spec.steps.is_empty() {
        let limits = Limits {
            memory_mb: 2048,
            pids: 512,
            cpu: 2.0,
            disk_mb: 2048,
            nofile: 4096,
        };
        let layer = run_and_seal(
            d,
            layers.clone(),
            spec.clone(),
            spec.steps.clone(),
            true,
            &format!("base {name}"),
            limits,
        )
        .await?;
        layers.push(layer.id);
    }
    let snap = d.store.new_snapshot("", "", "base", layers, &key)?;
    pin_once(d, &snap.id)?;
    d.metrics.inc("bases_built_total", 1.0);
    Ok((snap, spec))
}

/// Build the layer holding a task's files under `/workspace`: the fixture
/// with its mutation applied, plus the visible tests. Hidden tests never
/// enter any rollout layer.
fn build_files_layer(d: &Daemon, task: &LoadedTask) -> Result<LayerInfo> {
    let staging = d.store.new_staging()?;
    let ws = staging.join("root").join("workspace");
    let agent = d.agent_owner();
    let r = (|| -> Result<()> {
        std::fs::create_dir_all(&ws).ctx("create workspace")?;
        let map = move |_: &str| agent.unwrap_or((0, 0));
        let owner = || {
            if agent.is_some() {
                Owner::Map(&map)
            } else {
                Owner::Skip
            }
        };
        if let Some(src) = &task.source_dir {
            Copier::new(owner())
                .copy_tree(src, &ws, &task.spec.prepare.exclude)
                .ctx("copy task source")?;
        }
        if let Some(m) = &task.mutation {
            let path = ws.join(&m.file);
            let text = std::fs::read_to_string(&path)
                .ctx_with(|| format!("mutation target {}", m.file))?;
            // Rewriting in place keeps the file's owner and mode.
            std::fs::write(&path, m.apply(&text)?).ctx("write mutated file")?;
        }
        let visible = task.dir.join("visible");
        if visible.is_dir() {
            Copier::new(owner())
                .copy_tree(&visible, &ws.join("visible"), &["__pycache__".to_string()])
                .ctx("copy visible tests")?;
        }
        if let Some((u, g)) = agent {
            std::os::unix::fs::chown(&ws, Some(u), Some(g)).ctx("chown workspace")?;
            if visible.is_dir() {
                std::os::unix::fs::chown(ws.join("visible"), Some(u), Some(g))
                    .ctx("chown visible")?;
            }
        }
        Ok(())
    })();
    if let Err(e) = r {
        let _ = tree::remove_all(&staging);
        return Err(e);
    }
    d.store.finalize(&staging)
}

pub struct Prepared {
    pub entry: Arc<TaskEntry>,
    pub micros: u64,
    pub cached: bool,
    pub bytes_written: u64,
}

/// Record a task with the scheduler without preparing it.
pub fn register_task(d: &Daemon, dir: &Path) -> Result<String> {
    let task = LoadedTask::load(dir)?;
    lock(&d.sched).register(
        &task.spec.task.id,
        &task.dir.to_string_lossy(),
        &task.spec.task.family,
    );
    Ok(task.spec.task.id)
}

pub async fn prepare_task(d: &Arc<Daemon>, dir: &Path, force: bool) -> Result<Prepared> {
    let t0 = Instant::now();
    let dir2 = dir.to_path_buf();
    let task = blocking(move || LoadedTask::load(&dir2)).await?;
    let (base_snapshot, base) = ensure_base(d, &task.tasks_root, &task.spec.task.base).await?;
    let key = format!("task:{}:{}", task.spec_hash, base_snapshot.id);
    let lk = d.prep_lock(&key);
    let _guard = lk.lock().await;

    let existing = if force {
        None
    } else {
        d.store.find_snapshot_by_key(&key)?
    };
    let (snapshot, cached, bytes_written) = match existing {
        Some(s) => (s, true, 0),
        None => {
            let (d2, t2) = (d.clone(), task.clone());
            let files = blocking(move || build_files_layer(&d2, &t2)).await?;
            let mut bytes = files.bytes;
            let mut layers = base_snapshot.layers.clone();
            layers.push(files.id);
            if !task.spec.prepare.steps.is_empty() {
                let step_layer = run_and_seal(
                    d,
                    layers.clone(),
                    base.clone(),
                    task.spec.prepare.steps.clone(),
                    false,
                    &task.spec.task.id,
                    task.spec.limits.to_limits(),
                )
                .await?;
                bytes += step_layer.bytes;
                layers.push(step_layer.id);
            }
            let s = d.store.new_snapshot(
                &task.spec.task.id,
                &base_snapshot.id,
                "prepared",
                layers,
                &key,
            )?;
            d.metrics.inc("tasks_prepared_total", 1.0);
            (s, false, bytes)
        }
    };
    pin_once(d, &snapshot.id)?;
    let files_layer = snapshot
        .layers
        .get(base_snapshot.layers.len())
        .cloned()
        .ok_or_else(|| SfError::infra("prepare", "prepared snapshot has no files layer"))?;
    lock(&d.sched).register(
        &task.spec.task.id,
        &task.dir.to_string_lossy(),
        &task.spec.task.family,
    );
    let entry = Arc::new(TaskEntry {
        task,
        snapshot,
        base_snapshot,
        base,
        files_layer,
    });
    lock(&d.tasks).insert(entry.task.spec.task.id.clone(), entry.clone());
    Ok(Prepared {
        entry,
        micros: t0.elapsed().as_micros() as u64,
        cached,
        bytes_written,
    })
}
