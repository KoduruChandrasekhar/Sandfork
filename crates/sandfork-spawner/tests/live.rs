//! Namespace integration: real rootless (L1) sandboxes, driven over the
//! shim's control channel with no daemon in between.
//!
//! Covers sandbox creation, the mount layout, the shim's contract (reaping,
//! timeouts, output caps, the file API), the basic escapes, and teardown
//! leaving nothing behind. It enters a user namespace itself, so it has no
//! test harness; it skips where user namespaces are unavailable.
//!
//! Needs the `sandfork-shim` binary next to `sandfork-spawner`
//! (`cargo build --workspace` first), or `SANDFORK_BIN_DIR`.

use sandfork_core::task::BaseSpec;
use sandfork_core::wire::{
    recv_frame, send_frame, Bind, ExecExit, ExecSpec, Flavor, Limits, ShimReq, ShimResp,
};
use sandfork_core::Level;
use sandfork_layers::{base, OverlayBackend, RootHandle, SnapshotFs, Store, StoreCfg, XattrNs};
use sandfork_ns::outer::IdPlan;
use sandfork_spawner::{sandbox_cfg, CfgParams, SpawnerClient};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

struct Env {
    store: Store,
    state: PathBuf,
    spawner: SpawnerClient,
    backend: OverlayBackend,
    plan: IdPlan,
    shim: PathBuf,
    base: BaseSpec,
    snapshot: sandfork_layers::Snapshot,
}

struct Sbx {
    root: RootHandle,
    ctl: OwnedFd,
    pidfd: OwnedFd,
}

struct Out {
    exit: ExecExit,
    stdout: String,
    stderr: String,
}

impl Env {
    fn create(&self, limits: Limits) -> sandfork_core::Result<Sbx> {
        let id = sandfork_core::ids::sandbox_id();
        let root =
            self.backend
                .fork_root(&self.store, &self.snapshot, &id, limits.disk_mb << 20)?;
        let binds = base::effective_binds(&self.base)
            .into_iter()
            .map(|b| Bind {
                src: b.clone(),
                dst: b,
                readonly: true,
                recursive: true,
            })
            .collect();
        let cfg = sandbox_cfg(CfgParams {
            id: &id,
            level: Level::L1,
            flavor: Flavor::Normal,
            state_dir: &self.state,
            lowers: root
                .layers
                .iter()
                .rev()
                .map(|l| self.store.layer_rel(l))
                .collect(),
            rollout: format!("rollouts/{id}"),
            xattr_user: true,
            shim: &self.shim,
            rt_dir: None,
            binds,
            limits,
            cgroup: None,
            plan: self.plan,
            seccomp: true,
            env: vec![],
            l0_root: None,
        });
        match self.spawner.spawn(&cfg) {
            Ok(s) => {
                if std::env::var("SANDFORK_SHOW_STEPS").is_ok() {
                    let line: Vec<String> = s
                        .steps
                        .iter()
                        .map(|st| format!("{}={}us", st.name, st.ns / 1000))
                        .collect();
                    println!("    steps: {}", line.join(" "));
                }
                Ok(Sbx {
                    root,
                    ctl: s.ctl,
                    pidfd: s.pidfd,
                })
            }
            Err(e) => {
                self.backend.release(&self.store, &root)?;
                Err(e)
            }
        }
    }

    fn destroy(&self, sbx: Sbx) {
        let _ = send_frame(sbx.ctl.as_raw_fd(), &ShimReq::Shutdown, &[]);
        // PID 1 is dead when its pidfd becomes readable; only then is the
        // writable layer removed.
        assert!(
            sandfork_ns::sys::poll_readable(sbx.pidfd.as_raw_fd(), 10_000),
            "sandbox did not exit"
        );
        self.backend
            .release(&self.store, &sbx.root)
            .expect("release root");
    }
}

fn call(sbx: &Sbx, req: &ShimReq) -> ShimResp {
    send_frame(sbx.ctl.as_raw_fd(), req, &[]).expect("send");
    recv_frame::<ShimResp>(sbx.ctl.as_raw_fd()).expect("recv").0
}

fn exec_spec(sbx: &Sbx, spec: ExecSpec) -> Out {
    send_frame(sbx.ctl.as_raw_fd(), &ShimReq::Exec(spec), &[]).expect("send exec");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    loop {
        match recv_frame::<ShimResp>(sbx.ctl.as_raw_fd()).expect("recv").0 {
            ShimResp::Stdout(d) => out.extend(d),
            ShimResp::Stderr(d) => err.extend(d),
            ShimResp::Exit(exit) => {
                return Out {
                    exit,
                    stdout: String::from_utf8_lossy(&out).into_owned(),
                    stderr: String::from_utf8_lossy(&err).into_owned(),
                }
            }
            other => panic!("unexpected reply to exec: {other:?}"),
        }
    }
}

