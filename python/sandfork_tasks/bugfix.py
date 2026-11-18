"""Generate bug-fix tasks by mutation (families ``py-bugfix`` and ``py-heavy``).

For each fixture library, every single-site mutant of its source is tried.
A mutant is kept as a task only when all of these hold:

1. The unmutated fixture passes every test.
2. The mutant fails at least one test and at most five, so the bug is focused.
3. No kept mutant already fails the same set of tests with the same diff.
4. The inverse mutation, the reference fix, restores the source byte for byte,
   and the restored fixture passes the hidden suite three runs in a row. A
   fixture that fails this is flaky and every task from it is quarantined.

The visible tests are the failing test functions (up to three). The hidden
suite is the whole test suite, so a fix that breaks other behaviour is
caught. ``prompt.md`` is filled from the failing assertion's message, not by a
language model, so tasks are deterministic.
"""

from __future__ import annotations

import ast
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from concurrent.futures import ProcessPoolExecutor
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence, Tuple

from .mutate import Mutation, mutants

PROMPT = """A test in this repository fails. Find the bug in `src/` and fix it.

Failing test: `{test}`

    {message}

Run the visible tests with the `run_tests` tool. Only files under `src/` are
graded, against a larger hidden test suite. Call `submit` when you are done.
"""

FAILED_RE = re.compile(r"^FAILED (\S+?)(?: - (.*))?$")
SUMMARY_RE = re.compile(r"(\d+) (passed|failed|error|errors)")


@dataclass
class FixtureSpec:
    name: str
    family: str
    base: str
    #: Preparation steps run once per task (after the workspace is copied).
    steps: List[str]
    limits: Dict[str, int]
    timeout_s: int = 60
    #: Paths inside the fixture that preparation creates and tests need;
    #: built once for validation and linked into each scratch copy.
    prebuilt: List[str] = field(default_factory=list)


@dataclass
class RunResult:
    passed: int
    failed: List[str]
    messages: Dict[str, str]
    ok: bool
    output: str = ""


