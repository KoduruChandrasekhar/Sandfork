//! Failure handling: leases, liveness, and the reconciliation loop.
//!
//! The loop runs at start-up and every few seconds. It compares what the
//! daemon believes with reality (cgroups under `sandfork.slice`, rollout
//! directories, mounts) and tears down anything no live sandbox owns. Every
//! step is idempotent: running it twice changes nothing the second time.
//!
//! A daemon crash kills every sandbox (`PR_SET_PDEATHSIG` down the chain
//! daemon -> spawner -> PID 1) and takes its private mount namespace with it,
//! so what can be left behind is exactly: empty rollout directories, cgroup
//! directories, and database rows. Start-up cleanup removes all three.
//! Checkpoints and their layers survive, so in-flight rollouts can resume.

use crate::lifecycle::{now_ms, release_group};
use crate::state::{lock, log, Daemon, Purpose, SbxState};
use sandfork_cgroup::CgroupRoot;
use sandfork_core::wire::{ShimReq, ShimResp};
use sandfork_layers::{tree, Store};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn umount_detach(p: &Path) {
    if let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) {
        // SAFETY: `c` is a live NUL-terminated string. EINVAL (not a mount
        // point) is the expected result for a plain directory.
        unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    }
}

/// Remove everything a previous daemon left behind. Returns (rollout
/// directories, cgroups, database rows) removed.
pub fn startup_cleanup(store: &Store, cg: Option<&CgroupRoot>) -> (usize, usize, usize) {
    let (mut dirs, mut cgroups, mut rows) = (0, 0, 0);
    if let Ok(rd) = std::fs::read_dir(store.root().join("rollouts")) {
        for e in rd.flatten() {
            umount_detach(&e.path());
            if tree::remove_all(&e.path()).is_ok() {
                dirs += 1;
            }
        }
    }
    if let Some(cg) = cg {
        for (group, sbx) in cg.list_sandboxes() {
            let leaf = cg.sandbox(&group, &sbx);
            let _ = leaf.kill(5000);
            if leaf.remove().is_ok() {
                cgroups += 1;
            }
        }
        for group in cg.list_groups() {
            cg.remove_group(&group);
        }
    }
    for id in store.registered_sandboxes().unwrap_or_default() {
        if store.unregister_sandbox(&id).is_ok() {
            rows += 1;
        }
    }
    let _ = store.clear_pins();
    (dirs, cgroups, rows)
}

fn count_dir(p: &Path) -> usize {
    std::fs::read_dir(p).map(|d| d.count()).unwrap_or(0)
}

fn zombies_of(pid: u32) -> usize {
    let mut n = 0;
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return 0;
    };
    for t in tasks.flatten() {
        let children = std::fs::read_to_string(t.path().join("children")).unwrap_or_default();
        for child in children.split_whitespace() {
            let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).unwrap_or_default();
            // The state is the first field after the parenthesised command.
            if stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
            {
                n += 1;
            }
        }
    }
    n
}

/// Leak counters and pool gauges. Soak tests assert that every leak counter
/// stays flat.
pub fn refresh_gauges(d: &Daemon) {
    let m = &d.metrics;
    let state = d.store.root();
    let live = lock(&d.sandboxes).len();
    m.set("sandboxes_live", live as f64);
    m.set("groups_live", lock(&d.groups).len() as f64);
    let (slots, mb, queue) = d.admission.usage();
    m.set("admission_slots_used", slots as f64);
    m.set("admission_memory_mb_used", mb as f64);
    m.set("admission_queue_depth", queue as f64);
    let needle = state.to_string_lossy().into_owned();
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(&format!("{needle}/rollouts/")))
        .count();
    m.set("leak_mounts", mounts as f64);
    m.set(
        "leak_rollout_dirs",
        count_dir(&state.join("rollouts")) as f64,
    );
    m.set(
        "leak_cgroups",
        d.cg.as_ref().map_or(0, |c| c.list_sandboxes().len()) as f64,
    );
    m.set(
        "daemon_open_fds",
        count_dir(Path::new("/proc/self/fd")) as f64,
    );
    let spawner_pids: Vec<u32> = d
        .spawners
        .read()
        .map(|v| v.iter().map(|s| s.pid()).collect())
        .unwrap_or_default();
    let zombies: usize = std::iter::once(std::process::id())
        .chain(spawner_pids)
        .map(zombies_of)
        .sum();
    m.set("leak_zombies", zombies as f64);
    if let Ok(pool) = d.zygotes.try_lock() {
        m.set(
            "zygotes_live",
            (pool.active.len() + pool.retired.len()) as f64,
        );
    }
    if let Ok(s) = d.store.stats() {
        m.set("store_layers", s.layers as f64);
        m.set("store_bytes", s.bytes as f64);
        m.set("store_snapshots", s.snapshots as f64);
        m.set("store_dedup_hits", s.dedup_hits as f64);
    }
    m.set("idempotency_entries", d.idem.len() as f64);
    m.set("uptime_seconds", d.started.elapsed().as_secs_f64());
}

