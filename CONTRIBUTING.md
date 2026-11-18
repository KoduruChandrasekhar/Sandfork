# Contributing

## Development setup (Windows laptop, WSL2)

1. Install WSL2 with Ubuntu 24.04 or newer, and check that `uname -r` is
   5.15 or newer.
2. Enable systemd in `/etc/wsl.conf` (`[boot]`, `systemd=true`). It is
   needed for cgroup delegation to your user.
3. Check cgroup v2: `stat -fc %T /sys/fs/cgroup` must print `cgroup2fs`.
4. Optional, for a sandbox root that is not your user:
   `sudo apt install uidmap` (`newuidmap`, `newgidmap`).
5. Rust through rustup; Python 3.12 or newer.
6. On Ubuntu 24.04 the AppArmor restriction on unprivileged user namespaces
   is on by default. For development:
   `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`.
   This turns the protection off for the whole machine.

The sources may sit anywhere, including a Windows drive under `/mnt/c`.
Build output and all runtime state are kept on the Linux filesystem
(`scripts/cargo.sh` sets `CARGO_TARGET_DIR=~/.cache/sandfork/target`, and the
daemon's state goes in `~/.local/share/sandfork`). A Windows-mounted drive
cannot hold overlayfs upper directories, and it is slow.

```bash
make venv            # Python environment in ~/.cache/sandfork/venv
bash scripts/wheels.sh   # the wheel cache base layers install from (sandboxes have no network)
make build           # debug binaries and the runtime directory
make doctor          # the feasibility probes
make test-local      # lint plus every test layer below
```

## Test layers

| Layer | Command | Needs |
|---|---|---|
| Rust unit and property | `bash scripts/cargo.sh test --workspace` | nothing |
| Namespace integration | `make test-ns` | user namespaces |
| Python unit | `cd python && pytest tests --ignore tests/test_daemon.py` | PyTorch for the trainer tests (skipped otherwise) |
| End to end | `make test-python` | a daemon (started by the target) |
| Task validation | `make validate-tasks` | a daemon and generated tasks |

The namespace tests have no test harness, because the harness is
multithreaded and `unshare(CLONE_NEWUSER)` fails in a multithreaded process.
They skip themselves without user namespaces, unless
`SANDFORK_REQUIRE_NS=1`.

Rules: a test that sleeps is wrong (wait on a pidfd, `cgroup.events` or the
file in question). Timing assertions belong in benchmarks, not tests. CI
retries nothing.

## Running the daemon

```bash
make daemon                       # debug build, socket /tmp/sandfork-dev.sock
export SANDFORK_SOCKET=/tmp/sandfork-dev.sock
sf prepare tasks/py-bugfix/sample-0001
sf run tasks/py-bugfix/sample-0001 -- python -m pytest -q visible
```

`scripts/dev-daemon.sh` starts the daemon in a delegated systemd user scope
when it can, which is what gives rootless sandboxes cgroup limits.

## Adding a task family

1. Write the fixture under `tasks/fixtures/<name>/`, with its own tests and a
   `tests/pytest.ini` whose `pythonpath` points at `../workspace/src`, so the
   grader resolves the package from the transplanted workspace.
2. Either add a `FixtureSpec` to `BUGFIX_FIXTURES` in
   `python/sandfork_tasks/generate.py` (mutation-generated tasks) or a
   generator to `families.py` (template tasks with a reference solution).
3. If the family needs a new base, add `tasks/bases/<base>.toml`. Base steps
   run once, as the sandbox's root, with the wheel cache at `/wheels`.
4. Run `make tasks` and `make validate-tasks`, then check `tasks/REPORT.md`.

A task's stateful process (`warm_process`) must call
`sandfork_zygote.serve()` after loading its state, and must write no files.

## Adding a fork mode or snapshot backend

Snapshot backends implement `SnapshotFs` in
`crates/sandfork-layers/src/backend.rs` (`fork_root`, `upper_view`,
`release`; `seal` and `diff` have defaults). Fork modes are dispatched in
`start_one` in `crates/sandforkd/src/lifecycle.rs`, and named in the
`ForkMode` enum in the proto.

## Code rules

* Every crate has a module-level comment stating its invariants.
* Every `unsafe` block carries a `// SAFETY:` comment.
* Comments at every privilege boundary state what is trusted on each side.
* `cargo fmt`, `cargo clippy -D warnings` and `ruff` must pass.
