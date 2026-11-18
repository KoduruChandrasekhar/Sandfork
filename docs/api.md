# The control-plane API

Contract: `proto/sandfork.proto`, package `sandfork.v1`. Rust types are
generated in `crates/sandfork-proto`, Python stubs in `python/sandfork/v1`
(`scripts/gen-proto-py.sh`). A breaking change means `v2`.

## Calls

| Call | Does |
|---|---|
| `PrepareTask` | build (or find) a task's prepared snapshot; idempotent by spec hash |
| `ForkGroup` | gang-admit and start G sandboxes from a snapshot in mode AUTO/F1/F2/F3 |
| `Exec` | run a command; streams stdout and stderr chunks, then an exit event |
| `ReadFile`, `WriteFile` | size-capped file access, resolved inside the sandbox root |
| `Eval` | one request to the task's stateful kernel (data-analysis) |
| `Snapshot` | seal the sandbox's changes between turns |
| `Fork` | branch a running sandbox (snapshot, then fork) or fork any snapshot |
| `Verify` | verification by transplant (`security.md`) |
| `Checkpoint`, `Restore` | filesystem checkpoints (`checkpoint.md`) |
| `Release` | one sandbox or a whole group; always completes |
| `RenewLease` | extend a group's lease, guarded by its fencing token |
| `Diff` | what a sandbox changed, with unified diffs |
| `ListSandboxes`, `Stats` | inspection; Prometheus text included |
| `RegisterTasks`, `PickTasks` | the difficulty-band scheduler |
| `Gc`, `Drain` | maintenance |

## Error classes

Every error carries `sandfork-error-class` and `sandfork-error-code` in its
trailing metadata:

* `TASK`: belongs to the trajectory (a command failed or ran out of
  memory, a file does not exist, a path escapes the sandbox). A failing
  command is *not* an error at all: it is an exit event with its code.
* `INFRA`: the rollout is retried or dropped (a sandbox died, the shim
  missed a deadline, the spawner failed, admission timed out).
* `CLIENT`: a bad request (unknown snapshot, bad mode, group too large).

## Idempotency

Every mutating call carries a `request_id`. The daemon runs the operation in
its own task, detached from the connection, and keeps the outcome in a
bounded cache (8192 entries). A retry returns the stored result. A retry of
a call that is still running attaches to it. `Exec` keeps a per-sandbox
record of its last 64 commands, and a retry replays the original command's
events from the start. Infrastructure errors are not cached, so a retry
after one runs again.

Proven by `test_exec_retried_with_the_same_request_id_never_runs_twice`. The
client cancels each of twelve calls at a random point and retries with the
same ID, and a counter file in the sandbox shows each command ran exactly
once.

## Semantics

* **Leases.** A group's lease is renewed by the SDK every third of its
  length. An orchestrator that crashes loses its groups when the lease
  expires.
* **Streaming.** The final exec event carries the exit code or signal, wall
  and CPU time, peak RSS, whether output was truncated, the bytes dropped,
  the tail of each stream, and whether the cgroup recorded an OOM kill.
* **Sizes.** Messages are capped at 16 MiB. File writes are chunked at
  256 KiB.

## Transport and authentication

* **Locally:** a Unix socket with mode 0600, owned by the user. Anyone who
  can connect can run arbitrary commands in sandboxes, by design.
* **Remotely (topology T2):** the Rust client and the SDK accept `https://`
  URLs, but **the daemon serves only a Unix socket in this build.** A remote
  setup must forward the socket over an authenticated, encrypted tunnel (for
  example SSH). Never plain TCP.

Python gRPC sends a Unix socket's path as the HTTP/2 `:authority`, which the
daemon's HTTP/2 server rejects. The SDK sets `grpc.default_authority` to
`localhost`.

## The Python SDK

`python/sandfork/client.py`:

* One request ID per logical call, reused on every retry.
* Exponential backoff only on `INFRA` and `UNAVAILABLE`, never on `TASK`.
* `fork_group(...)` is an async context manager: leaving the block releases
  every sandbox, even on an exception or cancellation.
* `SandforkEnv` (`gym.py`): `reset()` / `step(action)` for frameworks that
  expect a Gym interface. The reward arrives when the episode is done.
  Infrastructure failures end it with `reward = None`.

## The `sf` tool

`sf doctor | prepare | run | fork | exec | eval | diff | snapshot | verify |
release | ls | top | stats | gc | pick | register | drain`. `sf --help` has
the flags.
