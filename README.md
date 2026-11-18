# Sandfork

Sandfork is a sandbox service for reinforcement-learning rollouts. It prepares
each task's environment once, snapshots it, and forks a whole GRPO group of
isolated sandboxes from that snapshot, instead of building G identical
environments from cold. It is written directly on Linux primitives
(namespaces, overlayfs, cgroup v2, seccomp, `fork`) and targets one ordinary
machine with no KVM: a WSL2 laptop or a CI runner. A small GRPO loop sits on
top as the test harness.

**What is not new:** one sandbox per rollout, snapshot-and-fork of sandboxes,
zygote provisioning, container-free namespace sandboxes and separate graders
all exist already (see *Prior art*).

**What Sandfork adds:**

* the GRPO group as the unit of the API, admitted as a gang;
* three fork modes measured against each other on the same machine;
* zygote forks of stateful agent tools;
* difficulty-band task scheduling;
* verification by transplant.

> **Status.** The environment layer runs end to end on the development
> laptop (WSL2, rootless). Not done, and stated where it matters:
>
> * process checkpoints and the restore fork mode (F4, CRIU);
> * every GPU experiment, including the E4 figure and the training run;
> * the dedicated escape suite and red-team pack;
> * measurements on any machine other than the laptop.
>
> See *Limitations*.

**Measured on the WSL2 laptop** (release build, machine not idle; `docs/benchmarks.md`):

* **Startup time, group of 8 sandboxes.** A filesystem fork (F2) is ready in 1.5 s, against 14.3 s for a cold start (F1) on the py-bugfix family.
* **Data-analysis tasks.** A zygote fork (F3) is ready in 2.2 s, against 7.6 s for F2.
* **Memory.** Eight zygote-forked data-analysis sandboxes use 123 MB of PSS in total, against 453 MB for eight F2 sandboxes.
* **Branching.** A 2×4 tree cuts command time per leaf by about 8 times and prefill tokens per leaf by about 2.8 times.

## Architecture

```
            sf / Python SDK / rollout orchestrator + GRPO trainer
                                   │ gRPC (sandfork.v1, Unix socket)
┌──────────────────────────────── sandforkd ────────────────────────────────┐
│ admission (gang, memory-aware, priorities, reserve)   scheduler (Beta band)│
│ idempotency cache · leases · reconciliation · leak counters · metrics      │
│ layer store (content-addressed, SQLite, GC, squash)   verifier (transplant)│
└──────┬──────────────────────────────┬───────────────────────────┬─────────┘
       │ spawn requests               │ fork requests             │ control channels
 sandfork-spawner              zygote (one per prepared state)    │
 (single-threaded)             holds the live prepared process    │
       │ clone3 + namespaces          │ fork, unshare, re-root     │
       ▼                              ▼                           ▼
   ┌────────┐ ┌────────┐ ┌────────┐ … G sandboxes: own namespaces, cgroup leaf,
   │ shim   │ │ shim   │ │ reaper │   empty writable layer on shared read-only
   │ PID 1  │ │ PID 1  │ │ +kernel│   lower layers
   └────────┘ └────────┘ └────────┘
```

The life of one group:

1. `ForkGroup(task, G)` is gang-admitted: all G sandboxes or none.
2. The first time, the task is prepared once: layers built, steps run,
   sealed as the prepared snapshot.
3. G sandboxes are forked from the snapshot (F2), or from the task's zygote
   (F3). Each gets its own namespaces, cgroup and empty writable layer.
4. The orchestrator runs G rollouts concurrently: token IDs to the
   generator, tool calls through the daemon.
5. Between turns, sandboxes may checkpoint (crash recovery) or branch.
6. Each final state is transplanted onto a fresh fork and graded against
   hidden tests the rollout never contained.
7. G rewards come back as one group, and the trainer steps.
8. The group is released, and its trace is written for Perfetto.

## Fork modes

| Mode | What is copied | What is shared | Status |
|---|---|---|---|
| F1 cold | everything: preparation runs again in every sandbox | the base layer | implemented (the baseline) |
| F2 filesystem fork | nothing at fork time; files copied up on first write | all prepared layers | implemented (default) |
| F3 zygote fork | only pages a sandbox writes | prepared layers and one live prepared process | implemented |
| F4 restore fork | process memory, once per restore | prepared layers | **not implemented** (needs CRIU) |

