//! `sf`: a thin client for the Sandfork daemon.
//!
//! Every subcommand except `doctor` is one or two RPCs. `sf doctor` runs the
//! feasibility probes on this machine and needs no daemon.

use clap::{Parser, Subcommand};
use sandfork_proto::v1 as pb;
use sandfork_proto::SandforkClient;
use std::io::Write;
use std::process::ExitCode;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

const PROBE: &str = include_str!("../../../scripts/probe.sh");

#[derive(Parser)]
#[command(name = "sf", version, about = "Sandfork command-line tool")]
struct Cli {
    /// Daemon address: a Unix socket path, unix:///path, or https://host:port.
    #[arg(long, global = true, env = "SANDFORK_SOCKET")]
    socket: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the feasibility probes on this machine (no daemon needed).
    Doctor {
        /// Also run the checks that need root (CRIU, cgroup controllers).
        #[arg(long)]
        root: bool,
    },
    /// Build a task's prepared snapshot.
    Prepare {
        task_dir: String,
        #[arg(long)]
        force: bool,
    },
    /// One sandbox, one command, then release.
    Run {
        task_dir: String,
        /// auto, f1, f2 or f3.
        #[arg(long, default_value = "auto")]
        mode: String,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Fork a group from a snapshot and print its sandbox IDs.
    Fork {
        snapshot: String,
        #[arg(short, default_value_t = 1)]
        n: u32,
        #[arg(long, default_value = "auto")]
        mode: String,
        /// Lease in seconds.
        #[arg(long, default_value_t = 600)]
        lease: u64,
    },
    /// Run a command in a live sandbox.
    Exec {
        sandbox: String,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Send code to the sandbox's stateful kernel (data-analysis tasks).
    Eval {
        sandbox: String,
        code: String,
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// What the sandbox changed.
    Diff {
        sandbox: String,
        /// Print unified diffs for text files.
        #[arg(long)]
        text: bool,
    },
    /// Seal the sandbox's current state.
    Snapshot { sandbox: String },
    /// Verify a sandbox's final state against the task's hidden grader.
    Verify { sandbox: String },
    /// Release a group (grp-...) or a single sandbox (sbx-...).
    Release { id: String },
    /// List sandboxes.
    Ls,
    /// A live view of the pool; refreshes every second.
    Top,
    /// Print the daemon's counters and gauges.
    Stats {
        /// Prometheus text format.
        #[arg(long)]
        prometheus: bool,
    },
    /// Run garbage collection on the layer store.
    Gc {
        #[arg(long, default_value_t = 0)]
        grace: u64,
        #[arg(long, default_value_t = 0)]
        budget_mb: u64,
    },
    /// Ask the scheduler for tasks worth training on.
    Pick {
        #[arg(short, default_value_t = 4)]
        n: u32,
        #[arg(long, default_value_t = 8)]
        group_size: u32,
        #[arg(long, default_value = "band")]
        strategy: String,
        #[arg(long, default_value = "")]
        family: String,
    },
    /// Register task directories with the scheduler without preparing them.
    Register { task_dirs: Vec<String> },
    /// Stop admitting groups, wait for running ones, then exit the daemon.
    Drain {
        #[arg(long, default_value_t = 30)]
        grace: u64,
    },
}

fn secs(s: u64) -> Option<prost_types::Duration> {
    Some(prost_types::Duration {
        seconds: s as i64,
        nanos: 0,
    })
}

fn mode(s: &str) -> Result<i32, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "auto" => pb::ForkMode::Auto,
        "f1" | "cold" => pb::ForkMode::F1Cold,
        "f2" | "fs" => pb::ForkMode::F2Fs,
        "f3" | "zygote" => pb::ForkMode::F3Zygote,
        "f4" | "restore" => pb::ForkMode::F4Restore,
        other => return Err(format!("unknown fork mode `{other}`")),
    } as i32)
}

