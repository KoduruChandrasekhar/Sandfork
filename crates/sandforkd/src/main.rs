//! `sandforkd`: the Sandfork daemon.
//!
//! # Process model
//!
//! Three kinds of process, so that nothing multithreaded ever forks a sandbox:
//!
//! * `sandforkd` (this binary): multithreaded and async. Serves the API, runs
//!   the pool and the scheduler. It never calls `fork` for a sandbox itself.
//! * `sandfork-spawner`: single-threaded, started here *before* the async
//!   runtime exists. It does the `clone3`, the maps, the mounts and the
//!   `execve` of the shim.
//! * `sandfork-shim`: PID 1 inside every sandbox.
//!
//! # Start-up order
//!
//! Everything that needs a single-threaded process happens first, in this
//! order: enter the user and mount namespace (L1), take over the cgroup
//! subtree, clean up after a previous daemon, start the spawners. Only then
//! does the tokio runtime start.
//!
//! # Trust
//!
//! The control socket is the privilege boundary: anyone who can connect can
//! run arbitrary commands in sandboxes, by design. It is a Unix socket with
//! mode 0600. The daemon is never exposed on a network without
//! authentication.

mod admission;
mod idem;
mod lifecycle;
mod prepare;
mod reconcile;
mod scheduler;
mod service;
mod state;
mod verify;
mod zygote;

use clap::Parser;
use sandfork_cgroup::CgroupRoot;
use sandfork_core::{Level, SfError};
use sandfork_layers::{CopyBackend, OverlayBackend, SnapshotFs, Store, StoreCfg, XattrNs};
use sandfork_ns::outer::IdPlan;
use sandfork_spawner::SpawnerClient;
use state::{log, Config, Daemon};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "sandforkd",
    version,
    about = "Sandfork daemon: snapshot-and-fork sandboxes for RL rollouts"
)]
struct Args {
    /// Layer store, rollout directories and metadata. Must be on a Linux
    /// filesystem (not a Windows-mounted drive).
    #[arg(long, env = "SANDFORK_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Unix socket to serve on (mode 0600).
    #[arg(long, env = "SANDFORK_SOCKET")]
    socket: Option<PathBuf>,
    /// Isolation level: auto, l0, l1 or l2.
    #[arg(long, default_value = "auto")]
    level: String,
    /// Wheel cache bound read-only at /wheels while bases and tasks are prepared.
    #[arg(long)]
    wheels_dir: Option<PathBuf>,
    /// Directory holding sandfork-spawner and sandfork-shim.
    #[arg(long)]
    bin_dir: Option<PathBuf>,
    /// Runtime directory (zygote library, kernel) bound at /.sandfork/rt.
    #[arg(long, env = "SANDFORK_RT_DIR")]
    rt_dir: Option<PathBuf>,
    /// Memory set aside for sandboxes, in MB. Default: 60% of available memory.
    #[arg(long)]
    memory_budget_mb: Option<u64>,
    /// Most sandboxes alive at once.
    #[arg(long, default_value_t = 64)]
    max_sandboxes: u32,
    /// Number of spawner processes.
    #[arg(long, default_value_t = 1)]
    spawners: usize,
    /// Default group lease, in seconds.
    #[arg(long, default_value_t = 120)]
    lease_seconds: u64,
    /// Squash a sandbox's checkpoint chain past this many layers.
    #[arg(long, default_value_t = 16)]
    squash_k: usize,
    /// Grace period before unreferenced layers are collected, in seconds.
    #[arg(long, default_value_t = 600)]
    gc_grace_seconds: u64,
    /// Disk budget for the layer store, in MB (0: none).
    #[arg(long, default_value_t = 0)]
    disk_budget_mb: u64,
    /// Memory estimate per sandbox with no history and no enforced limit, in MB.
    #[arg(long, default_value_t = 192)]
    default_mem_mb: u64,
    /// cgroup v2 limits: auto or off.
    #[arg(long, default_value = "auto")]
    cgroups: String,
    /// Writable-layer backing: auto, tmpfs (hard quota) or dir.
    #[arg(long, default_value = "auto")]
    rollout_fs: String,
    /// Retire a zygote after this many forks.
    #[arg(long, default_value_t = 500)]
    zygote_max_forks: u64,
    /// L2 only: host UID that sandbox UID 0 maps to.
    #[arg(long, default_value_t = 100_000)]
    uid_base: u32,
}

