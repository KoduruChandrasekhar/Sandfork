# The zygote and copy-on-write fork (F3)

Code: `crates/sandfork-zygote` (the library), `python/sandfork_rt/`
(the Python wrapper and the stateful kernel), `crates/sandforkd/src/zygote.rs`
(the pool).

## Problem

When the prepared state lives in process memory, a filesystem snapshot is
not enough. A data-analysis task's prepared state is a Python kernel with
pandas imported and a DataFrame parsed. A cold sandbox pays the import and
the parse every time. A zygote holds that state once, and every sandbox is a
copy-on-write fork of it.

## Lifecycle

1. The daemon starts a sandbox for the zygote from the task's prepared
   snapshot, as a `Zygote` flavour. It keeps its capabilities and has **no
   seccomp filter**, because filters are inherited and can never be removed.
   It runs task-author code only, never agent code. The layer store and the
   rollout directory are bound into its root: read-only at
   `/.sandfork/store/layers`, and read-write at `/.sandfork/rollouts`.
2. The shim starts the task's `warm_process` (for example
   `python sandfork_kernel.py --loader /workspace/load.py`) with the zygote
   channel on descriptor 3. The program imports, loads, and calls
   `sandfork_zygote.serve()`.
3. `serve()` settles the collector (`gc.collect()`, then `gc.freeze()`),
   calls `PyOS_BeforeFork()`, and enters the library, which runs the hygiene
   checks and then loops forever, forking a sandbox per request.

## Fork hygiene: checks before a zygote may serve

`serve()` refuses to start and names the failed check:

| Check | Why |
|---|---|
| exactly one thread (`/proc/self/task`) | a child of a multithreaded process inherits locks held by threads that no longer exist; BLAS and OpenMP pools are the usual cause, so the daemon sets `OMP_NUM_THREADS=1` and friends for every sandbox |
| no file open for writing | siblings would share one open file description and its offset: a hidden channel |
| no sockets or pipes except the control channel | same reason |
| empty writable layer | children re-root onto the lower layers plus their own empty layer; anything the zygote wrote would vanish for them while its memory still assumed it existed |
| no child processes | children are not inherited across fork |
| no timers, no pending signals | a timer or signal would fire in every child |
| settled garbage collector | done by the wrapper: `gc.collect()` then `gc.freeze()` |

The writable-layer check caught a real bug during development: a mount point
the zygote needed was missing from the skeleton layer, so creating it at
start-up wrote into the zygote's upper directory. The fix added the mount
point to the skeleton and versioned the base cache key. Rejection tests:
`hygiene::tests` (threads, writable file or pipe, non-empty layer).

## What happens in one fork

1. The zygote (Z) receives the sandbox's configuration and a fresh control
   socket over `SCM_RIGHTS`. A shared socket would be one open file
   description, shared by siblings.
2. Z forks A. A unshares user, mount, network, UTS, IPC, PID and cgroup
   namespaces; it is single-threaded, so `CLONE_NEWUSER` is allowed. Z,
   root in its own user namespace, writes A's maps.
3. A forks B, which is PID 1 of the new PID namespace, and exits.
4. B builds the world with **the same code the spawner's child uses**
   (`sandfork_ns::child::build_world`): a new overlay with the same lowers
   and its own empty writable layer, then `/proc`, `/dev`, `/tmp`,
   `pivot_root`, hostname, loopback, rlimits, capabilities and
   `no_new_privs`. After the pivot the store is no longer visible to
   anything the agent runs.
5. B opens a pidfd on itself and sends it to the daemon. The daemon moves B
   into its cgroup leaf and answers. B installs seccomp.
6. B forks C. **B stays PID 1 and runs the shim loop in Rust**: it reaps,
   runs commands, serves files, and never touches (so never un-shares) the
   interpreter's pages. C drops to the agent user, redirects its output to
   the sandbox's own `/tmp/sandfork-kernel.log`, returns from `serve()`
   after `PyOS_AfterFork_Child()`, and becomes the sandbox's stateful
   process.

A forked F3 sandbox is therefore two processes: the reaper as PID 1 and the
kernel as PID 2 (`ps` shows both as `python …` because B never execs).

## The pool

* One zygote per **prepared state** (layers plus warm-process command), not
  per task. Tasks with identical prepared layers share one: all 200
  data-analysis questions over one dataset fork from a single loaded kernel.
* Retired after `--zygote-max-forks` (500) forks. Its children live in its
  PID namespace, so a retired zygote is destroyed only once its last child
  is released.
* Destroyed after five minutes unused.

## Memory accounting

cgroups charge a page to the cgroup that first faulted it in. The shared
pages therefore belong to the zygote's cgroup, and each child's cgroup sees
only its private pages: a child's `memory.max` limits its *private growth*,
not the shared base. On the laptop, four F3 sandboxes of the 200k-row
dataset showed 1.5 to 8 MB each in their own cgroups while the zygote held
65 MB. Admission therefore estimates memory per sandbox from the family's
history of peak *private* memory (95th percentile), falling back to the
task's limit.

## Measurements

See E2 in `benchmarks.md`: total PSS of G sandboxes, F3 against F2, after
the fork, after a read-only query, and after copying the DataFrame. Open
items: private-dirty growth against time with and without `gc.freeze()`,
columnar against object-heavy data (the loader supports
`SANDFORK_DATA_LAYOUT=objects`), and fork latency against zygote size with
transparent huge pages on and off.

## Invariants

* The zygote never runs agent code.
* Every child installs seccomp before any agent code runs, and before the
  stateful process is forked.
* The zygote's writable layer is empty when it serves.
