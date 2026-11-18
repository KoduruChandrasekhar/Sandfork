//! Verification by transplant.
//!
//! Two adversaries share every sandbox: generated code that may do anything,
//! and an optimizer that rewards anything that raises the score. Isolation
//! handles the first. This module handles the second: the agent's own
//! environment is never run for grading.
//!
//! 1. **Diff.** Compute the agent's changes against the task's prepared
//!    snapshot.
//! 2. **Filter.** Keep only changes under `allowed_paths`. Everything else is
//!    discarded and listed in `rejected_paths`: tests, configuration,
//!    `conftest.py`, `sitecustomize.py`, `.pth` files, bytecode caches.
//! 3. **Transplant.** Apply the filtered diff to a fresh fork of the original
//!    prepared snapshot. Nothing else from the rollout comes along: no
//!    processes, no other files.
//! 4. **Mount the grader** read-only at `/grader`. It was never in any
//!    rollout's layers, so nothing during the rollout could read it.
//! 5. **Run the task's fixed command** with its own timeout and no network.
//!
//! An infrastructure failure is never a reward of zero: it is
//! `status = infra_error` with the reward unset.

use crate::admission::Priority;
use crate::lifecycle::{create_sandbox, destroy_sandbox, shell_spec, NewSbx, PreSpawn};
use crate::state::{lock, Daemon, Purpose, Sandbox, SbxState, TaskEntry};
use sandfork_core::task::{parse_pytest_summary, parse_script_result, reward_from};
use sandfork_core::wire::{Bind, Flavor};
use sandfork_core::{glob, paths, Ctx, ErrorClass, Result, SfError};
use sandfork_layers::tree::{self, Copier, Owner};
use sandfork_layers::{diff, Change, ChangeKind, RootHandle, UpperView};
use sandfork_proto::v1 as pb;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Never graded, even under an allowed path: files that change how the
/// interpreter or the test runner behaves without being the solution.
pub fn denied(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    rel.split('/')
        .any(|c| c == "__pycache__" || c == ".pytest_cache")
        || name.ends_with(".pyc")
        || name.ends_with(".pth")
        || matches!(
            name,
            "conftest.py"
                | "sitecustomize.py"
                | "usercustomize.py"
                | "pytest.ini"
                | "tox.ini"
                | "setup.cfg"
                | "pyproject.toml"
        )
}

/// Static-scan patterns for accepted source files. A hit does not change the
/// reward; it sends the rollout to the review queue.
const SUSPICIOUS: [&str; 12] = [
    "os._exit",
    "sys.modules",
    "pytest",
    "_pytest",
    "unittest",
    "PYTEST_CURRENT_TEST",
    "sitecustomize",
    "__builtins__",
    "builtins.",
    "inspect.stack",
    "sys.settrace",
    "atexit",
];

pub fn scan_flags(path: &str, text: &str) -> Vec<String> {
    SUSPICIOUS
        .iter()
        .filter(|p| text.contains(**p))
        .map(|p| format!("{p}:{path}"))
        .collect()
}

/// The accepted part of a rollout's changes, ready to transplant.
struct Collected {
    /// Overlay-format directory holding the rollout's changes.
    src: PathBuf,
    kept: Vec<Change>,
    rejected: Vec<String>,
    lines_added: u32,
    lines_removed: u32,
    files: u32,
    flags: Vec<String>,
    _view: Option<UpperView>,
    tmp: Option<PathBuf>,
}

impl Drop for Collected {
    fn drop(&mut self) {
        if let Some(t) = self.tmp.take() {
            let _ = tree::remove_all(&t);
        }
    }
}

/// Split a change list into what is graded and what is discarded.
pub fn filter_changes(changes: Vec<Change>, allowed: &[String]) -> (Vec<Change>, Vec<String>) {
    let (mut kept, mut rejected) = (Vec::new(), Vec::new());
    for c in changes {
        let Some(rel) = c.path.strip_prefix("workspace/") else {
            if c.path != "workspace" && (!c.is_dir || c.kind == ChangeKind::Deleted) {
                rejected.push(format!("/{}", c.path));
            }
            continue;
        };
        let ok = glob::any_match(allowed, rel) && !denied(rel);
        if ok {
            kept.push(c);
        } else if !c.is_dir || c.kind == ChangeKind::Deleted || c.opaque {
            rejected.push(rel.to_string());
        }
    }
    (kept, rejected)
}