fn spec(cmd: &str) -> ExecSpec {
    ExecSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
        env: vec![
            ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
            ("HOME".into(), "/home/agent".into()),
        ],
        cwd: "/workspace".into(),
        timeout_ms: 20_000,
        max_output: 1 << 16,
        as_root: false,
        stdin: vec![],
    }
}

fn sh(sbx: &Sbx, cmd: &str) -> Out {
    exec_spec(sbx, spec(cmd))
}

fn sh_root(sbx: &Sbx, cmd: &str) -> Out {
    exec_spec(
        sbx,
        ExecSpec {
            as_root: true,
            ..spec(cmd)
        },
    )
}

fn mounts_under(dir: &Path) -> usize {
    let needle = dir.to_string_lossy().into_owned();
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(&needle))
        .count()
}

fn identity(env: &Env) {
    let s = env.create(Limits::default()).expect("create");
    let o = sh(
        &s,
        "id -u; id -g; hostname; tr '\\0' ' ' < /proc/1/cmdline; echo; ls /",
    );
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(o.exit.code, 0, "{}", o.stderr);
    assert_eq!(lines[0], env.plan.agent().to_string(), "agent uid");
    assert_eq!(lines[1], env.plan.agent().to_string(), "agent gid");
    assert_eq!(lines[2], "sandbox", "hostname");
    assert!(
        lines[3].contains("sandfork-shim"),
        "PID 1 must be the shim, got {:?}",
        lines[3]
    );
    for want in ["workspace", "proc", "dev", "tmp", "usr", "etc", "home"] {
        assert!(
            lines[4..].contains(&want),
            "/{want} missing from {:?}",
            &lines[4..]
        );
    }
    assert_eq!(
        sh_root(&s, "id -u").stdout.trim(),
        "0",
        "preparation steps run as the sandbox's root"
    );
    // Only loopback exists in the network namespace.
    let net = sh(&s, "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '");
    assert_eq!(net.stdout.trim(), "lo", "interfaces: {:?}", net.stdout);
    // Only the sandbox's own processes are visible, and the host's home is not.
    let procs: usize = sh(&s, "ls /proc | grep -c '^[0-9]'")
        .stdout
        .trim()
        .parse()
        .unwrap();
    assert!(procs <= 6, "saw {procs} processes");
    assert_eq!(sh(&s, "ls /home").stdout.trim(), "agent");
    assert_ne!(sh(&s, "cat /etc/shadow").exit.code, 0);
    env.destroy(s);
}

fn file_api(env: &Env) {
    let s = env.create(Limits::default()).expect("create");
    let w = call(
        &s,
        &ShimReq::WriteFile {
            path: "src/pkg/a.txt".into(),
            data: b"hello\n".to_vec(),
            mode: 0,
            append: false,
        },
    );
    assert!(matches!(w, ShimResp::Written { bytes: 6 }), "{w:?}");
    call(
        &s,
        &ShimReq::WriteFile {
            path: "src/pkg/a.txt".into(),
            data: b"more\n".to_vec(),
            mode: 0,
            append: true,
        },
    );
    match call(
        &s,
        &ShimReq::ReadFile {
            path: "/workspace/src/pkg/a.txt".into(),
            offset: 0,
            max_len: 1000,
        },
    ) {
        ShimResp::File { data, eof, size } => assert_eq!(
            (data.as_slice(), eof, size),
            (&b"hello\nmore\n"[..], true, 11)
        ),
        other => panic!("{other:?}"),
    }
    // Written under the agent's filesystem UID, so the agent owns the file.
    assert_eq!(
        sh(&s, "stat -c %u src/pkg/a.txt").stdout.trim(),
        env.plan.agent().to_string()
    );
    match call(
        &s,
        &ShimReq::List {
            path: "src/pkg".into(),
        },
    ) {
        ShimResp::Listing(l) => assert_eq!(
            l.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["a.txt"]
        ),
        other => panic!("{other:?}"),
    }
    // Escapes: `..`, a symlink pointing outside, and a symlink parent.
    for path in ["../../etc/passwd", "../outside.txt"] {
        assert!(
            matches!(
                call(
                    &s,
                    &ShimReq::ReadFile {
                        path: path.into(),
                        offset: 0,
                        max_len: 10
                    }
                ),
                ShimResp::Err(_)
            ),
            "read {path} must fail"
        );
        assert!(
            matches!(
                call(
                    &s,
                    &ShimReq::WriteFile {
                        path: path.into(),
                        data: vec![1],
                        mode: 0,
                        append: false
                    }
                ),
                ShimResp::Err(_)
            ),
            "write {path} must fail"
        );
    }
    sh(&s, "ln -s /etc/passwd link; ln -s /etc dirlink");
    assert!(
        matches!(
            call(
                &s,
                &ShimReq::ReadFile {
                    path: "link".into(),
                    offset: 0,
                    max_len: 10
                }
            ),
            ShimResp::Err(_)
        ),
        "a symlink out of the workspace must not be followed"
    );
    assert!(matches!(
        call(
            &s,
            &ShimReq::WriteFile {
                path: "dirlink/evil".into(),
                data: vec![1],
                mode: 0,
                append: false
            }
        ),
        ShimResp::Err(_)
    ));
    assert_ne!(sh(&s, "test -e /etc/evil").exit.code, 0);
    // The agent cannot write where the agent has no permission.
    let r = call(
        &s,
        &ShimReq::WriteFile {
            path: "/etc/passwd".into(),
            data: b"x".to_vec(),
            mode: 0,
            append: false,
        },
    );
    if env.plan.agent() != 0 {
        assert!(
            matches!(r, ShimResp::Err(_)),
            "file API must apply the agent's permissions: {r:?}"
        );
    }
    // An absolute path is resolved inside the sandbox root, never the host's.
    match call(
        &s,
        &ShimReq::ReadFile {
            path: "/etc/hostname".into(),
            offset: 0,
            max_len: 100,
        },
    ) {
        ShimResp::File { data, .. } => assert_eq!(data, b"sandbox\n"),
        other => panic!("{other:?}"),
    }
    env.destroy(s);
}

