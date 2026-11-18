# Checkpoints, restore and crash recovery

## What is implemented

A checkpoint in this build is **filesystem-only**: the sandbox's writable
layer sealed as an incremental layer (`docs/layers.md`) under a cgroup
freeze, recorded in SQLite with the rollout's turn. Restoring a checkpoint
forks a fresh sandbox from those layers, with restore priority in admission.
For sandboxes that run nothing between turns, which is every agent turn in
the bug-fix, math and shell families, that is the sandbox's whole state.

* `Checkpoint(sandbox, turn)` seals what changed since the last seal and
  returns a checkpoint ID (`has_process_image = false`).
* `Restore(checkpoint, n)` first checks that the checkpoint's top layer
  still matches its manifest. A restore over different files would give
  state that assumes files that are not there. It then forks `n` sandboxes.
* Releasing a sandbox deletes its checkpoints (the rollout is over).
  Checkpoints otherwise outlive a daemon crash and expire after 24 hours.

### Crash recovery of a rollout (`python/sandfork_rl/rollout.py`)

1. **Detect.** A command fails because the sandbox's PID 1 died (the daemon
   watches its pidfd), the shim missed its heartbeat or a deadline, or the
   sandbox is gone (lease expiry, daemon restart).
2. **Classify.** An out-of-memory kill (`oom_kill` in `memory.events` went
   up) is the task's outcome: the error class is `TASK` and the state is
   graded as usual. Anything else is infrastructure.
3. **Recover.** Restore the last checkpoint, taken after turn j. Truncate
   the trajectory to turn j, dropping tokens and observations after it, and
   continue with the current policy. Kept tokens still carry the behaviour
   log-probabilities of the policy that generated them, so the rollout
   remains a valid sample. It is marked `resumed`.
4. If there is no checkpoint, or the restore fails, the rollout is an
   infrastructure error with **no reward**. Never a zero.

Tested end to end:

* `test_a_killed_sandbox_resumes_from_its_last_checkpoint` kills the
  sandbox's PID 1 with SIGKILL from the host before turn 2. The rollout
  restores from the turn-1 checkpoint and still scores 1.0.
* `test_without_a_checkpoint_a_dead_sandbox_is_an_infra_error_not_a_zero`.

## What is not implemented

**Process checkpoints with CRIU, and fork mode F4 (restore fork).** The
daemon reports F4 as `f4_unavailable`. Branching or restoring a task that
keeps state in a process (data-analysis) is refused with a clear error
rather than silently losing the kernel's memory. On the development machine
CRIU was not installed (the distribution's 4.2 is below the 4.2.1 that fixes
CVE-2026-73860) and there was no passwordless root, so nothing here could
have been verified. This remains the largest open part of the plan
(Part 5). When it is built, the rules from the plan stand:

* CRIU runs only in a separate root helper that alone can write the image
  directory.
* Every restore is verified: the restored tree's credential, capability and
  seccomp lines in `/proc/<pid>/status` are compared with those recorded at
  dump time, and the tree is killed on any mismatch.
* The UID mapping remains the second line of defence.

What CRIU cannot checkpoint (GPU state, some devices, traced processes,
io_uring, some inotify set-ups, resources shared outside the tree) should be
documented before it is relied on.