fn collect(d: &Daemon, sbx: &Sandbox, entry: &TaskEntry) -> Result<Collected> {
    let ns = d.store.cfg.xattr_ns;
    let prepared = &entry.snapshot.layers;
    let lowers: Vec<PathBuf> = prepared
        .iter()
        .rev()
        .map(|l| d.store.layer_root(l))
        .collect();
    let view = d.backend.upper_view(&d.store, &sbx.root)?;
    // A sandbox forked from a turn snapshot carries layers above the prepared
    // snapshot. Its changes relative to the task are those layers plus its
    // own writable layer, merged.
    let own = &sbx.snapshot.layers;
    let extra: &[String] = if own.len() > prepared.len() && own[..prepared.len()] == prepared[..] {
        &own[prepared.len()..]
    } else {
        &[]
    };
    let (src, tmp) = if extra.is_empty() {
        (view.path.clone(), None)
    } else {
        let tmp = d.store.new_staging()?;
        let merged = tmp.join("root");
        let owner = || {
            if d.namespaced() {
                Owner::Preserve
            } else {
                Owner::Skip
            }
        };
        for l in extra {
            tree::merge_layer(&merged, &d.store.layer_root(l), ns, owner())
                .ctx("merge turn layer")?;
        }
        tree::merge_layer(&merged, &view.path, ns, owner()).ctx("merge writable layer")?;
        (merged, Some(tmp))
    };
    let changes = diff::changes(&lowers, &src, ns).ctx("diff against the task snapshot")?;
    let (kept, rejected) = filter_changes(changes, &entry.task.spec.agent.allowed_paths);

    let (mut added, mut removed, mut files, mut flags) = (0u32, 0u32, 0u32, Vec::new());
    for c in kept.iter().filter(|c| !c.is_dir) {
        files += 1;
        let lower = tree::lower_lookup(&lowers, &c.path, ns).map(|(p, _)| p);
        let upper = (c.kind != ChangeKind::Deleted).then(|| src.join(&c.path));
        if let Some((_, a, r)) = diff::text_diff(lower.as_deref(), upper.as_deref(), &c.path) {
            added += a as u32;
            removed += r as u32;
        }
        if let Some(u) = &upper {
            if let Ok(text) = std::fs::read_to_string(u) {
                flags.extend(scan_flags(
                    c.path.strip_prefix("workspace/").unwrap_or(&c.path),
                    &text,
                ));
            }
        }
    }
    Ok(Collected {
        src,
        kept,
        rejected,
        lines_added: added,
        lines_removed: removed,
        files,
        flags,
        _view: Some(view),
        tmp,
    })
}

/// Apply the accepted changes to a fresh root. `overlay` writes them in
/// upper format into the writable layer; otherwise they are applied to the
/// plain tree.
fn transplant(d: &Daemon, c: &Collected, root: &RootHandle) -> Result<()> {
    let overlay = root.copy_base.is_none();
    let dst_base: &Path = if overlay { &root.upper } else { &root.rootfs };
    let ns = d.store.cfg.xattr_ns;
    let agent = d.agent_owner();
    let mut cp = Copier::new(if d.namespaced() {
        Owner::Preserve
    } else {
        Owner::Skip
    });
    cp.skip_xattr = Some(ns.opaque());
    for ch in &c.kept {
        // Parents first: copied from the rollout where it has them, so mode
        // and owner match; otherwise created for the agent.
        let mut acc = PathBuf::new();
        let parts: Vec<&str> = ch.path.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            acc.push(part);
            let dp = dst_base.join(&acc);
            if std::fs::symlink_metadata(&dp).is_ok() {
                continue;
            }
            match std::fs::symlink_metadata(c.src.join(&acc)) {
                Ok(m) if m.is_dir() => cp
                    .copy_entry(&c.src.join(&acc), &dp, "", &m)
                    .ctx("transplant parent")?,
                _ => {
                    std::fs::create_dir(&dp).ctx("transplant parent")?;
                    if let Some((u, g)) = agent {
                        std::os::unix::fs::chown(&dp, Some(u), Some(g)).ctx("chown parent")?;
                    }
                }
            }
        }
        let (src, dst) = (c.src.join(&ch.path), dst_base.join(&ch.path));
        match ch.kind {
            ChangeKind::Deleted => {
                tree::remove_all(&dst).ctx("transplant delete")?;
                if overlay {
                    tree::make_whiteout(&dst).ctx("transplant whiteout")?;
                }
            }
            _ => {
                let m = std::fs::symlink_metadata(&src).ctx("transplant source")?;
                if !(ch.is_dir
                    && !ch.opaque
                    && std::fs::symlink_metadata(&dst).is_ok_and(|x| x.is_dir()))
                {
                    tree::remove_all(&dst).ctx("transplant replace")?;
                    cp.copy_entry(&src, &dst, "", &m).ctx("transplant copy")?;
                }
                if ch.is_dir && ch.opaque && overlay {
                    tree::set_opaque(&dst, ns).ctx("transplant opaque")?;
                }
            }
        }
    }
    Ok(())
}