fn timeouts_and_output(env: &Env) {
    let s = env.create(Limits::default()).expect("create");
    // A command that double-forks: the timeout must take the grandchildren too.
    let o = exec_spec(
        &s,
        ExecSpec {
            timeout_ms: 300,
            ..spec("sleep 100 & sleep 100 & sleep 100")
        },
    );
    assert!(
        o.exit.timed_out && o.exit.signal == libc::SIGKILL,
        "{:?}",
        o.exit
    );
    assert!(o.exit.wall_ms < 5000);
    let left = sh(&s, "for p in /proc/[0-9]*; do tr '\\0' ' ' < $p/cmdline; echo; done | grep -c '^sleep' || true");
    assert_eq!(
        left.stdout.trim(),
        "0",
        "orphan grandchildren survived the kill"
    );

    // 1 MB of output with a 1000-byte cap: a bounded head and a bounded tail.
    let o = exec_spec(
        &s,
        ExecSpec {
            max_output: 1000,
            ..spec("head -c 1000000 /dev/zero | tr '\\0' x")
        },
    );
    assert_eq!(o.stdout.len(), 1000);
    assert!(o.exit.truncated);
    assert_eq!(o.exit.tail_stdout.len(), 8192);
    assert_eq!(o.exit.dropped_bytes, 1_000_000 - 1000 - 8192);

    let o = sh(&s, "echo out; echo err >&2; exit 7");
    assert_eq!(
        (o.exit.code, o.stdout.as_str(), o.stderr.as_str()),
        (7, "out\n", "err\n")
    );
    let o = exec_spec(
        &s,
        ExecSpec {
            stdin: b"from stdin".to_vec(),
            ..spec("cat")
        },
    );
    assert_eq!(o.stdout, "from stdin");
    assert_eq!(
        exec_spec(
            &s,
            ExecSpec {
                argv: vec!["/no/such/binary".into()],
                ..spec("")
            }
        )
        .exit
        .code,
        127
    );
    env.destroy(s);
}

fn reaping(env: &Env) {
    let s = env.create(Limits::default()).expect("create");
    sh(&s, "for i in $(seq 40); do (sleep 0.02 &); done; sleep 0.4");
    let zombies = sh(
        &s,
        "cat /proc/[0-9]*/stat 2>/dev/null | awk '{print $3}' | grep -c Z || true",
    );
    assert_eq!(zombies.stdout.trim(), "0", "zombies accumulated");
    match call(&s, &ShimReq::Ping) {
        ShimResp::Pong { reaped, .. } => assert!(reaped >= 40, "shim reaped only {reaped} orphans"),
        other => panic!("{other:?}"),
    }
    env.destroy(s);
}

