# Platform report (Part 0)

`scripts/probe.sh` (or `sf doctor`) prints one JSON object per machine. This
file records what it returned and the decisions taken from it. Only the
development laptop has been probed so far. The Kaggle and GitHub Actions
columns are open items, to be filled in by running the same script there.

## Capability matrix

| Probe | Laptop, WSL2 | Kaggle 2×T4 | GitHub Actions `ubuntu-24.04` |
|---|---|---|---|
| Kernel | 6.6.87.2-microsoft-standard-WSL2 | not probed | not probed |
| Distribution | Ubuntu 26.04 LTS | | |
| Cores / memory | 8 / 7.8 GB (7.3 GB available) | | |
| State filesystem | ext4 (`ext2/ext3` magic) | | |
| cgroup v2 | yes: `cpuset cpu io memory hugetlb pids rdma` | | |
| Delegated to the user | `cpu memory pids` (systemd user scope) | | |
| Unprivileged user namespaces | yes (`unshare -Ur id -u` → 0) | | expected yes after the AppArmor sysctl |
| AppArmor userns restriction | absent | | enabled by default on 24.04 |
| Mount / PID / network namespace | yes / yes (PID 1) / yes (only `lo`) | | |
| Time namespace | yes | | |
| Subordinate IDs | `100000:65536` in `/etc/subuid` | | |
| `newuidmap` / `newgidmap` | **not installed** (package `uidmap`) | | |
| overlayfs in a user namespace (`userxattr`) | yes, on the state directory too | | |
| tmpfs with `user.*` xattrs | yes (needs Linux 6.6) | | |
| Whiteouts by `mknod c 0 0` | yes | | |
| `openat2` | yes | | |
| Seccomp in the probe process | none (0) | | |
| CRIU | **absent**; the distribution offers 4.2, below the pinned 4.2.1 | | |
| Soft-dirty / userfaultfd | yes / yes (kernel config) | | |
| `/dev/kvm` | present, not accessible to the user | | |
| PSS (`smaps_rollup`) | readable | | |
| KSM | off (`run = 0`) | | |
| Reflinks on the state filesystem | no (ext4) | | |
| GPU | none | 2×T4 | none |
| Passwordless root | **no** | | yes |

## What that means

* **Isolation levels on the laptop.** L0 and L1 work. L1 runs with a
  *single-ID* mapping, because `newuidmap` is not installed. The sandbox's
  root is the invoking user, so there is no separate unprivileged agent
  user. Agent commands instead run as that root with every capability
  dropped and `no_new_privs` set, under the seccomp filter. Installing
  `uidmap` switches the daemon to the full mapping automatically: sandbox
  root becomes host UID 100000 and the agent becomes 101000.
* **L2 and CRIU need root,** which this machine does not grant without a
  password. L2 (real root, per-sandbox UID slots) is implemented in the
  daemon but was not exercised here. CRIU is not implemented in this build
  (see `checkpoint.md`).
* **cgroup limits at L1** work because systemd delegates `cpu memory pids`
  to the user's scope. `scripts/dev-daemon.sh` starts the daemon in a
  delegated scope, and the daemon moves itself into a `daemon/` leaf before
  enabling controllers.
* **Snapshot backend:** overlay, the default. Reflink copies are not
  available on ext4, and a btrfs loop device needs root.

## Cold-cost baselines

Not yet measured with `hyperfine`, which is not installed. What has been
measured on the laptop, in the course of building the task families:

| What | Time |
|---|---|
| Base layer `python` (venv, pytest unpacked from wheels) | about 1 s, once per machine |
| Base layer `python-data` (adds numpy, pandas) | about 3 s, once per machine |
| Preparing `py-bugfix/sample-0001` (files layer, `compileall`, test collection) | 3.0 s first time, 0 ms cached |
| Preparing `data-analysis/sample-0001` (generating 200k rows) | 6.8 s first time |
| Seeding the py-heavy ledger database (150k transactions, 13 MB) | 2.2 s |
| A fixture's whole test suite (`pytest -q`) | 0.7 to 1.1 s |

## GPU side and model choice

Not run: there is no GPU here and Kaggle was not used. The trainer, the
generator interfaces (local `transformers`, TokenMill, vLLM) and the loss
were tested on CPU with a tiny random GPT-2 that is built locally. Choosing
the model (pass@1 between 5% and 60% on 50 tasks per family) is the first
thing to do on Kaggle.

## Topology

Development and every measurement in `benchmarks.md`: **T3, measurement
only**, with scripted agents on the laptop. The end-to-end RL run (T1 or T2)
is open.

## Go / no-go decisions taken

| Result | Decision |
|---|---|
| User namespaces and overlayfs work under WSL2 | L1 is the development level |
| No `newuidmap` | Single-ID mapping with capabilities dropped; documented, automatic fallback |
| `criu check` cannot run (no CRIU, no root) | Checkpoints are filesystem-only; F4 is reported as unavailable |
| No GPU | Every experiment that needs one (E4, the training run) is open |