fn infra(entry: Option<&TaskEntry>, e: SfError, started: Instant) -> pb::VerifyResult {
    let class = match e.class {
        ErrorClass::Task => pb::ErrorClass::Task,
        ErrorClass::Infra => pb::ErrorClass::Infra,
        ErrorClass::Client => pb::ErrorClass::Client,
    };
    pb::VerifyResult {
        status: "infra_error".into(),
        reward: None,
        passed: 0,
        total: 0,
        verifier_version: entry
            .map(|t| t.task.spec.verifier_version())
            .unwrap_or_default(),
        duration_ms: started.elapsed().as_millis() as u64,
        diff: None,
        error: Some(pb::Error {
            class: class as i32,
            code: e.code,
            message: e.message,
        }),
        output_tail: String::new(),
    }
}

async fn run(
    d: &Arc<Daemon>,
    sbx: &Arc<Sandbox>,
    entry: &Arc<TaskEntry>,
    started: Instant,
) -> Result<pb::VerifyResult> {
    let spec = &entry.task.spec;
    if sbx.state() == (SbxState::Dead { oom: false }) {
        return Err(SfError::infra(
            "sandbox_dead",
            format!("sandbox {} died before verification", sbx.id),
        ));
    }
    // Quiesce: holding the control channel means no agent command is
    // running; freezing the cgroup stops background processes from writing
    // while the changes are read.
    let collected = {
        let _shim = sbx.shim.lock().await;
        let (d2, s2, e2) = (d.clone(), sbx.clone(), entry.clone());
        tokio::task::spawn_blocking(move || {
            let frozen = s2.is_live() && s2.cgroup.as_ref().is_some_and(|c| c.freeze(5000).is_ok());
            let r = collect(&d2, &s2, &e2);
            if frozen {
                if let Some(c) = &s2.cgroup {
                    let _ = c.thaw();
                }
            }
            r
        })
        .await
        .map_err(|e| SfError::infra("task_aborted", e.to_string()))??
    };
    let diff_stats = pb::DiffStats {
        files: collected.files,
        lines_added: collected.lines_added,
        lines_removed: collected.lines_removed,
        rejected_paths: collected.rejected.iter().take(100).cloned().collect(),
        flags: collected.flags.clone(),
    };
    d.metrics.inc(
        "verify_rejected_paths_total",
        collected.rejected.len() as f64,
    );
    d.metrics.inc(
        "verify_flagged_diffs_total",
        if collected.flags.is_empty() { 0.0 } else { 1.0 },
    );

    let grader = entry.task.grader_dir.clone().ok_or_else(|| {
        SfError::client(
            "grader_missing",
            format!(
                "task {} has no grader directory `{}`",
                spec.task.id, spec.verify.grader
            ),
        )
    })?;
    let mem = d.mem_estimate(&spec.task.family, &spec.limits.to_limits());
    d.admission
        .acquire(Priority::Verify, 1, mem, Duration::from_secs(300))
        .await?;

    let (d2, grader2) = (d.clone(), grader.clone());
    let pre: PreSpawn = Box::new(move |root| {
        transplant(&d2, &collected, root)?;
        if !d2.namespaced() {
            // No mounts at L0: the grader is copied in, into a root the
            // rollout never saw.
            let dst = root.rootfs.join("grader");
            let _ = tree::remove_all(&dst);
            Copier::new(Owner::Skip)
                .copy_tree(&grader2, &dst, &["__pycache__".to_string()])
                .ctx("copy grader")?;
        }
        Ok(())
    });
    let extra_binds = if d.namespaced() {
        vec![Bind {
            src: grader.to_string_lossy().into_owned(),
            dst: paths::GRADER.into(),
            readonly: true,
            recursive: true,
        }]
    } else {
        vec![]
    };
    let created = create_sandbox(
        d,
        NewSbx {
            group_id: format!("verify-{}", sandfork_core::ids::new_id("g")),
            task_id: spec.task.id.clone(),
            family: spec.task.family.clone(),
            snapshot: entry.snapshot.clone(),
            base: entry.base.clone(),
            limits: spec.limits.to_limits(),
            flavor: Flavor::Normal,
            extra_binds,
            mode: pb::ForkMode::F2Fs,
            purpose: Purpose::Verify,
            mem_est_mb: mem,
            counted: true,
            pre_spawn: Some(pre),
        },
    )
    .await;
    let vsbx = match created {
        Ok(s) => s,
        Err(e) => {
            d.admission.release(1, mem);
            return Err(SfError::infra(&e.code, e.message));
        }
    };

    let mut cmd = shell_spec(
        d,
        &vsbx,
        &spec.verify.command,
        spec.verify.timeout_s.max(1) * 1000,
        false,
    );
    cmd.env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
    let ran = match sandfork_core::fail::point("verify_exec") {
        Ok(()) => vsbx.exec_collect(cmd).await,
        Err(e) => Err(e),
    };
    let group = vsbx.group_id.clone();
    destroy_sandbox(d, vsbx).await;
    if let Some(cg) = &d.cg {
        cg.remove_group(&group);
    }
    // A verifier that crashes or a sandbox that dies is infrastructure.
    let (exit, out, err) = ran.map_err(|e| {
        SfError::infra(
            &e.code,
            format!("verification sandbox failed: {}", e.message),
        )
    })?;

    let mut text = String::from_utf8_lossy(&out).into_owned();
    text.push_str(&String::from_utf8_lossy(&exit.tail_stdout));
    let (passed, total) = match spec.verify.kind.as_str() {
        "script" => parse_script_result(&text).unwrap_or((0, 0)),
        _ => {
            let c = parse_pytest_summary(&text).unwrap_or_default();
            // An exit status that contradicts the summary (a forged summary,
            // an early exit) counts as nothing passed.
            let consistent = (exit.code == 0) == (c.failed == 0 && c.errors == 0 && c.passed > 0);
            (
                if consistent { c.passed } else { 0 },
                c.passed + c.failed + c.errors,
            )
        }
    };
    let total = spec.verify.total.unwrap_or(total);
    let passed = passed.min(total);
    let reward = reward_from(&spec.verify.reward, passed, total);
    let mut tail: String = text
        .chars()
        .rev()
        .take(1500)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !err.is_empty() {
        tail.push_str("\n--- stderr ---\n");
        tail.push_str(&String::from_utf8_lossy(
            &err[err.len().saturating_sub(1500)..],
        ));
    }
    Ok(pb::VerifyResult {
        status: if exit.timed_out {
            "timeout".into()
        } else {
            "ok".into()
        },
        reward: Some(if exit.timed_out { 0.0 } else { reward }),
        passed,
        total,
        verifier_version: spec.verifier_version(),
        duration_ms: started.elapsed().as_millis() as u64,
        diff: Some(diff_stats),
        error: None,
        output_tail: tail,
    })
}

