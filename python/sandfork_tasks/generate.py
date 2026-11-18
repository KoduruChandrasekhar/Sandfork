"""Generate and validate every task family, and write the generation report.

    python -m sandfork_tasks.generate --out tasks --per-family 200

Deterministic: the same seeds produce the same tasks. Generated task
directories are not committed (``tasks/*/gen-*`` and the template families
are rebuilt by this command); the report is.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import time
from concurrent.futures import ProcessPoolExecutor

from . import bugfix, families

BUGFIX_FIXTURES = [
    bugfix.FixtureSpec("intervals", "py-bugfix", "python", [], {"memory_mb": 512, "pids": 128, "cpu": 1.0, "disk_mb": 64}),
    bugfix.FixtureSpec("lrucache", "py-bugfix", "python", [], {"memory_mb": 512, "pids": 128, "cpu": 1.0, "disk_mb": 64}),
    bugfix.FixtureSpec("ratelimit", "py-bugfix", "python", [], {"memory_mb": 512, "pids": 128, "cpu": 1.0, "disk_mb": 64}),
    # Expensive preparation: a seeded SQLite database that every rollout
    # shares read-only, and that a write would copy up whole.
    bugfix.FixtureSpec("ledger", "py-heavy", "python", ["python build_db.py --txns 150000 --out data/ledger.sqlite"], {"memory_mb": 768, "pids": 128, "cpu": 1.0, "disk_mb": 128}, prebuilt=["data"]),
]


def _validate(args):
    fam, d, python = args
    if fam == "shell-ops":
        return d, families.validate_shell_task(d, python)
    return d, families.validate_answer_task(d, python)


def main(argv=None) -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="tasks")
    ap.add_argument("--fixtures", default="tasks/fixtures")
    ap.add_argument("--per-family", type=int, default=200)
    ap.add_argument("--python", default=sys.executable)
    ap.add_argument("--families", default="py-bugfix,py-heavy,math-tool,data-analysis,shell-ops")
    a = ap.parse_args(argv)
    wanted = a.families.split(",")

    for fam in ("py-bugfix", "py-heavy"):
        if fam not in wanted:
            continue
        fam_dir = os.path.join(a.out, fam)
        for d in os.listdir(fam_dir) if os.path.isdir(fam_dir) else []:
            if d.startswith(tuple(f.name + "-" for f in BUGFIX_FIXTURES)):
                shutil.rmtree(os.path.join(fam_dir, d))
        specs = [s for s in BUGFIX_FIXTURES if s.family == fam]
        per = [bugfix.generate(os.path.join(a.fixtures, s.name), a.out, s, python=a.python) for s in specs]
        for r in per:
            print(json.dumps(r), flush=True)
        save(a.out, fam, {
            "fixtures": per,
            "mutants": sum(r["mutants"] for r in per),
            "kept": sum(r["kept"] for r in per),
            "quarantined": sum(r["quarantined"] for r in per),
            "validated": sum(r["kept"] - r["quarantined"] for r in per),
        })

    for fam, fn in families.GENERATORS.items():
        if fam not in wanted:
            continue
        fam_dir = os.path.join(a.out, fam)
        for d in os.listdir(fam_dir) if os.path.isdir(fam_dir) else []:
            if d[0] in "qt" and d[1:5].isdigit():
                shutil.rmtree(os.path.join(fam_dir, d))
        r = fn(a.out, a.per_family, a.fixtures) if fam == "data-analysis" else fn(a.out, a.per_family)
        dirs = [os.path.join(a.out, fam, d) for d in sorted(os.listdir(os.path.join(a.out, fam))) if d[0] in "qt" and d[1:].isdigit()]
        with ProcessPoolExecutor() as ex:
            res = list(ex.map(_validate, [(fam, d, a.python) for d in dirs]))
        bad = [(d, why) for d, (ok, why) in res if not ok]
        for d, _ in bad:
            os.rename(d, d + ".quarantined")
        r.update({"validated": len(dirs) - len(bad), "quarantined": len(bad), "failures": [f"{os.path.basename(d)}: {w}" for d, w in bad[:10]]})
        print(json.dumps(r), flush=True)
        save(a.out, fam, r)


def save(out: str, fam: str, result: dict) -> None:
    """Merge one family's result into the report on disk, so a crash in a
    later family never loses an earlier one."""
    os.makedirs(out, exist_ok=True)
    path = os.path.join(out, "generation_report.json")
    report = json.load(open(path)) if os.path.exists(path) else {"families": {}}
    report["generated_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    report["families"][fam] = result
    with open(path, "w") as f:
        json.dump(report, f, indent=2)
    write_markdown(report, os.path.join(out, "REPORT.md"))


def write_markdown(report: dict, path: str) -> None:
    lines = ["# Task generation report", "", f"Generated {report['generated_at']} by `python -m sandfork_tasks.generate`.", "", "| Family | Produced | Kept | Quarantined | Validated |", "|---|---|---|---|---|"]
    for fam, r in report["families"].items():
        produced = r.get("mutants", r.get("generated", 0))
        kept = r.get("kept", r.get("generated", 0))
        lines.append(f"| {fam} | {produced} | {kept} | {r.get('quarantined', 0)} | {r.get('validated', 0)} |")
    lines += ["", "For the bug-fix families, *produced* counts every single-site mutant and *kept* those that passed the keep-rules. Discards by reason:", ""]
    for fam, r in report["families"].items():
        for fx in r.get("fixtures", []):
            reasons = ", ".join(f"{k}: {v}" for k, v in sorted(fx["discarded"].items()))
            ops = ", ".join(f"{k}: {v}" for k, v in sorted(fx["operators"].items()))
            lines.append(f"* **{fam} / {fx['fixture']}**: {fx['mutants']} mutants, {fx['kept']} kept, {fx['hidden_tests']} hidden tests. Discarded: {reasons}. Kept by operator: {ops}.")
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