Use F2 when the prepared state is files, and F3 when it lives in a process
(`AUTO` picks F3 when a task names a `warm_process`). Measured numbers:
`docs/benchmarks.md`.

## Isolation

| Level | Mechanisms | Runs on |
|---|---|---|
| L0 process | own session, rlimits, a private copy of the working directory, timeouts | anywhere |
| L1 rootless namespaces | user, mount, PID, network, UTS, IPC, cgroup namespaces; overlayfs; `pivot_root`; seccomp; `no_new_privs`; reduced capabilities; cgroup v2 limits when delegated | WSL2 (tested) |
| L2 namespaces as root | L1 plus root-owned cgroups and per-sandbox UID ranges | implemented, not exercised here |

This is **not a production security boundary**: L1 and L2 share the host
kernel. Threat model, what is checked and how: `docs/security.md`.

## Task families

`tasks/REPORT.md` has the full report.

| Family | Agent does | Tasks |
|---|---|---|
| `py-bugfix` | fixes a mutation-injected bug in one of three fixture libraries (intervals, LRU cache, rate limiters) | 290 |
| `py-heavy` | the same, over a ledger library with a seeded 13 MB SQLite database built at preparation | 126 (short of 200) |
| `math-tool` | word problems with a calculator tool | 200 |
| `data-analysis` | questions over a 200k-row DataFrame already loaded in a stateful kernel (F3) | 200 |
| `shell-ops` | reorganize files, edit configuration, write a script | 200 |

Bug-fix tasks are kept only if:

* the mutant fails 1 to 5 hidden tests;
* it is not a duplicate;
* the inverse mutation restores the source exactly and passes three runs in
  a row.

Template tasks are validated by grading a reference solution (1.0) and the
untouched state (0.0). Samples of every family are re-validated through the
daemon, grading each twice for determinism.

## The RL loop

`python/sandfork_rl` (`docs/rl.md`) provides:

* token-exact trajectories, asserted on every step;
* loss masks proven by a gradient test;
* Dr. GRPO advantages, with GRPO and RLOO as options;
* token-level, asymmetrically clipped loss;
* a training-inference mismatch metric logged every step;
* LoRA through PEFT;
* sync and async modes;
* branched rollouts;
* Chrome-trace output.

It runs end to end against the daemon with scripted agents and has been
trained on CPU with a tiny random model. **No GPU run has been made.**

## Reproduce

On Linux or WSL2 (`CONTRIBUTING.md` has the setup):

```bash
make venv
bash scripts/wheels.sh        # wheel cache for base layers (sandboxes have no network)
make build
make doctor                   # feasibility probes
make test-local               # lint plus every test layer
make tasks                    # generate and validate the five families
make bench-t3                 # every measurement-only experiment, then charts
```

Raw results are in `bench/results/`, with a machine fingerprint in every
file. Charts come from those files only (`bench/charts.py`).

## Configuration reference

`sandforkd` flags (also `sandforkd --help`):

| Flag | Default | Effect |
|---|---|---|
| `--state-dir` | `$XDG_DATA_HOME/sandfork` (L1), `/var/lib/sandfork` (L2) | layer store, rollout directories, SQLite |
| `--socket` | `$XDG_RUNTIME_DIR/sandfork.sock` | Unix socket, mode 0600 |
| `--level` | `auto` | `l0`, `l1`, `l2`; `auto` picks l2 as root, else l1, else l0 |
| `--wheels-dir` | `<state>/wheels` | bound read-only at `/wheels` while bases and tasks are prepared |
| `--bin-dir` | next to the daemon | where `sandfork-spawner` and `sandfork-shim` are |
| `--rt-dir` | `<bin-dir>/rt` | zygote library and kernel, bound at `/.sandfork/rt` |
| `--memory-budget-mb` | 60% of available | admission budget; new groups get 85% of it |
| `--max-sandboxes` | 64 | slot limit; one in eight is reserved for verification and restores |
| `--spawners` | 1 | spawner processes |
| `--lease-seconds` | 120 | default group lease |
| `--squash-k` | 16 | squash a checkpoint chain past this many layers |
| `--gc-grace-seconds` | 600 | before unreferenced layers are collected |
| `--disk-budget-mb` | 0 (none) | evict unpinned snapshots LRU above this |
| `--default-mem-mb` | 192 | memory estimate with no history and no enforced limit |
| `--cgroups` | `auto` | `off` disables limits |
| `--rollout-fs` | `auto` | `tmpfs` (a hard disk quota; Linux 6.6+ rootless) or `dir` |
| `--zygote-max-forks` | 500 | retire a zygote after this many forks |
| `--uid-base` | 100000 | L2: host UID of sandbox UID 0 |

