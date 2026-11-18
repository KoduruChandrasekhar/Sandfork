"""Benchmark driver: every experiment writes raw JSON into bench/results/
with the machine fingerprint and configuration; charts are generated from
those files only (bench/charts.py).

    python bench/run.py e1 e2 e3 e6 --reps 5

Methodology:

* Latency is "to a usable sandbox": the group is returned only after every
  sandbox has answered its first command.
* Every configuration is repeated ``--reps`` times after one warm-up group
  that is excluded; percentiles are reported, never the mean alone.
* Load for E3 is open-loop: rollouts are started on a schedule, not after
  the previous one completes, so queueing delay is measured.
* Page-cache state is declared: runs here are warm (the store was just used).

Experiments that need a GPU (E4) or CRIU (F4) are not run here.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import platform
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, os.path.join(ROOT, "python"))

import sandfork  # noqa: E402
from sandfork_rl import orchestrator as orch  # noqa: E402
from sandfork_rl import tools  # noqa: E402
from sandfork_rl.generate import ByteTokenizer, ScriptedGenerator  # noqa: E402

BUGFIX = os.path.join(ROOT, "tasks", "py-bugfix", "sample-0001")
HEAVY_DIR = os.path.join(ROOT, "tasks", "py-heavy")
DATA = os.path.join(ROOT, "tasks", "data-analysis", "sample-0001")


def sh(cmd: str) -> str:
    try:
        return subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=10).stdout.strip()
    except Exception:
        return ""


def fingerprint() -> dict:
    return {
        "kernel": platform.release(),
        "machine": platform.machine(),
        "cpu": sh("grep -m1 'model name' /proc/cpuinfo | cut -d: -f2").strip(),
        "cores": os.cpu_count(),
        "memory_mb": int(sh("awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo") or 0),
        "fs": sh("stat -f -c %T ${XDG_DATA_HOME:-$HOME/.local/share}"),
        "cgroup": sh("stat -fc %T /sys/fs/cgroup"),
        "distribution": sh(". /etc/os-release; echo $PRETTY_NAME"),
        "virtualization": "wsl2" if "microsoft" in platform.release().lower() else sh("systemd-detect-virt") or "unknown",
        "commit": sh(f"git -C {ROOT} rev-parse --short HEAD") or "uncommitted",
        "python": platform.python_version(),
        "page_cache": "warm",
        # Other load on the machine at the start of the run: results from a
        # machine that was not idle are reported as such.
        "loadavg_at_start": sh("cat /proc/loadavg"),
        "other_busy_processes": sh("ps -eo pcpu,args --sort=-pcpu --no-headers | awk '$1 > 2.0' | grep -v -e sandfork -e 'bench/run.py' -e ' ps ' | head -5 | cut -c1-120"),
    }


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p / 100
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def summary(xs):
    return {"n": len(xs), "p50": pct(xs, 50), "p95": pct(xs, 95), "p99": pct(xs, 99), "max": max(xs) if xs else None, "min": min(xs) if xs else None}


def pss_kb(pid: int) -> int:
    try:
        for line in open(f"/proc/{pid}/smaps_rollup"):
            if line.startswith("Pss:"):
                return int(line.split()[1])
    except OSError:
        pass
    return 0


def sandbox_pids(sandbox_id: str):
    """Host PIDs of every process in a sandbox: the shim found by its
    environment, then its descendants."""
    needle = f"SANDFORK_ID={sandbox_id}".encode() + bytes(1)
    roots = []
    for n in os.listdir("/proc"):
        if n.isdigit():
            try:
                if needle in open(f"/proc/{n}/environ", "rb").read():
                    roots.append(int(n))
            except OSError:
                pass
    out, stack = [], list(roots)
    while stack:
        p = stack.pop()
        out.append(p)
        try:
            for t in os.listdir(f"/proc/{p}/task"):
                stack += [int(c) for c in open(f"/proc/{p}/task/{t}/children").read().split()]
        except OSError:
            pass
    return out


def cgroup_of(pid: int) -> str:
    try:
        rel = open(f"/proc/{pid}/cgroup").read().strip().split("::", 1)[1]
        return "/sys/fs/cgroup" + rel
    except (OSError, IndexError):
        return ""


def group_pss_kb(sandbox_ids, extra_pids=()) -> int:
    """Total PSS of every process in these sandboxes (plus ``extra_pids``).
    PSS divides each shared page among its mappers, so it adds up."""
    pids = set(extra_pids)
    for s in sandbox_ids:
        pids.update(sandbox_pids(s))
    return sum(pss_kb(p) for p in pids)


def save(name: str, data: dict) -> str:
    os.makedirs(os.path.join(HERE, "results"), exist_ok=True)
    stamp = time.strftime("%Y%m%dT%H%M%S")
    path = os.path.join(HERE, "results", f"{name}-{stamp}.json")
    with open(path, "w") as f:
        json.dump({"experiment": name, "fingerprint": fingerprint(), **data}, f, indent=1)
    print(f"wrote {path}", file=sys.stderr)
    return path


# ---------------------------------------------------------------- E1: fork modes


async def e1(client, reps: int) -> dict:
    """Latency to a usable sandbox per fork mode and group size."""
    heavy = sorted(d for d in os.listdir(HEAVY_DIR) if d.startswith("ledger-"))[0]
    tasks = {"py-bugfix": (BUGFIX, ["f1", "f2"]), "py-heavy": (os.path.join(HEAVY_DIR, heavy), ["f1", "f2"]), "data-analysis": (DATA, ["f1", "f2", "f3"])}
    rows = []
    for family, (task, modes) in tasks.items():
        snap = await client.prepare_task(task)
        for mode in modes:
            for g in (1, 4, 8, 16):
                lat, per_sbx = [], []
                for rep in range(reps + 1):
                    try:
                        grp = await client.fork_group(snap, g, mode=mode, admission_deadline=600).open()
                    except sandfork.SandforkError as e:
                        rows.append({"family": family, "mode": mode, "g": g, "error": str(e)})
                        break
                    try:
                        if rep > 0:  # rep 0 is warm-up
                            lat.append(grp.fork_micros / 1000)
                            per_sbx += [s.fork_micros / 1000 for s in grp]
                    finally:
                        await grp.release()
                else:
                    rows.append({"family": family, "mode": mode, "g": g, "group_ms": summary(lat), "sandbox_ms": summary(per_sbx)})
                    print(json.dumps(rows[-1]), file=sys.stderr)
    return {"rows": rows, "reps": reps, "note": "F1 includes preparation; F3 latencies exclude the one-time zygote start (warm-up)."}


# ---------------------------------------------------------------- E2: memory


async def e2(client, reps: int) -> dict:
    """Total PSS of G data-analysis sandboxes, F3 against F2 (each F2
    sandbox loads its own kernel), right after the fork and after a query."""
    snap = await client.prepare_task(DATA)
    rows = []
    for mode in ("f3", "f2"):
        for g in (1, 4, 8):
            async with client.fork_group(snap, g, mode=mode, admission_deadline=600) as grp:
                await asyncio.sleep(0.5)
                ids = [s.id for s in grp]
                zygote = []
                if mode == "f3":
                    for s in await client.list_sandboxes():
                        if s.purpose == "zygote" and s.task_id == grp[0].task_id:
                            zygote = sandbox_pids(s.id)
                after_fork = group_pss_kb(ids, zygote)
                await asyncio.gather(*(s.eval("r = df[df.region=='north'].revenue.sum()") for s in grp))
                after_query = group_pss_kb(ids, zygote)
                await asyncio.gather(*(s.eval("df2 = df.copy(); df2['x'] = df2.units * 2") for s in grp))
                after_write = group_pss_kb(ids, zygote)
                rows.append({"mode": mode, "g": g, "pss_mb_after_fork": after_fork / 1024, "pss_mb_after_query": after_query / 1024, "pss_mb_after_copy": after_write / 1024, "includes_zygote": bool(zygote)})
                print(json.dumps(rows[-1]), file=sys.stderr)
    return {"rows": rows, "note": "PSS summed over every process of every sandbox; for F3 the zygote's own processes are included once."}


# ---------------------------------------------------------------- E3: throughput


async def e3(client, reps: int, duration: float = 60.0) -> dict:
    """Rollouts per second against pool size, open-loop, with a scripted
    agent and a simulated generator (no GPU)."""
    tok = ByteTokenizer()
    script = [tools.format_call("bash", cmd="python -m pytest -q visible | tail -1"), tools.format_call("bash", cmd="sed -i 's/other.hi + 1 == self.hi/other.hi + 1 == self.lo/' src/intervals/core.py"), tools.format_call("run_tests"), tools.format_call("submit")]
    from sandfork_rl.rollout import RolloutConfig, run_rollout
    from sandfork_rl.tasks import load_task

    task = load_task(BUGFIX)
    snap = await client.prepare_task(BUGFIX)
    rows = []
    for pool in (1, 2, 4, 6, 8, 12):
        sem = asyncio.Semaphore(pool)
        done, waits, lat = [], [], []
        # Offered load (4 rollouts/s) above what any pool size here can serve,
        # so the pool is the limit; queueing shows up as queue_wait.
        interval = 0.25
        t_end = time.monotonic() + duration

        async def one(t_arrive):
            async with sem:
                waits.append(time.monotonic() - t_arrive)
                gen = ScriptedGenerator(tok, script, seconds_per_token=0.0005)
                async with client.fork_group(snap, 1, mode="f2", admission_deadline=600) as g:
                    t = await run_rollout(client, task, tok, gen, g[0], RolloutConfig(checkpoint_every=0))
                done.append(time.monotonic())
                lat.append(time.monotonic() - t_arrive)
                return t.reward

        jobs = []
        t0 = time.monotonic()
        while time.monotonic() < t_end:
            jobs.append(asyncio.create_task(one(time.monotonic())))
            await asyncio.sleep(interval)
        # The window is over: whatever has not finished is cancelled (its
        # group is still released by the context manager), so the next pool
        # size starts from an empty pool.
        for j in jobs:
            if not j.done():
                j.cancel()
        results = await asyncio.gather(*jobs, return_exceptions=True)
        in_window = [d for d in done if d - t0 <= duration]
        errors = sum(1 for r in results if isinstance(r, BaseException) and not isinstance(r, asyncio.CancelledError))
        rows.append({"pool": pool, "rollouts_per_s": len(in_window) / duration, "completed": len(in_window), "offered": len(jobs), "errors": errors, "queue_wait_s": summary(waits), "latency_s": summary(lat)})
        await asyncio.sleep(3)  # let teardown finish before the next point
        print(json.dumps({k: rows[-1][k] for k in ("pool", "rollouts_per_s", "completed", "errors")}), file=sys.stderr)
    return {"rows": rows, "duration_s": duration, "agent": "scripted, 4 turns, 0.5 ms per generated token", "offered_per_s": 1 / 0.25}


# ---------------------------------------------------------------- E6: branching


async def e6(client, reps: int) -> dict:
    """Flat G=8 against 2x4 and 4x2 trees on the expensive-prefix task:
    environment seconds and prefill tokens per leaf."""
    tok = ByteTokenizer()
    heavy = sorted(d for d in os.listdir(HEAVY_DIR) if d.startswith("ledger-"))[0]
    task_dir = os.path.join(HEAVY_DIR, heavy)
    # Two expensive turns (full test runs), then two cheap ones.
    script = [tools.format_call("bash", cmd="python -m pytest -q /workspace/visible >/dev/null; python -m compileall -q src; echo prefix"), tools.format_call("run_tests"), tools.format_call("bash", cmd="echo branch"), tools.format_call("submit")]
    rows = []
    for name, b in (("flat", 0), ("tree-2x4", 4), ("tree-4x2", 2)):
        for rep in range(reps):
            gen = ScriptedGenerator(tok, script)
            cfg = orch.StepConfig(prompts_per_step=1, group_size=8, branch_factor=b, branch_turn=2, check_token_exact=True)
            t0 = time.monotonic()
            res = await orch.run_step(client, [task_dir], tok, gen, cfg, step=rep)
            wall = time.monotonic() - t0
            br = res.trace.breakdown()
            env = br.get("command", 0) + br.get("fork", 0) + br.get("branch", 0)
            rows.append({"shape": name, "rep": rep, "leaves": len(res.trajectories), "wall_s": wall, "command_s_per_leaf": br.get("command", 0) / 8, "env_s_per_leaf": env / 8, "prefill_tokens_per_leaf": (gen.prefill_total - gen.prefill_cached) / 8, "prefix_hit_rate": gen.prefill_cached / max(gen.prefill_total, 1), "rewards": [t.reward for t in res.trajectories]})
            print(json.dumps({k: rows[-1][k] for k in ("shape", "wall_s", "command_s_per_leaf", "prefill_tokens_per_leaf", "prefix_hit_rate")}), file=sys.stderr)
    return {"rows": rows, "note": "scripted agent; the model side counts prefill tokens through a block-hash prefix cache (16-token blocks)"}


EXPERIMENTS = {"e1": e1, "e2": e2, "e3": e3, "e6": e6}


async def amain(a) -> None:
    async with sandfork.connect(a.socket) as client:
        for name in a.experiments:
            t0 = time.monotonic()
            data = await EXPERIMENTS[name](client, a.reps)
            data["seconds"] = time.monotonic() - t0
            save(name, data)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("experiments", nargs="+", choices=sorted(EXPERIMENTS))
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--socket", default=None)
    asyncio.run(amain(ap.parse_args()))


if __name__ == "__main__":
    main()
