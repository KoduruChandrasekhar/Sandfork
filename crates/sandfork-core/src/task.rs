//! The task format (`task.toml`), base definitions, mutations and the pure
//! parts of the verifier contract.
//!
//! A task is a directory with one `task.toml`. A verifier is a deterministic
//! program that turns a final state into a reward. An infrastructure failure
//! is never a reward of zero: it is `status = infra_error`, `reward = null`.

use crate::{Ctx, Result, SfError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn d_base() -> String {
    "python".into()
}
fn d_timeout() -> u64 {
    180
}
fn d_turns() -> u32 {
    12
}
fn d_prompt() -> String {
    "prompt.md".into()
}
fn d_grader() -> String {
    "hidden".into()
}
fn d_vtimeout() -> u64 {
    30
}
fn d_reward() -> String {
    "fraction_passed".into()
}
fn d_exclude() -> Vec<String> {
    vec!["tests".into(), "__pycache__".into(), ".pytest_cache".into()]
}
fn d_mem() -> u64 {
    1024
}
fn d_pids() -> u64 {
    256
}
fn d_cpu() -> f64 {
    1.0
}
fn d_disk() -> u64 {
    512
}
fn d_threshold() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMeta {
    pub id: String,
    pub family: String,
    #[serde(default = "d_base")]
    pub base: String,
    #[serde(default = "d_timeout")]
    pub timeout_s: u64,
    #[serde(default = "d_turns")]
    pub max_turns: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prepare {
    /// Shared source tree; the mutation is applied on top. Empty means the
    /// task directory's own `repo/`.
    #[serde(default)]
    pub fixture: String,
    #[serde(default)]
    pub mutation: String,
    /// Run once per task, at preparation time, as the agent user.
    #[serde(default)]
    pub steps: Vec<String>,
    /// Data-analysis tasks name a stateful kernel here.
    #[serde(default)]
    pub warm_process: String,
    /// Fixture entries that never enter a rollout layer.
    #[serde(default = "d_exclude")]
    pub exclude: Vec<String>,
}

impl Default for Prepare {
    fn default() -> Self {
        Prepare {
            fixture: String::new(),
            mutation: String::new(),
            steps: vec![],
            warm_process: String::new(),
            exclude: d_exclude(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    #[serde(default = "d_prompt")]
    pub prompt: String,
    #[serde(default)]
    pub tools: Vec<String>,
    /// Only these paths, relative to the workspace, are graded.
    #[serde(default)]
    pub allowed_paths: Vec<String>,
}

impl Default for Agent {
    fn default() -> Self {
        Agent {
            prompt: d_prompt(),
            tools: vec![],
            allowed_paths: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verify {
    /// `pytest` parses a pytest summary; `script` reads the grader's last
    /// JSON line, `{"passed": n, "total": m}`.
    pub kind: String,
    /// Mounted read-only at `/grader`, only when grading.
    #[serde(default = "d_grader")]
    pub grader: String,
    pub command: String,
    #[serde(default = "d_vtimeout")]
    pub timeout_s: u64,
    /// `fraction_passed` or `all_or_nothing`.
    #[serde(default = "d_reward")]
    pub reward: String,
    /// Pins the denominator, so a run that collects fewer tests cannot score
    /// higher. Set by task validation.
    #[serde(default)]
    pub total: Option<u32>,
    /// A reward at or above this counts as a pass for the scheduler.
    #[serde(default = "d_threshold")]
    pub pass_threshold: f64,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLimits {
    #[serde(default = "d_mem")]
    pub memory_mb: u64,
    #[serde(default = "d_pids")]
    pub pids: u64,
    #[serde(default = "d_cpu")]
    pub cpu: f64,
    #[serde(default = "d_disk")]
    pub disk_mb: u64,
}

impl Default for TaskLimits {
    fn default() -> Self {
        TaskLimits {
            memory_mb: d_mem(),
            pids: d_pids(),
            cpu: d_cpu(),
            disk_mb: d_disk(),
        }
    }
}

impl TaskLimits {
    pub fn to_limits(&self) -> crate::wire::Limits {
        crate::wire::Limits {
            memory_mb: self.memory_mb,
            pids: self.pids,
            cpu: self.cpu,
            disk_mb: self.disk_mb,
            nofile: 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub task: TaskMeta,
    #[serde(default)]
    pub prepare: Prepare,
    #[serde(default)]
    pub agent: Agent,
    pub verify: Verify,
    #[serde(default)]
    pub limits: TaskLimits,
}

impl TaskSpec {
    pub fn verifier_version(&self) -> String {
        if self.verify.version.is_empty() {
            format!("{}-v1", self.verify.kind)
        } else {
            self.verify.version.clone()
        }
    }
}

/// A source mutation: replace `old` (starting at 1-based `start_line`) with `new`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mutation {
    pub file: String,
    pub start_line: usize,
    pub old: Vec<String>,
    pub new: Vec<String>,
    #[serde(default)]
    pub operator: String,
    #[serde(default)]
    pub description: String,
}

impl Mutation {
    /// Apply to file contents. Fails if the file no longer matches `old`, so a
    /// stale mutation can never silently produce a different task.
    pub fn apply(&self, text: &str) -> Result<String> {
        let mut lines: Vec<&str> = text.split('\n').collect();
        if self.start_line == 0 || self.start_line - 1 + self.old.len() > lines.len() {
            return Err(SfError::client(
                "mutation",
                format!("{}: line {} out of range", self.file, self.start_line),
            ));
        }
        let at = self.start_line - 1;
        for (i, want) in self.old.iter().enumerate() {
            if lines[at + i] != want {
                return Err(SfError::client(
                    "mutation",
                    format!(
                        "{}:{}: fixture does not match the mutation's `old` text",
                        self.file,
                        at + i + 1
                    ),
                ));
            }
        }
        let new: Vec<&str> = self.new.iter().map(|s| s.as_str()).collect();
        lines.splice(at..at + self.old.len(), new);
        Ok(lines.join("\n"))
    }
}

/// A task loaded from disk with every path resolved.
#[derive(Debug, Clone)]
pub struct LoadedTask {
    pub spec: TaskSpec,
    pub dir: PathBuf,
    pub tasks_root: PathBuf,
    /// Source tree copied into `/workspace`.
    pub source_dir: Option<PathBuf>,
    pub mutation: Option<Mutation>,
    pub grader_dir: Option<PathBuf>,
    pub prompt: String,
    /// BLAKE3 over everything that defines the prepared state. `PrepareTask`
    /// is idempotent by this hash.
    pub spec_hash: String,
}

fn resolve(dir: &Path, root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    for base in [dir, root] {
        let p = base.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

const SKIP: [&str; 3] = ["__pycache__", ".pytest_cache", ".git"];

fn hash_tree(h: &mut blake3::Hasher, dir: &Path, rel: &str) -> std::io::Result<()> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    names.sort_by_key(|e| e.file_name());
    for e in names {
        let name = e.file_name().to_string_lossy().into_owned();
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        let r = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let ft = e.file_type()?;
        h.update(r.as_bytes());
        h.update(&[0]);
        if ft.is_dir() {
            hash_tree(h, &e.path(), &r)?;
        } else if ft.is_symlink() {
            h.update(std::fs::read_link(e.path())?.to_string_lossy().as_bytes());
        } else {
            h.update(&std::fs::read(e.path())?);
        }
        h.update(&[0xff]);
    }
    Ok(())
}

impl LoadedTask {
    /// Load `dir/task.toml`. `tasks_root` is the directory that holds
    /// `fixtures/` and `bases/`; by default the task's grandparent.
    pub fn load(dir: &Path) -> Result<LoadedTask> {
        let dir = dir
            .canonicalize()
            .ctx_with(|| format!("task directory {}", dir.display()))?;
        let text = std::fs::read_to_string(dir.join("task.toml"))
            .ctx_with(|| format!("{}/task.toml", dir.display()))?;
        let spec: TaskSpec = toml::from_str(&text)
            .map_err(|e| SfError::client("task_toml", format!("{}: {e}", dir.display())))?;
        let tasks_root = dir
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(&dir)
            .to_path_buf();

        let source_dir = if spec.prepare.fixture.is_empty() {
            let repo = dir.join("repo");
            repo.is_dir().then_some(repo)
        } else {
            Some(
                resolve(&dir, &tasks_root, &spec.prepare.fixture).ok_or_else(|| {
                    SfError::client(
                        "task",
                        format!("fixture {} not found", spec.prepare.fixture),
                    )
                })?,
            )
        };
        let mutation = match resolve(&dir, &tasks_root, &spec.prepare.mutation) {
            Some(p) => {
                let t = std::fs::read_to_string(&p).ctx("mutation file")?;
                Some(
                    serde_json::from_str::<Mutation>(&t)
                        .map_err(|e| SfError::client("mutation", e.to_string()))?,
                )
            }
            None if spec.prepare.mutation.is_empty() => None,
            None => {
                return Err(SfError::client(
                    "task",
                    format!("mutation {} not found", spec.prepare.mutation),
                ))
            }
        };
        let grader_dir = resolve(&dir, &tasks_root, &spec.verify.grader);
        let prompt = std::fs::read_to_string(dir.join(&spec.agent.prompt)).unwrap_or_default();

        let mut h = blake3::Hasher::new();
        h.update(b"sandfork-task-v1\0");
        hash_tree(&mut h, &dir, "").ctx("hash task directory")?;
        if !spec.prepare.fixture.is_empty() {
            if let Some(src) = &source_dir {
                h.update(b"\0fixture\0");
                hash_tree(&mut h, src, "").ctx("hash fixture")?;
            }
        }
        if let Some(m) = &mutation {
            h.update(serde_json::to_string(m).unwrap_or_default().as_bytes());
        }
        let spec_hash = h.finalize().to_hex().to_string();
        Ok(LoadedTask {
            spec,
            dir,
            tasks_root,
            source_dir,
            mutation,
            grader_dir,
            prompt,
            spec_hash,
        })
    }
}

// ---------------------------------------------------------------- bases

fn d_kind() -> String {
    "host".into()
}
fn d_binds() -> Vec<String> {
    vec!["/usr".into()]
}
fn d_etc() -> Vec<String> {
    [
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/alternatives",
        "/etc/python3",
        "/etc/mime.types",
        "/etc/ssl/certs/ca-certificates.crt",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A base layer shared by a task family.
///
/// `kind = "host"` builds a skeleton root and binds the host's `/usr`
/// read-only into every sandbox, the way bubblewrap-style sandboxes do.
/// `kind = "rootfs"` uses a complete root filesystem imported with
/// `sf base import`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseSpec {
    pub name: String,
    #[serde(default = "d_kind")]
    pub kind: String,
    #[serde(default = "d_binds")]
    pub binds: Vec<String>,
    #[serde(default = "d_etc")]
    pub etc_copy: Vec<String>,
    /// Bind the wheel cache read-only at `/wheels` while the base is built.
    #[serde(default)]
    pub wheels: bool,
    /// Run once, as the sandbox's root, when the base is built.
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BaseFile {
    base: BaseSpec,
}

impl BaseSpec {
    pub fn builtin(name: &str) -> BaseSpec {
        BaseSpec {
            name: name.to_string(),
            kind: d_kind(),
            binds: d_binds(),
            etc_copy: d_etc(),
            wheels: false,
            steps: vec![],
            env: BTreeMap::new(),
        }
    }

    /// Load `<tasks_root>/bases/<name>.toml`, or fall back to the built-in host base.
    pub fn load(tasks_root: &Path, name: &str) -> Result<BaseSpec> {
        let p = tasks_root.join("bases").join(format!("{name}.toml"));
        if !p.exists() {
            return Ok(Self::builtin(name));
        }
        let text = std::fs::read_to_string(&p).ctx("base file")?;
        let f: BaseFile = toml::from_str(&text)
            .map_err(|e| SfError::client("base_toml", format!("{}: {e}", p.display())))?;
        Ok(f.base)
    }

    pub fn hash(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"sandfork-base-v1\0");
        h.update(serde_json::to_string(self).unwrap_or_default().as_bytes());
        h.finalize().to_hex().to_string()
    }
}

// ---------------------------------------------------------------- verifier parsing

/// Counts from a verifier run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub passed: u32,
    pub failed: u32,
    pub errors: u32,
}

/// Parse pytest's final summary line, e.g. `35 passed, 5 failed in 2.14s`.
/// Returns `None` if there is no summary at all.
pub fn parse_pytest_summary(output: &str) -> Option<Counts> {
    for line in output.lines().rev() {
        let line = line.trim().trim_matches('=').trim();
        let Some(idx) = line.rfind(" in ") else {
            continue;
        };
        let (head, tail) = line.split_at(idx);
        let secs = tail[4..].trim();
        let numeric = secs
            .split(|c: char| c == 's' || c == ' ')
            .next()
            .unwrap_or("");
        if numeric.is_empty() || numeric.parse::<f64>().is_err() {
            continue;
        }
        if head.trim() == "no tests ran" {
            return Some(Counts::default());
        }
        let mut c = Counts::default();
        let mut any = false;
        for part in head.split(',') {
            let mut it = part.split_whitespace();
            let (Some(n), Some(word)) = (it.next(), it.next()) else {
                continue;
            };
            let Ok(n) = n.parse::<u32>() else { continue };
            match word {
                "passed" => c.passed = n,
                "failed" => c.failed = n,
                "error" | "errors" => c.errors = n,
                "skipped" | "deselected" | "xfailed" | "xpassed" | "warning" | "warnings"
                | "rerun" => {}
                _ => continue,
            }
            any = true;
        }
        if any {
            return Some(c);
        }
    }
    None
}

/// Parse the last JSON object line of a script grader: `{"passed": n, "total": m}`.
pub fn parse_script_result(output: &str) -> Option<(u32, u32)> {
    for line in output.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            let p = v.get("passed").and_then(|x| x.as_u64());
            let t = v.get("total").and_then(|x| x.as_u64());
            if let (Some(p), Some(t)) = (p, t) {
                return Some((p as u32, t as u32));
            }
        }
    }
    None
}

/// Turn counts into a reward under the task's reward rule.
pub fn reward_from(rule: &str, passed: u32, total: u32) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let passed = passed.min(total);
    match rule {
        "all_or_nothing" => {
            if passed == total {
                1.0
            } else {
                0.0
            }
        }
        _ => passed as f64 / total as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pytest_summaries() {
        let c = parse_pytest_summary("....\n35 passed, 5 failed in 2.14s\n").unwrap();
        assert_eq!(
            c,
            Counts {
                passed: 35,
                failed: 5,
                errors: 0
            }
        );
        let c =
            parse_pytest_summary("===== 1 failed, 39 passed, 2 warnings in 0.31s =====").unwrap();
        assert_eq!((c.passed, c.failed), (39, 1));
        let c = parse_pytest_summary("40 passed in 0.50s").unwrap();
        assert_eq!(c.passed, 40);
        let c = parse_pytest_summary("2 errors in 0.20s").unwrap();
        assert_eq!(c.errors, 2);
        assert_eq!(
            parse_pytest_summary("no tests ran in 0.01s").unwrap(),
            Counts::default()
        );
        assert!(parse_pytest_summary("Traceback (most recent call last):\n  boom").is_none());
        assert!(parse_pytest_summary("").is_none());
    }

    #[test]
    fn script_results() {
        assert_eq!(
            parse_script_result("log line\n{\"passed\": 3, \"total\": 4}\n"),
            Some((3, 4))
        );
        assert_eq!(parse_script_result("nothing"), None);
    }

    #[test]
    fn rewards() {
        assert_eq!(reward_from("fraction_passed", 35, 40), 0.875);
        assert_eq!(reward_from("all_or_nothing", 39, 40), 0.0);
        assert_eq!(reward_from("all_or_nothing", 40, 40), 1.0);
        assert_eq!(reward_from("fraction_passed", 50, 40), 1.0);
        assert_eq!(reward_from("fraction_passed", 1, 0), 0.0);
    }

    #[test]
    fn mutation_applies_and_detects_drift() {
        let m = Mutation {
            file: "a.py".into(),
            start_line: 2,
            old: vec!["    return a < b".into()],
            new: vec!["    return a <= b".into()],
            operator: "flip".into(),
            description: String::new(),
        };
        let src = "def f(a, b):\n    return a < b\n";
        assert_eq!(m.apply(src).unwrap(), "def f(a, b):\n    return a <= b\n");
        assert!(m.apply("def f(a, b):\n    return a > b\n").is_err());
        assert!(m.apply("x\n").is_err());
    }

    #[test]
    fn task_toml_parses_with_defaults() {
        let t: TaskSpec = toml::from_str(
            r#"
            [task]
            id = "py-bugfix/intervals-0042"
            family = "py-bugfix"
            [verify]
            kind = "pytest"
            command = "python -m pytest -q /grader"
            "#,
        )
        .unwrap();
        assert_eq!(t.task.base, "python");
        assert_eq!(t.task.max_turns, 12);
        assert_eq!(t.limits.memory_mb, 1024);
        assert_eq!(t.verify.reward, "fraction_passed");
        assert_eq!(t.verifier_version(), "pytest-v1");
    }
}