`task.toml` keys:

| Section | Key | Default | Effect |
|---|---|---|---|
| `[task]` | `id`, `family` | required | identity; the family groups memory history and scheduling |
| | `base` | `python` | base layer, `tasks/bases/<base>.toml` or a built-in host base |
| | `timeout_s`, `max_turns` | 180, 12 | rollout limits |
| `[prepare]` | `fixture`, `mutation` | none | shared source tree, and a line-level edit applied on top |
| | `steps` | `[]` | run once at preparation, as the agent user |
| | `warm_process` | none | stateful process; must call `sandfork_zygote.serve()` |
| | `exclude` | `tests`, caches | fixture entries kept out of every rollout layer |
| `[agent]` | `prompt`, `tools` | `prompt.md` | what the agent sees and may call |
| | `allowed_paths` | `[]` | globs, relative to the workspace; only these are graded |
| `[verify]` | `kind` | required | `pytest` or `script` |
| | `grader`, `command`, `timeout_s` | `hidden`, required, 30 | mounted at `/grader` only when grading |
| | `reward`, `total`, `pass_threshold`, `version` | `fraction_passed`, collected, 1.0, `<kind>-v1` | scoring |
| `[limits]` | `memory_mb`, `pids`, `cpu`, `disk_mb` | 1024, 256, 1.0, 512 | cgroup limits and the writable layer's size |

## Code facts

* Rust crates under `crates/`:
  * `sandfork-core`: types, wire protocol, task format;
  * `sandfork-ns`: the setup sequence;
  * `sandfork-seccomp`;
  * `sandfork-cgroup`;
  * `sandfork-layers`;
  * `sandfork-shim`;
  * `sandfork-spawner`;
  * `sandfork-zygote`;
  * `sandfork-proto`;
  * `sandforkd`;
  * `sandfork-cli` (`sf`).
* Python packages under `python/`: `sandfork` (SDK), `sandfork_rl`,
  `sandfork_tasks`, and `sandfork_rt` (runs inside sandboxes).
* About 16,000 lines of Rust and 4,800 of Python (excluding generated code and tests' fixtures).
* 135 `unsafe` blocks and 3 `unsafe fn`s, every one with a `// SAFETY:` comment stating why it is sound.

## Limitations and non-goals

Limitations:

* **No CRIU.** Checkpoints are filesystem-only. F4 is unavailable.
  Data-analysis sandboxes cannot be branched or restored (the daemon
  refuses rather than losing state).
* **One machine measured,** and not an idle one. The Kaggle and Actions
  columns of `docs/platform_report.md` are empty.
* **No GPU work:** the E4 figure, the training run and its F1 control are
  open.
* **No escape suite or red-team pack** beyond the checks in the live test.
* **py-heavy has 126 tasks,** not 200.
* The daemon serves only a Unix socket; remote use needs an authenticated
  tunnel.
* Single-ID mapping without `newuidmap`: there is no separate agent user,
  only dropped capabilities.

Non-goals: more than one host, GPUs inside sandboxes, Windows or macOS
hosts, production-grade security, a strong model.

## Prior art and references

* AgentENV: Firecracker microVM sandboxes that fork.
* GKE Agent Sandbox: gVisor with warm pools.
* Hugging Face Sandboxes.
* Vercel Sandbox: forking.
* SWE-MiniSandbox: container-free namespace sandboxes for SWE RL.
* SOCK (Oakes et al., USENIX ATC 2018): zygote provisioning.
* Catalyzer (Du et al., ASPLOS 2020): restore instead of initialize, and
  `sfork`, "sandbox fork", the closest prior use of the idea in this
  project's name.
* MITOSIS (OSDI 2023).
* REAP (ASPLOS 2021).
* Baumann et al., *A fork() in the road* (HotOS 2019).
* DeepSeekMath (GRPO).
* DAPO.
* Liu et al. (Dr. GRPO).
* Qi et al. (fp16 against the training-inference mismatch).