fn basic_escapes(env: &Env) {
    let s = env.create(Limits::default()).expect("create");
    let py = |call: &str| {
        format!("python3 -c \"import ctypes; l=ctypes.CDLL(None, use_errno=True); r={call}; print(r, ctypes.get_errno())\"")
    };
    for (name, call_expr, errno) in [
        (
            "mount",
            "l.mount(b'none', b'/tmp', b'tmpfs', 0, 0)",
            libc::EPERM,
        ),
        ("unshare", "l.unshare(0x10000000)", libc::EPERM),
        ("ptrace", "l.ptrace(16, 1, 0, 0)", libc::EPERM),
        ("pivot_root", "l.syscall(155, b'/', b'/')", libc::EPERM),
        ("clone3", "l.syscall(435, 0, 0)", libc::ENOSYS),
    ] {
        // As the sandbox's root too: the filter, not a missing capability,
        // must be what refuses the call.
        for root in [false, true] {
            let o = if root {
                sh_root(&s, &py(call_expr))
            } else {
                sh(&s, &py(call_expr))
            };
            assert_eq!(
                o.stdout.trim(),
                format!("-1 {errno}"),
                "{name} (root={root}): {}",
                o.stderr
            );
        }
    }
    // PID 1 has no handler for SIGKILL from inside its own namespace.
    sh_root(&s, "kill -9 1; kill -TERM 1 || true");
    assert!(
        matches!(call(&s, &ShimReq::Ping), ShimResp::Pong { .. }),
        "the shim must survive kill -9 1 from inside"
    );
    // No route out.
    let net = sh(&s, "python3 -c \"import socket; s=socket.socket(); s.settimeout(3); s.connect(('1.1.1.1', 53))\"");
    assert_ne!(net.exit.code, 0);
    assert!(net.stderr.contains("unreachable"), "{}", net.stderr);
    // /proc/sys is read-only even for the sandbox's root.
    assert_ne!(sh_root(&s, "echo 1 > /proc/sys/kernel/sysrq").exit.code, 0);
    assert_ne!(
        sh_root(&s, "test -e /dev/kvm || test -e /dev/mem")
            .exit
            .code,
        0
    );
    env.destroy(s);
}

fn sibling_isolation_and_quota(env: &Env) {
    let a = env
        .create(Limits {
            disk_mb: 8,
            ..Limits::default()
        })
        .expect("create a");
    let b = env
        .create(Limits {
            disk_mb: 8,
            ..Limits::default()
        })
        .expect("create b");
    sh(
        &a,
        "echo secret > /workspace/only-in-a; echo changed > /etc/hostname 2>/dev/null",
    );
    sh_root(&a, "echo changed > /etc/hostname");
    assert_ne!(
        sh(&b, "test -e /workspace/only-in-a").exit.code,
        0,
        "siblings must not see each other's writes"
    );
    assert_eq!(
        sh(&b, "cat /etc/hostname").stdout.trim(),
        "sandbox",
        "a change to a lower-layer file must land in the writer's own layer"
    );
    let lower = env
        .store
        .layer_root(&env.snapshot.layers[0])
        .join("etc/hostname");
    assert_eq!(
        std::fs::read_to_string(lower).unwrap(),
        "sandbox\n",
        "the lower layer itself must be untouched"
    );
    // The change is in A's writable layer, where the daemon can read it.
    assert!(a.root.upper.join("workspace/only-in-a").exists());

    // The per-sandbox tmpfs is a hard quota.
    let o = sh(&a, "dd if=/dev/zero of=/workspace/big bs=1M count=64 2>&1");
    assert_ne!(o.exit.code, 0);
    assert!(o.stdout.contains("No space left"), "{}", o.stdout);
    assert_eq!(
        sh(&b, "echo ok > /workspace/fine && cat /workspace/fine")
            .stdout
            .trim(),
        "ok",
        "a full sibling must not affect this sandbox"
    );

    let nofile = sh(&a, "python3 -c \"\nfs=[]\ntry:\n    while True: fs.append(open('/dev/null'))\nexcept OSError as e:\n    print(e.errno, len(fs))\"");
    let mut it = nofile.stdout.split_whitespace();
    assert_eq!(it.next(), Some("24"), "EMFILE expected: {}", nofile.stdout);
    assert!(it.next().unwrap().parse::<u64>().unwrap() < 1024);
    env.destroy(a);
    env.destroy(b);
}