fn rid() -> String {
    format!(
        "sf-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn describe(st: &tonic::Status) -> String {
    let class = st
        .metadata()
        .get("sandfork-error-class")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?");
    let code = st
        .metadata()
        .get("sandfork-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?");
    format!("{class}/{code}: {}", st.message())
}

/// Stream a command's output to this terminal. Returns its exit code.
async fn exec(
    c: &mut SandforkClient<Channel>,
    sandbox: &str,
    argv: Vec<String>,
    timeout: u64,
) -> Result<i32, String> {
    let req = pb::ExecRequest {
        request_id: rid(),
        sandbox_id: sandbox.to_string(),
        command: Some(pb::exec_request::Command::Argv(pb::Argv { args: argv })),
        timeout: secs(timeout),
        max_output_bytes: 4 << 20,
        ..Default::default()
    };
    let mut stream = c.exec(req).await.map_err(|e| describe(&e))?.into_inner();
    let mut code = 1;
    while let Some(ev) = stream.next().await {
        match ev.map_err(|e| describe(&e))?.event {
            Some(pb::exec_event::Event::Stdout(b)) => {
                let _ = std::io::stdout().write_all(&b);
            }
            Some(pb::exec_event::Event::Stderr(b)) => {
                let _ = std::io::stderr().write_all(&b);
            }
            Some(pb::exec_event::Event::Exit(x)) => {
                if x.truncated {
                    eprintln!("[sf] output truncated: {} bytes dropped", x.dropped_bytes);
                }
                if x.timed_out {
                    eprintln!("[sf] command timed out after {} ms", x.wall_ms);
                }
                if x.oom_killed {
                    eprintln!("[sf] command was killed for exceeding the memory limit");
                }
                code = if x.signal != 0 {
                    128 + x.signal
                } else {
                    x.exit_code
                };
            }
            None => {}
        }
    }
    let _ = std::io::stdout().flush();
    Ok(code)
}

fn mode_name(m: i32) -> &'static str {
    match pb::ForkMode::try_from(m).unwrap_or(pb::ForkMode::Auto) {
        pb::ForkMode::Auto => "auto",
        pb::ForkMode::F1Cold => "F1",
        pb::ForkMode::F2Fs => "F2",
        pb::ForkMode::F3Zygote => "F3",
        pb::ForkMode::F4Restore => "F4",
    }
}

