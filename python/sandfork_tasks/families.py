"""Template-generated task families: ``math-tool``, ``data-analysis`` and
``shell-ops``. Every task is deterministic from its seed, carries a hidden
expected result, and is validated by grading a reference solution (must score
1.0) and the untouched state (must score 0.0).
"""

from __future__ import annotations

import json
import math
import os
import random
import shutil
import subprocess
import sys
import tempfile
from typing import Callable, Dict, List, Tuple

# ---------------------------------------------------------------- shared grader

CHECK_ANSWER = '''"""Grader: compare the submitted number with the expected one."""
import json, math, os
here = os.path.dirname(os.path.abspath(__file__))
exp = json.load(open(os.path.join(here, "expected.json")))
try:
    got = float(open("answer.txt").read().strip().replace(",", ""))
    ok = math.isclose(got, exp["answer"], rel_tol=exp.get("rel_tol", 1e-9), abs_tol=exp.get("abs_tol", 1e-6))
except (OSError, ValueError):
    ok = False
print(json.dumps({"passed": int(ok), "total": 1}))
'''


def _write(path: str, text: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def _task_toml(tid: str, family: str, base: str, tools: List[str], allowed: List[str], *, fixture: str = "", steps: List[str] = (), warm: str = "", command: str, total: int, reward: str, limits: Dict[str, int], max_turns: int = 8) -> str:
    prep = []
    if fixture:
        prep.append(f'fixture = "{fixture}"')
    if steps:
        prep.append("steps = [" + ", ".join(json.dumps(s) for s in steps) + "]")
    if warm:
        prep.append(f"warm_process = {json.dumps(warm)}")
    lim = "\n".join(f"{k} = {v}" for k, v in limits.items())
    return f"""[task]
id = "{tid}"
family = "{family}"
base = "{base}"
timeout_s = 120
max_turns = {max_turns}

[prepare]
{chr(10).join(prep)}

[agent]
prompt = "prompt.md"
tools = {json.dumps(tools)}
allowed_paths = {json.dumps(allowed)}

[verify]
kind = "script"
grader = "hidden"
command = {json.dumps(command)}
timeout_s = 20
reward = "{reward}"
total = {total}
version = "script-v1"

[limits]
{lim}
"""


def _grade(task_dir: str, workspace: str, python: str) -> Tuple[int, int]:
    """Run a script grader the way the daemon does: cwd is the workspace and
    the grader directory stands in for /grader."""
    hidden = os.path.abspath(os.path.join(task_dir, "hidden"))
    cmd = json.load(open(os.path.join(task_dir, "verify.json")))["command"].replace("/grader", hidden).replace("python ", f"{python} ", 1)
    p = subprocess.run(cmd, shell=True, cwd=workspace, capture_output=True, text=True, timeout=60)
    for line in reversed(p.stdout.strip().splitlines()):
        if line.startswith("{"):
            r = json.loads(line)
            return r["passed"], r["total"]
    raise RuntimeError(f"{task_dir}: grader printed no result: {p.stdout[-500:]} {p.stderr[-500:]}")


# ---------------------------------------------------------------- math-tool

NAMES = ["Ada", "Ravi", "Mei", "Tomas", "Lena", "Kofi", "Iris", "Omar", "Priya", "Jonas"]
ITEMS = [("apples", 0.45), ("notebooks", 2.35), ("tickets", 12.5), ("cables", 7.8), ("lamps", 23.99), ("boxes", 1.15), ("pens", 0.89), ("tiles", 3.4)]


def _math_problem(rng: random.Random) -> Tuple[str, float, str]:
    kind = rng.randrange(6)
    who = rng.choice(NAMES)
    if kind == 0:
        item, price = rng.choice(ITEMS)
        n, m = rng.randint(3, 60), rng.randint(2, 9)
        total = round(n * price * m, 2)
        return f"{who} buys {n} {item} at ${price:.2f} each, every week for {m} weeks. How many dollars does {who} spend in total?", total, f"{n} * {price} * {m}"
    if kind == 1:
        p0, rate, years = rng.randint(1000, 20000), rng.choice([2, 3, 4, 5, 6, 7]), rng.randint(2, 12)
        v = round(p0 * (1 + rate / 100) ** years, 2)
        return f"{who} deposits ${p0} at {rate}% interest compounded yearly. What is the balance after {years} years, in dollars, rounded to the cent?", v, f"round({p0} * (1 + {rate}/100) ** {years}, 2)"
    if kind == 2:
        a, b, c = rng.randint(120, 900), rng.randint(2, 9), rng.randint(15, 80)
        v = a * b - c * b
        return f"A train carries {a} passengers per trip and makes {b} trips. On each trip {c} passengers get off early without riding to the end. How many full rides are completed?", float(v), f"{a} * {b} - {c} * {b}"
    if kind == 3:
        w, h, d = rng.randint(2, 30), rng.randint(2, 30), rng.randint(2, 30)
        v = 2 * (w * h + w * d + h * d)
        return f"What is the surface area of a closed box measuring {w} by {h} by {d} centimetres, in square centimetres?", float(v), f"2 * ({w}*{h} + {w}*{d} + {h}*{d})"
    if kind == 4:
        speed1, speed2, gap = rng.randint(40, 90), rng.randint(40, 90), rng.randint(100, 900)
        v = round(gap / (speed1 + speed2), 4)
        return f"Two cyclists start {gap} km apart and ride toward each other at {speed1} km/h and {speed2} km/h. After how many hours do they meet? Round to 4 decimals.", v, f"round({gap} / ({speed1} + {speed2}), 4)"
    vals = [rng.randint(1, 99) for _ in range(rng.randint(5, 9))]
    v = round(sum(vals) / len(vals), 4)
    return f"{who} scored {', '.join(map(str, vals))} on a series of quizzes. What is the mean score, rounded to 4 decimals?", v, f"round(sum([{', '.join(map(str, vals))}]) / {len(vals)}, 4)"


def gen_math(out: str, n: int, seed: int = 11) -> dict:
    rng = random.Random(seed)
    fam = os.path.join(out, "math-tool")
    seen = set()
    made = 0
    while made < n:
        text, answer, expr = _math_problem(rng)
        if text in seen:
            continue
        seen.add(text)
        tid = f"math-tool/q{made:04d}"
        d = os.path.join(fam, f"q{made:04d}")
        _write(os.path.join(d, "prompt.md"), f"{text}\n\nYou have a `calc` tool for arithmetic. Call `submit` with the numeric answer, for example `{{\"answer\": 42.5}}`.\n")
        _write(os.path.join(d, "hidden", "check.py"), CHECK_ANSWER)
        _write(os.path.join(d, "hidden", "expected.json"), json.dumps({"answer": answer, "abs_tol": 0.005, "rel_tol": 1e-9, "expr": expr}))
        cmd = "python3 -I /grader/check.py"
        _write(os.path.join(d, "task.toml"), _task_toml(tid, "math-tool", "shell", ["calc", "submit"], ["answer.txt"], command=cmd, total=1, reward="all_or_nothing", limits={"memory_mb": 128, "pids": 32, "cpu": 0.5, "disk_mb": 16}, max_turns=6))
        _write(os.path.join(d, "verify.json"), json.dumps({"command": cmd}))
        _write(os.path.join(d, "solution.json"), json.dumps({"answer": answer}))
        made += 1
    return {"family": "math-tool", "generated": made}


# ---------------------------------------------------------------- data-analysis

REGIONS = ["north", "south", "east", "west", "central", "coastal", "inland", "metro"]


def _data_question(rng: random.Random, data) -> Tuple[str, float, str]:
    import numpy as np

    region = rng.randrange(len(REGIONS))
    lo = rng.randint(0, 300)
    hi = lo + rng.randint(5, 60)
    prod = rng.randrange(50)
    rev = data["units"] * data["price"]
    kept = ~data["returned"]
    kind = rng.randrange(6)
    if kind == 0:
        m = (data["region"] == region) & kept
        return f"What is the total revenue of the `{REGIONS[region]}` region, counting only rows that were not returned? Round to two decimals.", round(float(rev[m].sum()), 2), f"df[(df.region=='{REGIONS[region]}') & ~df.returned].revenue.sum()"
    if kind == 1:
        m = (data["day"] >= lo) & (data["day"] <= hi)
        return f"How many units were sold between day {lo} and day {hi} inclusive, returns included?", float(data["units"][m].sum()), f"df[df.day.between({lo},{hi})].units.sum()"
    if kind == 2:
        m = data["product"] == prod
        return f"What is the mean price of product {prod}, rounded to four decimals?", round(float(data["price"][m].mean()), 4), f"df[df.product=={prod}].price.mean()"
    if kind == 3:
        m = (data["region"] == region) & (data["day"] >= lo) & (data["day"] <= hi)
        return f"What fraction of `{REGIONS[region]}` rows between day {lo} and day {hi} inclusive were returned? Round to six decimals.", round(float(data["returned"][m].mean()), 6), f"df[(df.region=='{REGIONS[region]}') & df.day.between({lo},{hi})].returned.mean()"
    if kind == 4:
        m = data["product"] == prod
        per = np.bincount(data["region"][m], weights=rev[m], minlength=len(REGIONS))
        best = int(np.argmax(per))
        return f"Which region has the highest total revenue for product {prod}? Answer with the region's 0-based index in this list: {REGIONS}.", float(best), f"df[df.product=={prod}].groupby('region').revenue.sum().idxmax()"
    m = (data["day"] >= lo) & (data["day"] <= hi) & kept
    return f"What is the largest single-row revenue between day {lo} and day {hi} inclusive, excluding returns? Round to two decimals.", round(float(rev[m].max()), 2), f"df[df.day.between({lo},{hi}) & ~df.returned].revenue.max()"


def gen_data(out: str, n: int, fixtures: str, seed: int = 13) -> dict:
    sys.path.insert(0, os.path.join(fixtures, "sales"))
    from gen_data import generate  # type: ignore

    data = generate(200_000, 1)
    rng = random.Random(seed)
    fam = os.path.join(out, "data-analysis")
    made, seen = 0, set()
    while made < n:
        q, answer, how = _data_question(rng, data)
        if q in seen or (isinstance(answer, float) and math.isnan(answer)):
            continue
        seen.add(q)
        d = os.path.join(fam, f"q{made:04d}")
        tid = f"data-analysis/q{made:04d}"
        _write(os.path.join(d, "prompt.md"), "A sales dataset is already loaded in the Python kernel as the DataFrame `df` with columns `region` (categorical), `product`, `units`, `price`, `day`, `returned` and `revenue` (units times price).\n\n" + f"Question: {q}\n\nUse the `python` tool to compute it, then call `submit` with your answer.\n")
        _write(os.path.join(d, "hidden", "check.py"), CHECK_ANSWER)
        _write(os.path.join(d, "hidden", "expected.json"), json.dumps({"answer": answer, "abs_tol": 0.005 if "two decimals" in q else 1e-6, "rel_tol": 1e-9, "how": how}))
        cmd = "python -I /grader/check.py"
        _write(
            os.path.join(d, "task.toml"),
            _task_toml(
                tid,
                "data-analysis",
                "python-data",
                ["python", "submit"],
                ["answer.txt"],
                fixture="fixtures/sales",
                steps=["python gen_data.py --rows 200000 --seed 1 --out data.npz"],
                warm="python /.sandfork/rt/sandfork_kernel.py --loader /workspace/load.py",
                command=cmd,
                total=1,
                reward="all_or_nothing",
                limits={"memory_mb": 384, "pids": 64, "cpu": 1.0, "disk_mb": 64},
            ),
        )
        _write(os.path.join(d, "verify.json"), json.dumps({"command": cmd}))
        _write(os.path.join(d, "solution.json"), json.dumps({"answer": answer}))
        made += 1
    return {"family": "data-analysis", "generated": made}


# ---------------------------------------------------------------- shell-ops

SHELL_CHECK = '''"""Grader: checks on the final filesystem of the workspace."""
import hashlib, json, os, stat, sys
here = os.path.dirname(os.path.abspath(__file__))
spec = json.load(open(os.path.join(here, "expected.json")))
passed = 0
for c in spec["checks"]:
    p = c.get("path", "")
    try:
        if c["kind"] == "exists":
            ok = os.path.lexists(p)
        elif c["kind"] == "absent":
            ok = not os.path.lexists(p)
        elif c["kind"] == "content":
            ok = open(p, encoding="utf-8").read().strip() == c["value"].strip()
        elif c["kind"] == "sha256":
            ok = hashlib.sha256(open(p, "rb").read()).hexdigest() == c["value"]
        elif c["kind"] == "executable":
            ok = os.path.isfile(p) and bool(os.stat(p).st_mode & stat.S_IXUSR)
        elif c["kind"] == "listing":
            ok = sorted(os.listdir(p)) == sorted(c["value"])
        elif c["kind"] == "run":
            import subprocess
            r = subprocess.run(c["argv"], capture_output=True, text=True, timeout=10, cwd=c.get("cwd", "."))
            ok = r.returncode == 0 and r.stdout.strip() == c["value"].strip()
        else:
            ok = False
    except Exception:
        ok = False
    passed += bool(ok)
print(json.dumps({"passed": passed, "total": len(spec["checks"])}))
'''

WORDS = ["alpha", "bravo", "cedar", "delta", "ember", "fjord", "gable", "harbor", "indigo", "juniper", "kestrel", "lumen", "maple", "nectar", "onyx", "pylon"]


def _sh(rng: random.Random) -> Tuple[str, Dict[str, str], List[dict], str]:
    """(prompt, files to create, checks, reference solution script)."""
    import hashlib

    kind = rng.randrange(5)
    if kind == 0:
        n = rng.randint(4, 9)
        days = sorted(rng.sample(range(1, 28), 3))
        files, moved = {}, {}
        for i in range(n):
            day = rng.choice(days)
            name = f"{rng.choice(WORDS)}-{i}.log"
            files[f"logs/{name}"] = f"day={day}\n" + "\n".join(rng.choice(WORDS) for _ in range(rng.randint(2, 6))) + "\n"
            moved.setdefault(f"2026-03-{day:02d}", []).append(name)
            if rng.random() < 0.4:
                files[f"logs/{name[:-4]}.txt"] = "keep me\n"
        checks = [{"kind": "listing", "path": f"archive/{d}", "value": v} for d, v in sorted(moved.items())]
        checks += [{"kind": "absent", "path": f"logs/{x}"} for v in moved.values() for x in v]
        checks += [{"kind": "exists", "path": p} for p in files if p.endswith(".txt")]
        prompt = "Every `.log` file in `logs/` starts with a line `day=N`. Move each one into `archive/2026-03-DD/` (DD is the day, two digits), keeping its name. Leave other files where they are."
        sol = "for f in logs/*.log; do d=$(head -1 \"$f\" | cut -d= -f2); dir=archive/2026-03-$(printf %02d $d); mkdir -p $dir; mv \"$f\" $dir/; done"
        return prompt, files, checks, sol
    if kind == 1:
        key, val = rng.choice(WORDS), str(rng.randint(10, 9999))
        keys = rng.sample(WORDS, 5)
        if key not in keys:
            keys[2] = key
        body = "".join(f"{k} = {rng.randint(1, 99)}\n" for k in keys)
        section = rng.choice(["server", "cache", "worker"])
        cfg = f"# settings\n[{section}]\n{body}\n[other]\n{key} = 1\n"
        expected = cfg.replace(f"[{section}]\n", "", 1)
        lines = cfg.split("\n")
        out, in_sec = [], False
        for line in lines:
            if line.startswith("["):
                in_sec = line == f"[{section}]"
            if in_sec and line.startswith(f"{key} = "):
                line = f"{key} = {val}"
            out.append(line)
        new = "\n".join(out)
        _ = expected
        prompt = f"In `config.ini`, set `{key}` to `{val}` in the `[{section}]` section only. Change nothing else."
        sol = f"python3 - <<'EOF'\nlines=open('config.ini').read().split('\\n'); sec=False; out=[]\nfor l in lines:\n    if l.startswith('['): sec = l == '[{section}]'\n    if sec and l.startswith('{key} = '): l = '{key} = {val}'\n    out.append(l)\nopen('config.ini','w').write('\\n'.join(out))\nEOF"
        return prompt, {"config.ini": cfg}, [{"kind": "sha256", "path": "config.ini", "value": hashlib.sha256(new.encode()).hexdigest()}], sol
    if kind == 2:
        rows = [(rng.choice(WORDS), rng.randint(1, 500)) for _ in range(rng.randint(6, 14))]
        csv = "name,qty\n" + "".join(f"{a},{b}\n" for a, b in rows)
        target = rng.choice(["total.txt", "sum.txt"])
        total = sum(b for _, b in rows)
        prompt = f"`data.csv` has a header and two columns, `name` and `qty`. Write the sum of the `qty` column, as a plain integer, to `{target}`."
        sol = f"tail -n +2 data.csv | cut -d, -f2 | paste -sd+ | bc > {target} 2>/dev/null || python3 -c \"import csv; print(sum(int(r['qty']) for r in csv.DictReader(open('data.csv'))))\" > {target}"
        return prompt, {"data.csv": csv}, [{"kind": "content", "path": target, "value": str(total)}], sol
    if kind == 3:
        n = rng.randint(3, 7)
        names = [f"{rng.choice(WORDS).upper()}_{i}.TXT" for i in range(n)]
        files = {f"inbox/{x}": f"{x}\n" for x in names}
        checks = [{"kind": "listing", "path": "inbox", "value": [x.lower() for x in names]}]
        checks += [{"kind": "content", "path": f"inbox/{x.lower()}", "value": x} for x in names]
        prompt = "Rename every file in `inbox/` to its lowercase name. Do not change file contents."
        sol = "cd inbox && for f in *; do l=$(echo \"$f\" | tr A-Z a-z); [ \"$f\" != \"$l\" ] && mv \"$f\" \"$l\"; done; true"
        return prompt, files, checks, sol
    word = rng.choice(WORDS)
    n_files = rng.randint(3, 6)
    files = {}
    count = 0
    for i in range(n_files):
        lines = [" ".join(rng.choice(WORDS) for _ in range(rng.randint(3, 8))) for _ in range(rng.randint(2, 9))]
        count += sum(1 for ln in lines if word in ln.split())
        files[f"notes/n{i}.txt"] = "\n".join(lines) + "\n"
    prompt = f"Write an executable shell script `count.sh` that prints how many lines in `notes/*.txt` contain the word `{word}` as a whole word. It is run from this directory."
    sol = f"printf '#!/bin/sh\\ngrep -hw {word} notes/*.txt | wc -l\\n' > count.sh && chmod +x count.sh"
    checks = [{"kind": "executable", "path": "count.sh"}, {"kind": "run", "argv": ["./count.sh"], "value": str(count)}]
    return prompt, files, checks, sol


def gen_shell(out: str, n: int, seed: int = 17) -> dict:
    rng = random.Random(seed)
    fam = os.path.join(out, "shell-ops")
    made, seen = 0, set()
    while made < n:
        prompt, files, checks, sol = _sh(rng)
        key = json.dumps([prompt, files], sort_keys=True)
        if key in seen:
            continue
        seen.add(key)
        d = os.path.join(fam, f"t{made:04d}")
        shutil.rmtree(d, ignore_errors=True)
        for p, body in files.items():
            _write(os.path.join(d, "repo", p), body)
        _write(os.path.join(d, "prompt.md"), prompt + "\n\nUse the `bash`, `read_file` and `write_file` tools. Call `submit` when you are done.\n")
        _write(os.path.join(d, "hidden", "check.py"), SHELL_CHECK)
        _write(os.path.join(d, "hidden", "expected.json"), json.dumps({"checks": checks}, indent=1))
        _write(os.path.join(d, "solution.sh"), sol + "\n")
        cmd = "python3 -I /grader/check.py"
        _write(
            os.path.join(d, "task.toml"),
            _task_toml(f"shell-ops/t{made:04d}", "shell-ops", "shell", ["bash", "read_file", "write_file", "submit"], ["**"], command=cmd, total=len(checks), reward="all_or_nothing", limits={"memory_mb": 128, "pids": 64, "cpu": 0.5, "disk_mb": 32}),
        )
        _write(os.path.join(d, "verify.json"), json.dumps({"command": cmd}))
        made += 1
    return {"family": "shell-ops", "generated": made}


# ---------------------------------------------------------------- validation


def validate_answer_task(task_dir: str, python: str) -> Tuple[bool, str]:
    """The reference answer scores 1, an empty workspace scores 0."""
    sol = json.load(open(os.path.join(task_dir, "solution.json")))
    with tempfile.TemporaryDirectory() as ws:
        p0, t0 = _grade(task_dir, ws, python)
        with open(os.path.join(ws, "answer.txt"), "w") as f:
            f.write(f"{sol['answer']}\n")
        p1, t1 = _grade(task_dir, ws, python)
    if (p0, p1) != (0, t1) or t1 != 1:
        return False, f"untouched {p0}/{t0}, reference {p1}/{t1}"
    return True, ""


def validate_shell_task(task_dir: str, python: str) -> Tuple[bool, str]:
    """The reference script passes every check three times in a row; the
    untouched repository fails at least one check."""
    results = []
    for run in range(4):
        with tempfile.TemporaryDirectory() as ws:
            repo = os.path.join(task_dir, "repo")
            for name in os.listdir(repo):
                s = os.path.join(repo, name)
                (shutil.copytree if os.path.isdir(s) else shutil.copy2)(s, os.path.join(ws, name))
            if run > 0:
                subprocess.run(["sh", "-c", open(os.path.join(task_dir, "solution.sh")).read()], cwd=ws, check=False, capture_output=True, timeout=30)
            results.append(_grade(task_dir, ws, python))
    (p0, t0), refs = results[0], results[1:]
    if p0 == t0:
        return False, "the untouched repository already passes"
    if any(p != t for p, t in refs):
        return False, f"reference runs scored {refs}"
    return True, ""


GENERATORS: Dict[str, Callable] = {"math-tool": gen_math, "data-analysis": gen_data, "shell-ops": gen_shell}
