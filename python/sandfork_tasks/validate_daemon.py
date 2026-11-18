"""Validate generated tasks through a running daemon: the real prepare,
fork, transplant and grading path.

    python -m sandfork_tasks.validate_daemon --tasks tasks --per-family 5

For each sampled task: fork two sandboxes; apply the reference solution in
one, touch nothing in the other; verify both. The reference must score 1.0,
the untouched state must score below 1.0 (exactly 0.0 for all-or-nothing
tasks), and grading the reference twice must agree (grader determinism).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import random
import sys

import sandfork
from sandfork_rl.tasks import find_tasks


async def apply_reference(sbx, task_dir: str) -> None:
    if os.path.exists(os.path.join(task_dir, "solution.json")):
        ans = json.load(open(os.path.join(task_dir, "solution.json")))["answer"]
        await sbx.write_file("answer.txt", f"{ans}\n")
    elif os.path.exists(os.path.join(task_dir, "solution.sh")):
        r = await sbx.exec(open(os.path.join(task_dir, "solution.sh")).read(), timeout=60)
        if not r.ok:
            raise RuntimeError(f"solution.sh failed: {r.text[-500:]}")
    else:
        m = json.load(open(os.path.join(task_dir, "mutation.json")))
        text = (await sbx.read_file(m["file"])).decode()
        lines = text.split("\n")
        at = m["start_line"] - 1
        assert lines[at : at + len(m["new"])] == m["new"], "the sandbox does not hold the mutated source"
        lines[at : at + len(m["new"])] = m["old"]
        await sbx.write_file(m["file"], "\n".join(lines))


async def check(client, task_dir: str) -> dict:
    snap = await client.prepare_task(task_dir)
    async with client.fork_group(snap, 2, mode="f2", admission_deadline=300) as g:
        await apply_reference(g[0], task_dir)
        ref, untouched = await asyncio.gather(g[0].verify(), g[1].verify())
        again = await g[0].verify()
    allornothing = "all_or_nothing" in open(os.path.join(task_dir, "task.toml")).read()
    problems = []
    if ref.reward != 1.0:
        problems.append(f"reference scored {ref.reward} ({ref.passed}/{ref.total}, {ref.status}, {ref.error})")
    if untouched.reward is None or untouched.reward >= 1.0 or (allornothing and untouched.reward != 0.0):
        problems.append(f"untouched scored {untouched.reward}")
    if again.reward != ref.reward:
        problems.append(f"grader not deterministic: {ref.reward} then {again.reward}")
    return {"task": task_dir, "ok": not problems, "problems": problems, "ref": ref.reward, "untouched": untouched.reward, "verify_ms": ref.duration_ms}


async def amain(a) -> int:
    rng = random.Random(a.seed)
    dirs = []
    for fam in a.families.split(","):
        found = find_tasks(os.path.join(a.tasks, fam))
        dirs += rng.sample(found, min(a.per_family, len(found)))
    sem = asyncio.Semaphore(a.concurrency)
    async with sandfork.connect(a.socket) as client:

        async def one(d):
            async with sem:
                try:
                    return await check(client, d)
                except Exception as e:  # noqa: BLE001 - reported, never hidden
                    return {"task": d, "ok": False, "problems": [repr(e)]}

        results = await asyncio.gather(*(one(d) for d in dirs))
    bad = [r for r in results if not r["ok"]]
    for r in results:
        print(json.dumps(r), flush=True)
    print(f"{len(results) - len(bad)}/{len(results)} tasks validated through the daemon", file=sys.stderr)
    if a.out:
        with open(a.out, "w") as f:
            json.dump(results, f, indent=1)
    return 1 if bad else 0


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", default=None)
    ap.add_argument("--tasks", default="tasks")
    ap.add_argument("--families", default="py-bugfix,py-heavy,math-tool,data-analysis,shell-ops")
    ap.add_argument("--per-family", type=int, default=5)
    ap.add_argument("--concurrency", type=int, default=4)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    sys.exit(asyncio.run(amain(a)))


if __name__ == "__main__":
    main()