fn die(e: impl std::fmt::Display) -> ! {
    log("fatal", &e.to_string(), &[]);
    std::process::exit(1)
}

fn mem_available_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(4096)
}

/// Can a tmpfs hold the `user.overlay.*` markers a rootless overlay needs?
/// That takes Linux 6.6; older kernels fall back to plain directories, which
/// work but give no disk quota.
fn tmpfs_supports_user_xattr(state: &Path) -> bool {
    let probe = state
        .join("tmp")
        .join(format!("probe-{}", std::process::id()));
    if std::fs::create_dir_all(&probe).is_err() {
        return false;
    }
    let p = probe.to_string_lossy().into_owned();
    let ok = sandfork_ns::sys::mount(Some("tmpfs"), &p, Some("tmpfs"), 0, Some("size=1m")).is_ok()
        && sandfork_layers::tree::set_xattr(&probe, "user.overlay.opaque", b"y").is_ok();
    let _ = sandfork_ns::sys::umount_detach(&p);
    let _ = std::fs::remove_dir(&probe);
    ok
}

fn main() {
    let args = Args::parse();

    // ---- single-threaded start-up ------------------------------------
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    let wanted = match args.level.as_str() {
        "auto" => None,
        s => Some(
            Level::parse(s)
                .unwrap_or_else(|| die(format!("unknown level `{s}`: use auto, l0, l1 or l2"))),
        ),
    };
    let (level, plan) = match wanted {
        Some(Level::L0) => (Level::L0, IdPlan { base: 0, count: 1 }),
        Some(Level::L2) | None if euid == 0 => {
            if let Err(e) = sandfork_ns::outer::enter_private_mountns() {
                die(format!("level l2: {e}"));
            }
            (
                Level::L2,
                IdPlan {
                    base: args.uid_base,
                    count: 65536,
                },
            )
        }
        Some(Level::L2) => die("level l2 needs root"),
        other => match sandfork_ns::outer::enter_outer_userns() {
            Ok(plan) => (Level::L1, plan),
            Err(e) if other.is_none() => {
                log(
                    "warn",
                    "user namespaces unavailable; falling back to level l0",
                    &[("error", e.message)],
                );
                (Level::L0, IdPlan { base: 0, count: 1 })
            }
            Err(e) => die(format!("level l1: {e}")),
        },
    };
    let namespaced = level != Level::L0;

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let state_dir = args.state_dir.clone().unwrap_or_else(|| {
        if level == Level::L2 {
            PathBuf::from("/var/lib/sandfork")
        } else {
            PathBuf::from(
                std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share")),
            )
            .join("sandfork")
        }
    });
    if let Err(e) = std::fs::create_dir_all(&state_dir) {
        die(format!(
            "create state directory {}: {e}",
            state_dir.display()
        ));
    }
    let state_dir = state_dir.canonicalize().unwrap_or(state_dir);
    if state_dir.starts_with("/mnt/")
        && state_dir.components().count() >= 3
        && std::fs::read_to_string("/proc/version").is_ok_and(|v| v.contains("microsoft"))
    {
        log(
            "warn",
            "the state directory looks like a Windows-mounted drive; overlayfs cannot use it",
            &[("state_dir", state_dir.display().to_string())],
        );
    }
    let socket = args
        .socket
        .clone()
        .unwrap_or_else(|| PathBuf::from(sandfork_proto::default_socket()));
    let bin_dir = args.bin_dir.clone().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    });
    let rt_dir = args
        .rt_dir
        .clone()
        .or_else(|| Some(bin_dir.join("rt")).filter(|p| p.is_dir()));
    for bin in ["sandfork-spawner", "sandfork-shim"] {
        if !bin_dir.join(bin).exists() {
            die(format!(
                "{} not found: build the workspace or pass --bin-dir",
                bin_dir.join(bin).display()
            ));
        }
    }

    // The overlay is mounted by the sandbox's own first process, inside its
    // user namespace, so the store always uses `user.overlay.*` markers.
    let store = match Store::open(StoreCfg {
        root: state_dir.clone(),
        xattr_ns: XattrNs::User,
        id_base: namespaced.then_some(plan.base),
    }) {
        Ok(s) => Arc::new(s),
        Err(e) => die(e),
    };

    let cg = if namespaced && args.cgroups != "off" {
        let base = if level == Level::L2 {
            Some(PathBuf::from("/sys/fs/cgroup"))
        } else {
            sandfork_cgroup::own_cgroup()
        };
        match base
            .ok_or_else(|| SfError::infra("cgroup", "cannot find this process's cgroup"))
            .and_then(|b| CgroupRoot::init(&b, level == Level::L1))
        {
            Ok(c) => Some(c),
            Err(e) => {
                log(
                    "warn",
                    "running without cgroup limits",
                    &[("reason", e.message), ("hint", "for limits at l1, start the daemon in a delegated scope: systemd-run --user --scope -p Delegate=yes sandforkd ...".into())],
                );
                None
            }
        }
    } else {
        None
    };

    let (dirs, cgroups, rows) = reconcile::startup_cleanup(&store, cg.as_ref());
    if dirs + cgroups + rows > 0 {
        log(
            "info",
            "cleaned up after a previous daemon",
            &[
                ("rollout_dirs", dirs.to_string()),
                ("cgroups", cgroups.to_string()),
                ("rows", rows.to_string()),
            ],
        );
    }

    let mut rollout_tmpfs = false;
    if namespaced {
        rollout_tmpfs = match args.rollout_fs.as_str() {
            "tmpfs" => true,
            "dir" => false,
            _ => tmpfs_supports_user_xattr(&state_dir),
        };
        // Per-sandbox tmpfs mounts made here must reach a zygote's mount
        // namespace, so the rollout directory is a shared mount point.
        let r = state_dir.join("rollouts").to_string_lossy().into_owned();
        if let Err(e) = sandfork_ns::sys::mount(Some(&r), &r, None, libc::MS_BIND, None)
            .and_then(|_| sandfork_ns::sys::mount(None, &r, None, libc::MS_SHARED, None))
        {
            log("warn", "could not make the rollout directory a shared mount; zygote forks (F3) will not work", &[("error", e.message)]);
        }
    }

    let mut spawners = Vec::new();
    for _ in 0..args.spawners.max(1) {
        match SpawnerClient::start(&bin_dir.join("sandfork-spawner")) {
            Ok(s) => spawners.push(Arc::new(s)),
            Err(e) => die(format!("start spawner: {e}")),
        }
    }

    let owner = namespaced.then(|| (plan.outer(0), plan.outer(0)));
    let backend: Arc<dyn SnapshotFs> = if namespaced {
        Arc::new(OverlayBackend {
            tmpfs: rollout_tmpfs,
            owner,
        })
    } else {
        Arc::new(CopyBackend { owner: None })
    };
    let budget = args
        .memory_budget_mb
        .unwrap_or_else(|| mem_available_mb() * 6 / 10);
    let cfg = Config {
        wheels_dir: args
            .wheels_dir
            .clone()
            .unwrap_or_else(|| state_dir.join("wheels")),
        state_dir: state_dir.clone(),
        socket: socket.clone(),
        level,
        bin_dir,
        rt_dir,
        memory_budget_mb: budget,
        max_sandboxes: args.max_sandboxes,
        spawners: spawners.len(),
        default_lease: Duration::from_secs(args.lease_seconds.max(1)),
        squash_k: args.squash_k.max(1),
        gc_grace_s: args.gc_grace_seconds,
        disk_budget_bytes: (args.disk_budget_mb > 0).then_some(args.disk_budget_mb << 20),
        default_mem_mb: args.default_mem_mb.max(8),
        rollout_tmpfs,
        zygote_max_forks: args.zygote_max_forks.max(1),
    };
    let d = Arc::new(Daemon {
        // New groups are admitted only while the total stays under about 85%
        // of the memory set aside for Sandfork; the rest, and one slot in
        // eight, is reserved for verification forks and restores.
        admission: admission::Admission::with_reserve(
            cfg.max_sandboxes,
            budget,
            (cfg.max_sandboxes / 8).max(1),
            budget * 15 / 100,
        ),
        cfg,
        store,
        backend,
        spawners: RwLock::new(spawners),
        next_spawner: AtomicUsize::new(0),
        cg,
        plan,
        sandboxes: Mutex::new(HashMap::new()),
        groups: Mutex::new(HashMap::new()),
        tasks: Mutex::new(HashMap::new()),
        sched: Mutex::new(scheduler::Scheduler::default()),
        idem: idem::IdemCache::new(8192),
        metrics: state::Metrics::default(),
        zygotes: tokio::sync::Mutex::new(zygote::ZygotePool::default()),
        mem_hist: Mutex::new(HashMap::new()),
        draining: AtomicBool::new(false),
        prep_locks: Mutex::new(HashMap::new()),
        pinned: Mutex::new(HashSet::new()),
        started: Instant::now(),
    });

    // ---- multithreaded from here on ----------------------------------
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => die(format!("start runtime: {e}")),
    };
    rt.block_on(async move {
        let _ = std::fs::remove_file(&socket);
        if let Some(parent) = socket.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let listener = match tokio::net::UnixListener::bind(&socket) {
            Ok(l) => l,
            Err(e) => die(format!("bind {}: {e}", socket.display())),
        };
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
        tokio::spawn(reconcile::run_loop(d.clone()));
        log(
            "info",
            "ready",
            &[
                ("socket", socket.display().to_string()),
                ("level", d.cfg.level.as_str().to_string()),
                ("state_dir", d.cfg.state_dir.display().to_string()),
                ("id_mapping", if !d.namespaced() { "none".into() } else if d.plan.single() { "single".into() } else { format!("{} ids from {}", d.plan.count, d.plan.base) }),
                ("cgroups", d.cg.as_ref().map_or("off".into(), |c| c.controllers.join(","))),
                ("rollout_fs", if d.cfg.rollout_tmpfs { "tmpfs".into() } else { "dir".to_string() }),
                ("memory_budget_mb", d.cfg.memory_budget_mb.to_string()),
                ("rt_dir", d.cfg.rt_dir.as_ref().map_or("none".into(), |p| p.display().to_string())),
            ],
        );
        let stop = async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
            tokio::select! {
                _ = async { match term.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {}
                _ = async { match int.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {}
            }
        };
        let svc = sandfork_proto::SandforkServer::new(service::Service { d: d.clone() }).max_decoding_message_size(sandfork_proto::MAX_MESSAGE).max_encoding_message_size(sandfork_proto::MAX_MESSAGE);
        let served = tonic::transport::Server::builder().add_service(svc).serve_with_incoming_shutdown(tokio_stream::wrappers::UnixListenerStream::new(listener), stop).await;
        if let Err(e) = served {
            log("error", "server stopped", &[("error", e.to_string())]);
        }
        reconcile::shutdown(&d).await;
        let _ = std::fs::remove_file(&socket);
        log("info", "stopped", &[]);
    });
}
