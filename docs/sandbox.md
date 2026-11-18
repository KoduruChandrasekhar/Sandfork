# One isolated sandbox, from raw syscalls

Code: `crates/sandfork-ns` (the setup sequence), `crates/sandfork-spawner`
(the process that runs it), `crates/sandfork-shim` (PID 1), and
`crates/sandfork-seccomp` and `crates/sandfork-cgroup`.

## Process model

* `sandforkd` is multithreaded and async, and never forks a sandbox.
* `sandfork-spawner` is single-threaded, started before the daemon's async
  runtime exists, and asserts that it has one thread.
* Every sandbox's PID 1 is `sandfork-shim` (or, for an F3 fork, the zygote
  library's reaper running the same shim loop).

At L1 the daemon first enters a long-lived user and mount namespace of its
own, the trick rootless Podman uses. Inside it, it is root and can mount
each sandbox's tmpfs and read writable layers after the sandbox has
pivoted. At L2 it is real root and only takes a private mount namespace.

## Spawner side

1. At L2, or at L1 with a delegated cgroup: create the sandbox's leaf with
   its limits already written, and open it as a descriptor.
2. Create a sync pipe, a report socket, and the control socket pair.
3. `clone3` with user, mount, PID, network, UTS, IPC and cgroup namespaces,
   `CLONE_PIDFD`, and `CLONE_INTO_CGROUP` when there is a leaf. The first
   process therefore starts inside its cgroup with limits set; setting
   limits after processes exist is how fork bombs survive the PID limit.
4. Write the UID and GID maps from outside (`deny` to `setgroups` first
   when unprivileged).
5. Release the child. Wait up to 30 s on the report socket, then kill on
   timeout. The child reports per-step timings, or an error and exits.
   End-of-file with no error means `execve` succeeded, since the socket is
   close-on-exec.

## Child side, in order

| Step | Why it is here |
|---|---|
| Open the shim, runtime and bind sources as `O_PATH` descriptors, `chdir` to the store | still the spawner's kernel UID, which owns the path; later mounts name them through `/proc/self/fd`, so no path lookup under the mapped root is needed |
| Become the mapped root | maps are written; needed for mounts |
| `mount("/", MS_REC \| MS_PRIVATE)` | without it the overlay can propagate back into the host's mount namespace |
| Overlay with relative lowers, `userxattr,volatile` | lowers are read-only by construction; `volatile` skips fsync for throwaway layers |
| Bind the shim, the runtime, the host's `/usr` (read-only, keeping locked flags) and the grader | before `pivot_root`, while host paths are reachable |
| `/proc`, masked like runc (`kcore`, `sysrq-trigger`, timers, …; `sys`, `irq`, `bus`, `fs` read-only) | needs the new PID namespace, which the child is PID 1 of |
| A tmpfs `/dev` with bound `null zero full random urandom tty` and `fd`/`std*` symlinks | `mknod` is not allowed in a user namespace |
| tmpfs `/tmp` and `/run` | |
| `pivot_root(".", ".")`, then `umount2(".", MNT_DETACH)` | the documented put-old-less pivot; the overlay is a mount point |
| hostname `sandbox`, `lo` up by `SIOCSIFFLAGS` | no netlink needed |
| rlimits: NOFILE, CORE = 0, NPROC and FSIZE as backstops | |
| Cut capabilities to the shim's eight; `no_new_privs`; `PR_SET_PDEATHSIG` | the shim needs SETUID/SETGID to drop to the agent, DAC overrides to serve files under the agent's filesystem UID, KILL for its children |
| Keep only the control channel (fd 3); `close_range(5, …)` | no inherited descriptor reaches the agent |
| Seccomp, last | it forbids the calls above |
| `execve("/.sandfork/shim")` | |

## The shim

1. **Reap.** It loops `wait4(-1, WNOHANG)` on every wakeup (signals
   coalesce), and counts reaped orphans.
2. **Run commands.** It forks; the child calls `setsid` and resets its
   signals, drops to the agent user (or, with a single-ID mapping, drops
   every capability), wires pipes, and `execvp`s. At the timeout the shim
   kills the whole process group.
3. **Bound its own memory.** It forwards up to `max_output` bytes per
   stream, then keeps an 8 KiB tail. Output can be gigabytes; the shim's
   memory stays fixed.
4. **Serve files** through `openat2` (`RESOLVE_BENEATH` from the workspace
   for relative paths, `RESOLVE_IN_ROOT` for absolute ones, no magic links),
   with `setfsuid` to the agent. Path resolution is never hand-rolled.
5. **Signals as PID 1.** SIGTERM from outside the PID namespace (sender PID
   0) shuts down; from inside it is ignored, like SIGINT and SIGHUP. The
   kernel never delivers SIGKILL to PID 1 from inside.

Messages are length-prefixed `postcard` frames of one shared Rust enum
(`sandfork_core::wire`), linked by the daemon, spawner, shim and zygote, so
the protocol cannot drift.

## cgroup v2

```
<root>/daemon/                  the daemon and spawner (delegated scopes only)
<root>/sandfork.slice/
    group-<id>/                 one per GRPO group: aggregate accounting
        sbx-<id>/               one per sandbox: limits; processes live only here
```

The leaf gets `memory.max` from the task, `memory.high` at 90% of it,
`memory.swap.max = 0`, `memory.oom.group = 1`, `pids.max` and `cpu.max`. On
teardown the daemon reads `memory.peak`, `memory.events` and `cpu.stat`
before the leaf goes. An OOM kill is a task outcome, not an infrastructure
error.

## Measured

Creation-latency breakdown for one L1 sandbox on the WSL2 laptop, debug
build, from the child's `CLOCK_MONOTONIC` marks (`SANDFORK_SHOW_STEPS=1`
with the live test), in microseconds:

| clone3 | maps | overlay | binds | proc | dev | tmp | pivot_root | net | privs | seccomp | exec |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1291 | 109 | 372 | 175 | 209 | 183 | 56 | 155 | 255 | 50 | 436 | 2165 |

That is about 5.5 ms from request to the shim running. Fork latency to a
usable group, per mode and group size, is E1 in `benchmarks.md`. Open items:

* the 1,000-creation p50/p99 breakdown;
* concurrency from one spawner against four;
* L0/L1/L2 on GitHub Actions;
* the 10,000-cycle leak run (the live test does 50).

## Done-when check

With the daemon at level l1 (`sandforkd --level l1`; the level is
daemon-wide), `sf run <task> -- sh -c 'id; hostname; ls /; cat /proc/1/cmdline; ip link'`
shows a mapped UID, hostname `sandbox`, only task files and system binds,
the shim as PID 1, and only `lo`. The `identity` case of the live test
asserts all of this.