/// Verify one rollout's final state. Never fails: every failure becomes an
/// `infra_error` result with the reward unset.
pub async fn verify(d: &Arc<Daemon>, sbx: Arc<Sandbox>) -> pb::VerifyResult {
    let started = Instant::now();
    d.metrics.inc("verify_total", 1.0);
    let entry = match d.task(&sbx.task_id) {
        Ok(e) => e,
        Err(e) => return infra(None, e, started),
    };
    match run(d, &sbx, &entry, started).await {
        Ok(r) => {
            let reward = r.reward.unwrap_or(0.0);
            lock(&d.sched).record(
                &sbx.task_id,
                reward >= entry.task.spec.verify.pass_threshold,
            );
            d.metrics.inc("verify_ok_total", 1.0);
            d.metrics
                .inc("verify_seconds_total", started.elapsed().as_secs_f64());
            after_verify(d, &sbx);
            r
        }
        Err(e) => {
            d.metrics.inc("verify_infra_errors_total", 1.0);
            infra(Some(&entry), e, started)
        }
    }
}

/// Straggler policy: once most of a group has been verified, raise the CPU
/// weight of the members still running, so the group finishes sooner.
fn after_verify(d: &Daemon, sbx: &Sandbox) {
    let Ok(group) = d.group(&sbx.group_id) else {
        return;
    };
    let done = group.verified.fetch_add(1, Ordering::SeqCst) + 1;
    let members = lock(&group.sandboxes).clone();
    if (done as usize) * 2 < members.len() || done as usize >= members.len() {
        return;
    }
    for id in members {
        if id == sbx.id {
            continue;
        }
        if let Some(cg) = lock(&d.sandboxes).get(&id).and_then(|s| s.cgroup.clone()) {
            let _ = cg.set_weight(300);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(path: &str, kind: ChangeKind) -> Change {
        Change {
            path: path.into(),
            kind,
            is_dir: false,
            opaque: false,
            size: 1,
        }
    }

    #[test]
    fn only_allowed_paths_are_graded() {
        let allowed = vec!["src/**".to_string()];
        let changes = vec![
            ch("workspace/src/pkg/a.py", ChangeKind::Modified),
            ch("workspace/src/new.py", ChangeKind::Added),
            ch("workspace/conftest.py", ChangeKind::Added),
            ch("workspace/pytest.ini", ChangeKind::Modified),
            ch("workspace/visible/test_a.py", ChangeKind::Deleted),
            ch("workspace/src/sitecustomize.py", ChangeKind::Added),
            ch(
                "workspace/src/pkg/__pycache__/a.cpython-312.pyc",
                ChangeKind::Added,
            ),
            ch("workspace/src/evil.pth", ChangeKind::Added),
            ch("workspace/src/conftest.py", ChangeKind::Added),
            ch("home/agent/.bashrc", ChangeKind::Added),
            ch("usr/lib/python3/sitecustomize.py", ChangeKind::Added),
        ];
        let (kept, rejected) = filter_changes(changes, &allowed);
        assert_eq!(
            kept.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            vec!["workspace/src/pkg/a.py", "workspace/src/new.py"]
        );
        for want in [
            "conftest.py",
            "pytest.ini",
            "visible/test_a.py",
            "src/sitecustomize.py",
            "src/evil.pth",
            "src/conftest.py",
            "/home/agent/.bashrc",
            "/usr/lib/python3/sitecustomize.py",
        ] {
            assert!(
                rejected.iter().any(|r| r == want),
                "{want} must be rejected, got {rejected:?}"
            );
        }
    }

    #[test]
    fn nothing_is_graded_without_allowed_paths() {
        let (kept, rejected) =
            filter_changes(vec![ch("workspace/src/a.py", ChangeKind::Modified)], &[]);
        assert!(kept.is_empty());
        assert_eq!(rejected, vec!["src/a.py"]);
    }

    #[test]
    fn static_scan_flags_test_framework_awareness() {
        let flags = scan_flags(
            "src/a.py",
            "import sys\nif 'pytest' in sys.modules:\n    os._exit(0)\n",
        );
        assert!(flags.contains(&"pytest:src/a.py".to_string()));
        assert!(flags.contains(&"sys.modules:src/a.py".to_string()));
        assert!(flags.contains(&"os._exit:src/a.py".to_string()));
        assert!(scan_flags("src/a.py", "def add(a, b):\n    return a + b\n").is_empty());
    }
}
