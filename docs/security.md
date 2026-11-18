# Security model and reward-hacking defences

**This is not a production security boundary.** L1 and L2 share the host
kernel, and a kernel bug reachable through the allowed syscall surface is an
escape.

## Threat model

* **Assets:** host files and processes, sibling sandboxes, the grader and
  its hidden tests, the training data, the network, the daemon.
* **Adversary:** arbitrary model-generated code under optimization pressure.
  Not malicious intent, but any reachable path to a higher reward will
  eventually be found and reinforced.
* **Out of scope:** host kernel exploits at L1 and L2, timing and cache side
  channels, and protecting the host from deliberate resource exhaustion at
  L0.

## Isolation, and what checks it

The setup order is in `sandbox.md`. Every check below runs in
`crates/sandfork-spawner/tests/live.rs` against real L1 sandboxes, as the
sandbox's root as well as the agent, so that the seccomp filter, not a
missing capability, is what refuses:

| Property | Enforced by | Checked by |
|---|---|---|
| Host files are not in view; `/home` holds only the agent's directory | mount namespace, `pivot_root` | `identity` |
| Only the sandbox's own processes exist; PID 1 is the shim | PID namespace | `identity` |
| PID 1 survives signals from inside | init of its PID namespace; the shim ignores in-sandbox SIGTERM | `basic_escapes` |
| A write to a lower-layer file lands in the writer's own layer; siblings see nothing | overlayfs | `sibling_isolation_and_quota` |
| Namespace and mount syscalls are refused (`EPERM`; `clone3` returns `ENOSYS`) | seccomp | `basic_escapes`, and every denied call in `sandfork-seccomp`'s tests |
| No network beyond loopback | network namespace | `identity`, `basic_escapes` |
| `/proc/sys` read-only; no `/dev/kvm` or `/dev/mem` | `/proc` masking, a tmpfs `/dev` with bound nodes only | `basic_escapes` |
| Filling the disk gives `ENOSPC` inside, siblings unaffected | per-sandbox tmpfs | `sibling_isolation_and_quota` |
| Descriptor exhaustion gives `EMFILE` | `RLIMIT_NOFILE` | `sibling_isolation_and_quota` |
| The file API cannot leave the sandbox (`..`, outward symlinks, symlinked parents) | `openat2` with `RESOLVE_BENEATH` / `RESOLVE_IN_ROOT`, under the agent's filesystem UID | `file_api` |
| Timeouts kill a command's whole process group | `setsid` plus `kill(-pgid)`; `cgroup.kill` at teardown | `timeouts_and_output` |
| Memory, PID and CPU limits | cgroup v2 (`memory.max`, `pids.max`, `cpu.max`) | applied when the daemon has a delegated cgroup; not yet asserted by a test |

**Not part of this build:** the plan's dedicated escape suite with a
per-level results table run in CI at L1 and L2, and the red-team pack of ten
hand-written exploit "solutions". The checks above cover a subset at L1
only.

## The seccomp policy

A denylist, as data (`crates/sandfork-seccomp`). It is installed last,
because setup needs the calls it forbids. The groups:

* mounts and namespaces;
* `clone` with any namespace bit;
* `clone3` always, with `ENOSYS`, so glibc falls back to `clone`, whose
  flags a filter can read;
* reading or controlling other processes;
* kernel interfaces with long vulnerability histories (`bpf`,
  `perf_event_open`, `userfaultfd`, io_uring, keyrings);
* host administration;
* file-handle calls and legacy interfaces;
* any non-native architecture, which kills the process.

The unit test forks a child per denied call, installs the filter, makes the
call and asserts the errno (decision 0003).

## UID mapping

With subordinate IDs and `newuidmap`, every sandbox maps its 65,536 IDs onto
the user's subordinate range, so a process escaping a sandbox is not the
invoking user. Without them (the development machine), the mapping is a
single ID: the sandbox's root is the invoking user, so agent commands run
with every capability dropped and `no_new_privs`. Per-sandbox UID slots,
which would make siblings different host users, are an L2 feature.

## Verification by transplant

The agent's environment is never run for grading
(`crates/sandforkd/src/verify.rs`):

1. **Diff** the rollout's changes against the task's prepared snapshot,
   including turn layers if the sandbox was forked from a checkpoint.
2. **Filter.** Keep only changes under `allowed_paths`. Discard everything
   else and list it in `rejected_paths`: tests, configuration, files that
   change interpreter or test-runner start-up, and bytecode caches. A
   denylist of such names applies even under allowed paths.
3. **Transplant** the kept changes onto a fresh fork of the prepared
   snapshot. No processes and no other files come along.
4. **Mount the grader** read-only at `/grader`. It was never in any rollout
   layer.
5. **Run the task's fixed command** with its own timeout and no network.
   For pytest the summary must agree with the exit status, or nothing
   counts as passed. The denominator is pinned per task (`verify.total`), so
   collecting fewer tests cannot raise the score.

An infrastructure failure gives `status = infra_error` with **no reward**,
never zero.

What remains possible is gaming inside the allowed paths themselves.
Hidden suites are supersets of the visible tests, which catches most
hard-coding. For the rest, see monitoring.

## Monitoring

* `verify_rejected_paths_total` counts rejected edits. A rising rate means
  the policy is learning to attack the grader.
* A static scan of accepted files flags test-framework awareness, early
  exits and patched built-ins in `diff.flags`, counted as
  `verify_flagged_diffs_total` for a review queue.
* Grader determinism: `sandfork_tasks.validate_daemon` grades each sampled
  task's reference twice and fails on disagreement.

## Review log

No training run has been made, so no rollouts have been reviewed.
