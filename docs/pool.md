# The pool, the group-aware scheduler and failure handling

Code: `crates/sandforkd/src/{admission,scheduler,reconcile,lifecycle}.rs`.

## Sizing

The average number of sandboxes in use is `L = λ W` (Little's law). In
synchronous training with P prompts of G rollouts, L is about P × G at the
peak.

A multi-turn rollout alternates between the model generating (the sandbox
idle) and a command running (busy). With duty cycle d and C cores, about
C / d sandboxes keep the CPUs busy, so memory usually sets the cap:

    N_max ≈ min(C / d, M_free / m_private)

Here m_private is the private memory per sandbox, measured as PSS (see
`zygote.md`). Measuring d per family is an open item; E3 in `benchmarks.md`
measures throughput against pool size with a scripted agent.

## Admission control

* **Gang admission.** `ForkGroup(G)` starts only when all G sandboxes fit,
  otherwise it waits, first in first out within a priority, until a deadline.
  Only the head of the queue may be admitted, so small groups cannot starve a
  large one. If any sandbox of an admitted group fails to start, the whole
  group is torn down: a partial group is useless to GRPO.
* **Memory-aware.** Each sandbox is charged an estimate: the 95th
  percentile of peak private memory for its task family, once eight
  rollouts have been seen, else the task's limit (when a cgroup enforces it)
  or `--default-mem-mb`. A group that could never fit is refused at once
  (`group_too_large`) instead of waiting out its deadline.
* **Priorities.** Verification forks first, restores of in-flight rollouts
  second, new groups last.
* **A reserve for work in flight.** Priority orders the queue but cannot
  preempt. New groups may use only 85% of the memory budget and all but one
  slot in eight. The rest is reserved for verification and restores.

  This was a real deadlock found while validating tasks. Each task's two
  rollout sandboxes filled the pool, and each then waited for a verification
  fork that could never be admitted. The regression test is
  `verification_can_always_start_when_new_groups_fill_their_share`.

Property-style test: `invariants_hold_under_random_load` runs 200 random
pools with 300 random arrivals and releases each, several times
oversubscribed. It asserts that the pool is never overcommitted in slots or
memory and that no group is ever partially admitted.

## Difficulty-band scheduling

A group whose rewards are all equal has zero advantage everywhere. With
binary rewards at pass rate p, the chance a group of G is wasted is
`p^G + (1 − p)^G`:

| p | 0.05 | 0.10 | 0.20 | 0.50 | 0.80 | 0.95 |
|---|---|---|---|---|---|---|
| wasted at G = 8 | 66% | 43% | 17% | 0.8% | 17% | 66% |

The scheduler (`PickTasks`) works in three steps:

1. It keeps a Beta posterior per task, updated from every verified rollout
   (a reward at or above the task's `pass_threshold` counts as a pass).
2. It multiplies alpha and beta by 0.95 each epoch, because the policy
   improves.
3. It samples Thompson-style: draw p, score the task `1 − p^G − (1 − p)^G`,
   and pick the top.

`strategy = "uniform"` is the baseline. The trainer holds out a uniformly
sampled evaluation set to catch overfitting to the band. In a simulation of
twenty tasks spread from 2% to 98%, the unit test
`the_band_wastes_fewer_groups_than_uniform_sampling` requires a
useful-group fraction above 85% and at least 10 points above uniform. With
the skewed pass-rate mix in E5 the gain is smaller; see `benchmarks.md`.

## Stragglers

Every rollout has a turn limit and a wall-clock limit. Once half of a
group's members have been verified, the CPU weight of the members still
running is raised (`cpu.weight = 300`). A group deadline (verify the rest
as they stand) is not implemented.

## Failure handling

* **Three levels of timeout.**
  * A command: the shim kills its process group at the deadline.
  * A turn: the orchestrator.
  * A sandbox call: the daemon treats a shim that misses its deadline as
    dead.
* **Leases.** A group holds a lease that the SDK renews in the background,
  with a fencing token. If the orchestrator dies, the group is reclaimed
  when the lease expires (`test_an_abandoned_group_is_reclaimed_when_its_lease_expires`).
* **Liveness.** Every idle shim is pinged each second; a sandbox whose
  pidfd is readable is marked dead. A dead spawner is restarted.
* **Reconciliation,** at start-up and every five seconds. It compares the
  database with reality (rollout directories, cgroups under
  `sandfork.slice`, mounts) and tears down anything no live sandbox owns.
  It is idempotent. A grace period keeps it away from sandboxes still being
  created.
* **Daemon crash.** The daemon's death kills the spawner, and the spawner's
  kills every sandbox (`PR_SET_PDEATHSIG` down the chain). The daemon's
  private mount namespace takes its mounts with it. On restart, start-up
  cleanup removes rollout directories, cgroups and stale rows. This was
  exercised for real once, when the WSL VM restarted under a running daemon.
  The 50-SIGKILL soak from the plan has not been run.
* **Leak counters.** `leak_mounts`, `leak_rollout_dirs`, `leak_cgroups`,
  `leak_zombies` and `daemon_open_fds` are exported in `Stats` and shown by
  `sf top`. The live test asserts no leaked mounts, directories or rows
  after 50 create/destroy cycles. The plan's 10,000-cycle run and 24-hour
  soak are open.

## Teardown order

1. SIGTERM to PID 1 through its pidfd (from outside the PID namespace it
   reaches the shim, which kills everything), then SIGKILL as the backstop,
   then `cgroup.kill`.
2. Wait until the pidfd is readable and `cgroup.events` reports
   `populated 0`.
3. Read `memory.peak`, `memory.events` and `cpu.stat`.
4. Unmount and remove the writable layer.
5. Remove the cgroup.
6. Return the admission share.
