"""Reading task directories from Python (the daemon reads them in Rust)."""

from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field
from typing import List


@dataclass
class TaskInfo:
    id: str
    family: str
    dir: str
    prompt: str
    tools: List[str] = field(default_factory=list)
    max_turns: int = 12
    timeout_s: int = 180
    allowed_paths: List[str] = field(default_factory=list)
    warm_process: str = ""


def load_task(task_dir: str) -> TaskInfo:
    with open(os.path.join(task_dir, "task.toml"), "rb") as f:
        t = tomllib.load(f)
    meta, agent = t["task"], t.get("agent", {})
    prompt_file = os.path.join(task_dir, agent.get("prompt", "prompt.md"))
    prompt = open(prompt_file, encoding="utf-8").read() if os.path.exists(prompt_file) else ""
    return TaskInfo(
        id=meta["id"],
        family=meta["family"],
        dir=os.path.abspath(task_dir),
        prompt=prompt,
        tools=list(agent.get("tools", [])),
        max_turns=int(meta.get("max_turns", 12)),
        timeout_s=int(meta.get("timeout_s", 180)),
        allowed_paths=list(agent.get("allowed_paths", [])),
        warm_process=t.get("prepare", {}).get("warm_process", ""),
    )


def find_tasks(root: str, family: str = "") -> List[str]:
    """Every task directory under ``root`` (optionally one family), sorted."""
    out = []
    base = os.path.join(root, family) if family else root
    for dirpath, dirnames, filenames in os.walk(base):
        if "task.toml" in filenames:
            out.append(dirpath)
            dirnames[:] = []
        dirnames[:] = [d for d in dirnames if d not in ("fixtures", "bases", "hidden", "visible", "repo", "__pycache__")]
    return sorted(out)