fn failed_spawn_leaks_nothing(env: &Env) {
    let before = std::fs::read_dir(env.state.join("rollouts"))
        .unwrap()
        .count();
    let mut broken = env.snapshot.clone();
    broken.layers = vec!["0000000000000000000000000000000000000000".into()];
    let id = sandfork_core::ids::sandbox_id();
    let root = env
        .backend
        .fork_root(&env.store, &broken, &id, 1 << 20)
        .unwrap();
    let cfg = sandbox_cfg(CfgParams {
        id: &id,
        level: Level::L1,
        flavor: Flavor::Normal,
        state_dir: &env.state,
        lowers: vec![env.store.layer_rel(&broken.layers[0])],
        rollout: format!("rollouts/{id}"),
        xattr_user: true,
        shim: &env.shim,
        rt_dir: None,
        binds: vec![],
        limits: Limits::default(),
        cgroup: None,
        plan: env.plan,
        seccomp: true,
        env: vec![],
        l0_root: None,
    });
    let err = env
        .spawner
        .spawn(&cfg)
        .err()
        .expect("spawn over a missing layer must fail");
    assert!(err.message.contains("overlay"), "{err}");
    env.backend.release(&env.store, &root).unwrap();
    assert_eq!(
        std::fs::read_dir(env.state.join("rollouts"))
            .unwrap()
            .count(),
        before
    );
    // The spawner is still healthy afterwards.
    assert_eq!(env.spawner.ping().unwrap(), 1);
}

fn churn(env: &Env, n: usize) {
    let mounts = mounts_under(&env.state);
    for _ in 0..n {
        let s = env
            .create(Limits {
                disk_mb: 4,
                ..Limits::default()
            })
            .expect("create");
        assert_eq!(sh(&s, "echo hi").stdout, "hi\n");
        env.destroy(s);
    }
    assert_eq!(
        mounts_under(&env.state),
        mounts,
        "leaked mounts after {n} create/destroy cycles"
    );
    assert_eq!(
        std::fs::read_dir(env.state.join("rollouts"))
            .unwrap()
            .count(),
        0,
        "leaked rollout directories"
    );
    assert!(
        env.store.registered_sandboxes().unwrap().is_empty(),
        "leaked sandbox rows"
    );
}

fn main() {
    let plan = match sandfork_ns::outer::enter_outer_userns() {
        Ok(p) => p,
        Err(e) => {
            if std::env::var("SANDFORK_REQUIRE_NS").is_ok() {
                eprintln!("live: cannot enter a user namespace: {e}");
                std::process::exit(1);
            }
            println!("live: SKIPPED (no user namespaces here: {e})");
            return;
        }
    };
    let bin_dir = std::env::var("SANDFORK_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_BIN_EXE_sandfork-spawner"))
                .parent()
                .unwrap()
                .to_path_buf()
        });
    let shim = bin_dir.join("sandfork-shim");
    assert!(
        shim.exists(),
        "{} not found: run `cargo build --workspace` first",
        shim.display()
    );
    let state = std::env::var("SANDFORK_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(format!("sf-live-{}", std::process::id()));
    let _ = sandfork_layers::tree::remove_all(&state);
    let store = Store::open(StoreCfg {
        root: state.clone(),
        xattr_ns: XattrNs::User,
        id_base: Some(plan.base),
    })
    .expect("open store");
    let base_spec = BaseSpec::builtin("host");
    let outer = |id: u32| plan.outer(id);
    let skel = base::build_skeleton(
        &store,
        &base_spec,
        Some(&outer),
        (plan.agent(), plan.agent()),
    )
    .expect("skeleton");
    let snapshot = store
        .new_snapshot("", "", "base", vec![skel.id], "")
        .expect("snapshot");
    let spawner = SpawnerClient::start(&bin_dir.join("sandfork-spawner")).expect("start spawner");
    let env = Env {
        backend: OverlayBackend {
            tmpfs: true,
            owner: Some((plan.outer(0), plan.outer(0))),
        },
        store,
        state: state.clone(),
        spawner,
        plan,
        shim,
        base: base_spec,
        snapshot,
    };
    println!("live: id plan {:?}", env.plan);

    let cycles: usize = std::env::var("SANDFORK_CHURN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let tests: [(&str, &dyn Fn(&Env)); 7] = [
        ("identity", &identity),
        ("file_api", &file_api),
        ("timeouts_and_output", &timeouts_and_output),
        ("reaping", &reaping),
        ("basic_escapes", &basic_escapes),
        ("sibling_isolation_and_quota", &sibling_isolation_and_quota),
        ("failed_spawn_leaks_nothing", &failed_spawn_leaks_nothing),
    ];
    let only = std::env::var("SANDFORK_ONLY").ok();
    for (name, f) in tests {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        f(&env);
        println!("live: ok {name}");
    }
    if only.is_none() {
        churn(&env, cycles);
        println!("live: ok churn ({cycles} cycles, no leaked mounts, directories or rows)");
    }
    drop(env);
    let _ = sandfork_layers::tree::remove_all(&state);
    println!("live: all passed");
}