def run_pytest(workdir: str, python: str, test_dir: str = "tests", timeout: int = 120) -> RunResult:
    env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1", "PYTHONPATH": os.path.join(workdir, "src")}
    try:
        p = subprocess.run([python, "-m", "pytest", "-q", "-p", "no:cacheprovider", "--tb=no", "-rf", test_dir], cwd=workdir, env=env, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return RunResult(0, ["<timeout>"], {}, False, "timeout")
    failed, messages = [], {}
    passed = 0
    for line in p.stdout.splitlines():
        m = FAILED_RE.match(line.strip())
        if m:
            failed.append(m.group(1))
            messages[m.group(1)] = (m.group(2) or "").strip()
        if " in " in line and ("passed" in line or "failed" in line or "error" in line):
            for n, word in SUMMARY_RE.findall(line):
                if word == "passed":
                    passed = int(n)
                elif word in ("error", "errors"):
                    failed.append(f"<{n} collection errors>")
    return RunResult(passed, sorted(failed), messages, p.returncode == 0, p.stdout[-4000:])


def _scratch(fixture_dir: str, prebuilt_dir: Optional[str], spec: FixtureSpec) -> str:
    d = tempfile.mkdtemp(prefix=f"sf-mut-{spec.name}-")
    for name in os.listdir(fixture_dir):
        if name in spec.prebuilt or name in ("__pycache__", ".pytest_cache"):
            continue
        src = os.path.join(fixture_dir, name)
        (shutil.copytree if os.path.isdir(src) else shutil.copy2)(src, os.path.join(d, name))
    for name in spec.prebuilt:
        os.symlink(os.path.join(prebuilt_dir or fixture_dir, name), os.path.join(d, name))
    return d


def _try(args) -> Tuple[Mutation, Optional[RunResult]]:
    fixture_dir, prebuilt_dir, spec, mut, python = args
    d = _scratch(fixture_dir, prebuilt_dir, spec)
    try:
        path = os.path.join(d, mut.file)
        with open(path) as f:
            src = f.read()
        with open(path, "w") as f:
            f.write(mut.apply(src))
        return mut, run_pytest(d, python, timeout=spec.timeout_s * 2)
    except Exception:
        return mut, None
    finally:
        shutil.rmtree(d, ignore_errors=True)


def visible_source(test_files: Dict[str, str], failing: Sequence[str], keep: int = 3) -> Tuple[str, List[str]]:
    """A single test module holding only the chosen failing test functions,
    with every import, helper and constant of the modules they came from."""
    chosen: List[Tuple[str, str]] = []
    for tid in failing:
        file, _, name = tid.partition("::")
        name = name.split("[")[0]
        if (file, name) not in chosen:
            chosen.append((file, name))
        if len(chosen) == keep:
            break
    parts, ids = [], []
    for file in dict.fromkeys(f for f, _ in chosen):
        src = test_files[file]
        tree = ast.parse(src)
        wanted = {n for f, n in chosen if f == file}
        lines = src.split("\n")
        drop = []
        for node in tree.body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_") and node.name not in wanted:
                start = min([d.lineno for d in node.decorator_list] + [node.lineno])
                drop.append((start, node.end_lineno))
        keep_lines = [ln for i, ln in enumerate(lines, 1) if not any(a <= i <= b for a, b in drop)]
        parts.append(re.sub(r"\n{3,}", "\n\n\n", "\n".join(keep_lines)).strip() + "\n")
        ids += [f"visible/test_visible.py::{n}" for f, n in chosen if f == file]
    return "\n\n".join(parts), ids


def generate(fixture_dir: str, out_root: str, spec: FixtureSpec, *, python: str = sys.executable, workers: int = 0, limit: int = 0) -> dict:
    """Generate tasks for one fixture into ``out_root/<family>/``."""
    fixture_dir = os.path.abspath(fixture_dir)
    report = {"fixture": spec.name, "family": spec.family, "mutants": 0, "kept": 0, "discarded": {}, "quarantined": 0, "operators": {}}

    prebuilt_dir = None
    if spec.prebuilt:
        # Preparation steps run once, in a copy, for validation; every
        # mutant's scratch copy links to what they built.
        prebuilt_dir = tempfile.mkdtemp(prefix=f"sf-prebuilt-{spec.name}-")
        _copy_into(fixture_dir, prebuilt_dir)
        env = {**os.environ, "PYTHONPATH": os.path.join(prebuilt_dir, "src")}
        for step in spec.steps:
            subprocess.run(step.replace("python ", f"{python} ", 1) if step.startswith("python ") else step, shell=True, cwd=prebuilt_dir, check=True, capture_output=True, env=env)

    # Rule 1, and rule 4's three-in-a-row run, on the unmutated fixture.
    base = _scratch(fixture_dir, prebuilt_dir, spec)
    runs = [run_pytest(base, python, timeout=spec.timeout_s * 2) for _ in range(3)]
    shutil.rmtree(base, ignore_errors=True)
    if not runs[0].ok:
        raise SystemExit(f"fixture {spec.name} does not pass its own tests:\n{runs[0].output}")
    total = runs[0].passed
    flaky = any(r.passed != total or not r.ok for r in runs)

    sources = {}
    for dirpath, _, files in os.walk(os.path.join(fixture_dir, "src")):
        for f in sorted(files):
            if f.endswith(".py") and f != "__init__.py":
                rel = os.path.relpath(os.path.join(dirpath, f), fixture_dir).replace(os.sep, "/")
                sources[rel] = open(os.path.join(dirpath, f)).read()
    test_files = {}
    for f in sorted(os.listdir(os.path.join(fixture_dir, "tests"))):
        if f.startswith("test_") and f.endswith(".py"):
            test_files[f"tests/{f}"] = open(os.path.join(fixture_dir, "tests", f)).read()

    muts: List[Mutation] = []
    for rel, src in sources.items():
        muts += mutants(src, rel)
    report["mutants"] = len(muts)
    jobs = [(fixture_dir, prebuilt_dir, spec, m, python) for m in muts]
    with ProcessPoolExecutor(max_workers=workers or os.cpu_count()) as ex:
        results = list(ex.map(_try, jobs, chunksize=4))

    def discard(reason: str) -> None:
        report["discarded"][reason] = report["discarded"].get(reason, 0) + 1

    seen = set()
    kept = 0
    fam_dir = os.path.join(out_root, spec.family)
    os.makedirs(fam_dir, exist_ok=True)
    for mut, res in results:
        if res is None:
            discard("error")
            continue
        n_fail = len(res.failed)
        if n_fail == 0:
            discard("equivalent: no test fails")
            continue
        if any(f.startswith("<") for f in res.failed):
            discard("collection error or timeout")
            continue
        if n_fail > 5:
            discard("unfocused: more than 5 tests fail")
            continue
        key = (tuple(res.failed), mut.new)
        if key in seen:
            discard("duplicate")
            continue
        # Rule 4: the inverse edit restores the original bytes exactly.
        original = sources[mut.file]
        if Mutation(mut.file, mut.start_line, mut.new, mut.old, "inverse", "").apply(mut.apply(original)) != original:
            discard("inverse does not restore the source")
            continue
        seen.add(key)
        if limit and kept >= limit:
            discard("over the per-fixture limit")
            continue
        kept += 1
        if flaky:
            report["quarantined"] += 1
        report["operators"][mut.operator] = report["operators"].get(mut.operator, 0) + 1
        write_task(fam_dir, spec, mut, res, test_files, total, quarantined=flaky)
    report["kept"] = kept
    report["hidden_tests"] = total
    if prebuilt_dir:
        shutil.rmtree(prebuilt_dir, ignore_errors=True)
    return report


def _copy_into(fixture_dir: str, dst: str) -> None:
    for name in os.listdir(fixture_dir):
        if name in ("__pycache__", ".pytest_cache"):
            continue
        src = os.path.join(fixture_dir, name)
        (shutil.copytree if os.path.isdir(src) else shutil.copy2)(src, os.path.join(dst, name))


def task_id(spec: FixtureSpec, mut: Mutation) -> str:
    h = hashlib.blake2b(json.dumps(mut.to_json(), sort_keys=True).encode(), digest_size=4).hexdigest()
    return f"{spec.name}-{h}"


def write_task(fam_dir: str, spec: FixtureSpec, mut: Mutation, res: RunResult, test_files: Dict[str, str], total: int, quarantined: bool = False) -> str:
    name = task_id(spec, mut)
    d = os.path.join(fam_dir, name)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(os.path.join(d, "visible"))
    vis, vis_ids = visible_source(test_files, res.failed)
    with open(os.path.join(d, "visible", "test_visible.py"), "w") as f:
        f.write(vis)
    with open(os.path.join(d, "mutation.json"), "w") as f:
        json.dump(mut.to_json(), f, indent=2)
    with open(os.path.join(d, "solution.patch"), "w") as f:
        f.write(mut.inverse_patch())
    first = res.failed[0]
    msg = res.messages.get(first, "") or "the assertion fails"
    shown = vis_ids[0] if vis_ids else first
    with open(os.path.join(d, "prompt.md"), "w") as f:
        f.write(PROMPT.format(test=shown, message=msg[:400]))
    steps = ['"python -m compileall -q src"'] + [json.dumps(s) for s in spec.steps] + ['"python -m pytest -q --collect-only visible/"']
    limits = "\n".join(f"{k} = {v}" for k, v in spec.limits.items())
    with open(os.path.join(d, "task.toml"), "w") as f:
        f.write(
            f"""# Generated by sandfork_tasks from fixtures/{spec.name}: {mut.operator} at {mut.file}:{mut.start_line}.
# {mut.description}. Fails {len(res.failed)} of {total} hidden tests.{"  QUARANTINED: the fixture is flaky." if quarantined else ""}

[task]
id = "{spec.family}/{name}"
family = "{spec.family}"
base = "{spec.base}"
timeout_s = 180
max_turns = 12

[prepare]
fixture = "fixtures/{spec.name}"
mutation = "mutation.json"
steps = [{", ".join(steps)}]

[agent]
prompt = "prompt.md"
tools = ["bash", "read_file", "write_file", "run_tests", "submit"]
allowed_paths = ["src/**"]

[verify]
kind = "pytest"
grader = "fixtures/{spec.name}/tests"
command = "python -I -m pytest -q -p no:cacheprovider --noconftest -c /grader/pytest.ini /grader"
timeout_s = {spec.timeout_s}
reward = "fraction_passed"
total = {total}
version = "pytest-v1"

[limits]
{limits}
"""
        )
    with open(os.path.join(d, "failing.json"), "w") as f:
        json.dump({"failing": res.failed, "messages": res.messages, "quarantined": quarantined}, f, indent=2)
    return d