/// Tear down what exists on disk or in the cgroup tree but belongs to no
/// live sandbox. A grace period keeps it away from sandboxes still being
/// created, which have a directory before they have an entry.
fn sweep_orphans(d: &Daemon) -> usize {
    const GRACE: Duration = Duration::from_secs(120);
    let old = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|a| a > GRACE)
    };
    let known = |id: &str| lock(&d.sandboxes).contains_key(id);
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(d.store.root().join("rollouts")) {
        for e in rd.flatten() {
            let id = e.file_name().to_string_lossy().into_owned();
            if !known(&id) && old(&e.path()) {
                umount_detach(&e.path());
                let _ = tree::remove_all(&e.path());
                let _ = d.store.unregister_sandbox(&id);
                n += 1;
            }
        }
    }
    if let Some(cg) = &d.cg {
        for (group, id) in cg.list_sandboxes() {
            let leaf = cg.sandbox(&group, &id);
            if !known(&id) && old(&leaf.path) {
                let _ = leaf.kill(5000);
                let _ = leaf.remove();
                cg.remove_group(&group);
                n += 1;
            }
        }
    }
    n
}

async fn heartbeat(d: &Arc<Daemon>) {
    let all: Vec<_> = lock(&d.sandboxes).values().cloned().collect();
    for sbx in all {
        if !sbx.is_live() {
            continue;
        }
        if sbx.pid1_dead() {
            let st = sbx.mark_dead();
            d.metrics.inc("sandboxes_died_total", 1.0);
            log(
                "warn",
                "sandbox died",
                &[
                    ("sandbox", sbx.id.clone()),
                    ("oom", (st == SbxState::Dead { oom: true }).to_string()),
                ],
            );
            continue;
        }
        // Only an idle shim is pinged: one that is running a command answers
        // through that command's own deadline.
        let Ok(mut shim) = sbx.shim.try_lock() else {
            continue;
        };
        let ok = tokio::time::timeout(Duration::from_secs(5), async {
            shim.send(&ShimReq::Ping).await?;
            shim.recv().await
        })
        .await;
        drop(shim);
        if !matches!(ok, Ok(Ok(ShimResp::Pong { .. }))) {
            sbx.mark_dead();
            d.metrics.inc("sandboxes_died_total", 1.0);
            d.metrics.inc("heartbeats_missed_total", 1.0);
            log(
                "warn",
                "shim missed its heartbeat",
                &[("sandbox", sbx.id.clone())],
            );
        }
    }
}

fn restart_dead_spawners(d: &Daemon) {
    let mut spawners = match d.spawners.write() {
        Ok(s) => s,
        Err(p) => p.into_inner(),
    };
    for slot in spawners.iter_mut() {
        if slot.is_alive() {
            continue;
        }
        // Its sandboxes died with it; the heartbeat marks them dead.
        match sandfork_spawner::SpawnerClient::start(&d.cfg.bin_dir.join("sandfork-spawner")) {
            Ok(s) => {
                *slot = Arc::new(s);
                d.metrics.inc("spawner_restarts_total", 1.0);
                log("warn", "spawner restarted", &[]);
            }
            Err(e) => log("error", "spawner restart failed", &[("error", e.message)]),
        }
    }
}

pub async fn run_loop(d: Arc<Daemon>) {
    let mut tick = 0u64;
    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        tick += 1;

        // Leases: a group whose orchestrator crashed is reclaimed when its
        // lease expires instead of leaking.
        let now = now_ms();
        let expired: Vec<String> = lock(&d.groups)
            .values()
            .filter(|g| g.lease_expires_ms.load(Ordering::SeqCst) <= now)
            .map(|g| g.id.clone())
            .collect();
        for id in expired {
            log(
                "warn",
                "lease expired; reclaiming group",
                &[("group", id.clone())],
            );
            release_group(&d, &id).await;
            d.metrics.inc("leases_expired_total", 1.0);
        }

        heartbeat(&d).await;

        if tick % 5 == 0 {
            let d2 = d.clone();
            let orphans = tokio::task::spawn_blocking(move || {
                restart_dead_spawners(&d2);
                let n = sweep_orphans(&d2);
                refresh_gauges(&d2);
                n
            })
            .await
            .unwrap_or(0);
            if orphans > 0 {
                d.metrics.inc("reconcile_orphans_total", orphans as f64);
            }
            crate::zygote::reap_idle(&d).await;
        }
        if tick % 60 == 0 {
            let d2 = d.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let _ = d2.store.expire_checkpoints(24 * 3600);
                match d2.store.gc(d2.cfg.gc_grace_s, d2.cfg.disk_budget_bytes) {
                    Ok(r) => {
                        d2.metrics
                            .inc("gc_layers_removed_total", r.layers_removed as f64);
                        d2.metrics.inc("gc_bytes_freed_total", r.bytes_freed as f64);
                    }
                    Err(e) => log(
                        "error",
                        "garbage collection failed",
                        &[("error", e.message)],
                    ),
                }
            })
            .await;
        }
    }
}

/// Release everything: groups, zygotes, and any internal sandbox still alive.
pub async fn shutdown(d: &Arc<Daemon>) {
    d.draining.store(true, Ordering::SeqCst);
    let groups: Vec<String> = lock(&d.groups).keys().cloned().collect();
    for g in groups {
        release_group(d, &g).await;
    }
    crate::zygote::stop_all(d).await;
    let rest: Vec<_> = lock(&d.sandboxes)
        .values()
        .filter(|s| s.purpose != Purpose::Zygote)
        .cloned()
        .collect();
    for s in rest {
        crate::lifecycle::destroy_sandbox(d, s).await;
    }
}