async fn print_ls(c: &mut SandforkClient<Channel>) -> Result<(), String> {
    let list = c
        .list_sandboxes(pb::ListSandboxesRequest {})
        .await
        .map_err(|e| describe(&e))?
        .into_inner();
    println!(
        "{:<18} {:<18} {:<8} {:<5} {:<5} {:>8} {:>9} {:>6}  TASK",
        "SANDBOX", "GROUP", "PURPOSE", "MODE", "STATE", "AGE(s)", "MEM(MB)", "EXECS"
    );
    for s in list.sandboxes {
        println!(
            "{:<18} {:<18} {:<8} {:<5} {:<5} {:>8.1} {:>9.1} {:>6}  {}",
            s.id,
            s.group_id.chars().take(18).collect::<String>(),
            s.purpose,
            mode_name(s.mode),
            s.state,
            s.age_ms as f64 / 1000.0,
            s.memory_bytes as f64 / 1048576.0,
            s.execs,
            s.task_id
        );
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<i32, String> {
    if let Cmd::Doctor { root } = &cli.cmd {
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c").arg(PROBE).arg("probe.sh");
        if *root {
            cmd.arg("--root");
        }
        return cmd
            .status()
            .map(|s| s.code().unwrap_or(1))
            .map_err(|e| format!("run probe: {e}"));
    }
    let addr = cli.socket.unwrap_or_else(sandfork_proto::default_socket);
    let mut c = sandfork_proto::connect(&addr)
        .await
        .map_err(|e| format!("cannot reach the daemon at {addr}: {e}"))?;
    match cli.cmd {
        Cmd::Doctor { .. } => {}
        Cmd::Prepare { task_dir, force } => {
            let dir = std::fs::canonicalize(&task_dir).map_err(|e| format!("{task_dir}: {e}"))?;
            let s = c
                .prepare_task(pb::PrepareTaskRequest {
                    request_id: rid(),
                    task_dir: dir.to_string_lossy().into_owned(),
                    force,
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            println!("{}", s.id);
            eprintln!(
                "task {}  layers {}  {}  prepared in {:.1} ms  {} bytes written",
                s.task_id,
                s.layers.len(),
                if s.cached { "cached" } else { "built" },
                s.prepare_micros as f64 / 1000.0,
                s.bytes_written
            );
        }
        Cmd::Run {
            task_dir,
            mode: m,
            timeout,
            cmd,
        } => {
            let dir = std::fs::canonicalize(&task_dir).map_err(|e| format!("{task_dir}: {e}"))?;
            let snap = c
                .prepare_task(pb::PrepareTaskRequest {
                    request_id: rid(),
                    task_dir: dir.to_string_lossy().into_owned(),
                    force: false,
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            let g = c
                .fork_group(pb::ForkGroupRequest {
                    request_id: rid(),
                    snapshot_id: snap.id,
                    n: 1,
                    mode: mode(&m)?,
                    lease: secs(timeout + 60),
                    ..Default::default()
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            let result = exec(&mut c, &g.sandboxes[0].id, cmd, timeout).await;
            let _ = c
                .release(pb::ReleaseRequest {
                    target: Some(pb::release_request::Target::GroupId(g.id)),
                })
                .await;
            return result;
        }
        Cmd::Fork {
            snapshot,
            n,
            mode: m,
            lease,
        } => {
            let g = c
                .fork_group(pb::ForkGroupRequest {
                    request_id: rid(),
                    snapshot_id: snapshot,
                    n,
                    mode: mode(&m)?,
                    lease: secs(lease),
                    ..Default::default()
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            eprintln!(
                "group {}  mode {}  queued {:.2} ms  forked in {:.2} ms",
                g.id,
                mode_name(g.mode),
                g.queued_micros as f64 / 1000.0,
                g.fork_micros as f64 / 1000.0
            );
            for s in g.sandboxes {
                println!("{}", s.id);
            }
        }
        Cmd::Exec {
            sandbox,
            timeout,
            cmd,
        } => return exec(&mut c, &sandbox, cmd, timeout).await,
        Cmd::Eval {
            sandbox,
            code,
            timeout,
        } => {
            let r = c
                .eval(pb::EvalRequest {
                    request_id: rid(),
                    sandbox_id: sandbox,
                    code,
                    timeout: secs(timeout),
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            print!("{}", r.output);
            return Ok(if r.ok { 0 } else { 1 });
        }
        Cmd::Diff { sandbox, text } => {
            let d = c
                .diff(pb::DiffRequest {
                    sandbox_id: sandbox,
                    text,
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            for ch in &d.changes {
                let tag = match ch.kind.as_str() {
                    "added" => "A",
                    "modified" => "M",
                    _ => "D",
                };
                println!("{tag} {}{}", ch.path, if ch.is_dir { "/" } else { "" });
                if text && !ch.text_diff.is_empty() {
                    print!("{}", ch.text_diff);
                }
            }
            eprintln!(
                "{} changes, +{} -{} lines",
                d.changes.len(),
                d.lines_added,
                d.lines_removed
            );
        }
        Cmd::Snapshot { sandbox } => {
            let s = c
                .snapshot(pb::SnapshotRequest {
                    request_id: rid(),
                    sandbox_id: sandbox,
                    kind: String::new(),
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            println!("{}", s.id);
            eprintln!(
                "{} layers, {} bytes written in {:.2} ms",
                s.layers.len(),
                s.bytes_written,
                s.prepare_micros as f64 / 1000.0
            );
        }
        Cmd::Verify { sandbox } => {
            let v = c
                .verify(pb::VerifyRequest {
                    request_id: rid(),
                    sandbox_id: sandbox,
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            let reward = v.reward.map_or("null".to_string(), |r| format!("{r:.4}"));
            println!(
                "status={} reward={reward} passed={}/{} verifier={} duration_ms={}",
                v.status, v.passed, v.total, v.verifier_version, v.duration_ms
            );
            if let Some(d) = v.diff {
                println!(
                    "diff: files={} +{} -{}",
                    d.files, d.lines_added, d.lines_removed
                );
                for r in d.rejected_paths {
                    println!("rejected: {r}");
                }
                for f in d.flags {
                    println!("flag: {f}");
                }
            }
            if let Some(e) = v.error {
                println!("error: {}: {}", e.code, e.message);
            }
            return Ok(if v.status == "ok" { 0 } else { 2 });
        }
        Cmd::Release { id } => {
            let target = if id.starts_with("sbx-") {
                pb::release_request::Target::SandboxId(id)
            } else {
                pb::release_request::Target::GroupId(id)
            };
            c.release(pb::ReleaseRequest {
                target: Some(target),
            })
            .await
            .map_err(|e| describe(&e))?;
        }
        Cmd::Ls => print_ls(&mut c).await?,
        Cmd::Top => loop {
            let st = c
                .stats(pb::StatsRequest {})
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            let v = |k: &str| st.values.get(k).copied().unwrap_or(0.0);
            print!("\x1b[2J\x1b[H");
            println!(
                "sandforkd {} level {}   uptime {:.0}s",
                st.version,
                st.level,
                v("uptime_seconds")
            );
            println!(
                "pool: {} sandboxes in {} groups   admission: {} slots, {:.0} MB, queue {}   zygotes {}",
                v("sandboxes_live"),
                v("groups_live"),
                v("admission_slots_used"),
                v("admission_memory_mb_used"),
                v("admission_queue_depth"),
                v("zygotes_live")
            );
            println!(
                "leak counters: mounts {} rollout-dirs {} cgroups {} zombies {} fds {}   store: {} layers, {:.1} MB, {} dedup hits",
                v("leak_mounts"),
                v("leak_rollout_dirs"),
                v("leak_cgroups"),
                v("leak_zombies"),
                v("daemon_open_fds"),
                v("store_layers"),
                v("store_bytes") / 1048576.0,
                v("store_dedup_hits")
            );
            println!();
            print_ls(&mut c).await?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        },
        Cmd::Stats { prometheus } => {
            let st = c
                .stats(pb::StatsRequest {})
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            if prometheus {
                print!("{}", st.prometheus);
            } else {
                let mut keys: Vec<_> = st.values.iter().collect();
                keys.sort_by(|a, b| a.0.cmp(b.0));
                for (k, v) in keys {
                    println!("{k:<36} {v}");
                }
            }
        }
        Cmd::Gc { grace, budget_mb } => {
            let r = c
                .gc(pb::GcRequest {
                    grace_seconds: grace,
                    budget_bytes: budget_mb << 20,
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            println!(
                "removed {} snapshots, {} layers, {} orphans; freed {} bytes; store now {} bytes",
                r.snapshots_removed,
                r.layers_removed,
                r.orphans_removed,
                r.bytes_freed,
                r.bytes_total
            );
        }
        Cmd::Pick {
            n,
            group_size,
            strategy,
            family,
        } => {
            let r = c
                .pick_tasks(pb::PickTasksRequest {
                    n,
                    group_size,
                    strategy,
                    family,
                    seed: 0,
                    new_epoch: false,
                })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            for t in r.tasks {
                println!(
                    "{:<40} p~{:.2} usefulness {:.2}  {}",
                    t.task_id, t.pass_estimate, t.usefulness, t.task_dir
                );
            }
        }
        Cmd::Register { task_dirs } => {
            let dirs = task_dirs
                .iter()
                .map(|d| {
                    std::fs::canonicalize(d)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| d.clone())
                })
                .collect();
            let r = c
                .register_tasks(pb::RegisterTasksRequest { task_dirs: dirs })
                .await
                .map_err(|e| describe(&e))?
                .into_inner();
            println!("registered {}", r.registered);
            for e in r.errors {
                eprintln!("error: {}", e.message);
            }
        }
        Cmd::Drain { grace } => {
            c.drain(pb::DrainRequest { grace: secs(grace) })
                .await
                .map_err(|e| describe(&e))?;
        }
    }
    Ok(0)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("sf: {e}");
            ExitCode::from(1)
        }
    }
}
