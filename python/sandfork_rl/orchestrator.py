"""The rollout orchestrator: one synchronous GRPO step, flat or branched.

One synchronous step:

1. The scheduler picks P tasks.
2. ``ForkGroup(task, G)`` for each.
3. All P x G rollouts run concurrently with asyncio.
4. Every final state is verified to get rewards.
5. Advantages are computed within each group.
6. The trainer takes one optimizer step (or a few).
7. The new weights reach the generator.
8. The groups are released and the step's trace is written.

A branched group (Part 9) runs a trunk to turn ``k``, snapshots it, forks
``b`` branches, and continues each: same G leaves, shared prefix paid once
on both sides.
"""

from __future__ import annotations

import asyncio
import json
import os
import time
from dataclasses import asdict, dataclass, field
from typing import Dict, List, Optional, Sequence

from . import grpo, rollout
from .rollout import RolloutConfig
from .tasks import TaskInfo, load_task
from .trace import Trace
from .trajectory import Trajectory, assert_token_exact


@dataclass
class StepConfig:
    prompts_per_step: int = 4
    group_size: int = 8
    mode: str = "auto"
    advantage: str = "dr_grpo"
    strategy: str = "band"
    family: str = ""
    #: 0: flat groups. b > 0: branch each group at turn ``branch_turn`` into b.
    branch_factor: int = 0
    branch_turn: int = 2
    rollout: RolloutConfig = field(default_factory=RolloutConfig)
    check_token_exact: bool = True


@dataclass
class StepResult:
    step: int
    trajectories: List[Trajectory]
    rewards: List[List[Optional[float]]]
    metrics: Dict[str, float]
    trace: Trace


async def _flat_group(client, task: TaskInfo, snap, tok, gen, cfg: StepConfig, trace: Trace, labels: Dict[str, str], gi: int) -> List[Trajectory]:
    t0 = trace.now_us()
    async with client.fork_group(snap, cfg.group_size, mode=cfg.mode, labels=labels) as group:
        trace.complete(f"g{gi}", "queue", t0, group.queued_micros)
        trace.complete(f"g{gi}", "fork", t0 + group.queued_micros, group.fork_micros, mode=group.mode, n=len(group))
        return list(await asyncio.gather(*(rollout.run_rollout(client, task, tok, gen, sbx, cfg.rollout, trace=trace, track=f"g{gi}/r{i}") for i, sbx in enumerate(group))))


async def _branched_group(client, task: TaskInfo, snap, tok, gen, cfg: StepConfig, trace: Trace, labels: Dict[str, str], gi: int) -> List[Trajectory]:
    """``G / b`` trunks run to turn k, each snapshot is forked b ways."""
    b = cfg.branch_factor
    trunks = max(1, cfg.group_size // b)
    out: List[Trajectory] = []
    async with client.fork_group(snap, trunks, mode=cfg.mode, labels=labels) as group:
        trace.complete(f"g{gi}", "fork", trace.now_us(), group.fork_micros, n=len(group))

        async def one_trunk(ti: int, sbx) -> List[Trajectory]:
            track = f"g{gi}/trunk{ti}"
            st = rollout.start(task, tok, sbx, group.id)
            await rollout.run_turns(client, task, tok, gen, st, cfg.rollout, until_turn=cfg.branch_turn, trace=trace, track=track)
            if st.done:
                # The trunk ended before the branch point: its leaves are copies.
                t = await rollout.finish(st, trace=trace, track=track)
                t.meta["returned"] = st.returned
                return [t] + [t.branch() for _ in range(b - 1)]
            with trace.span(track, "branch", b=b):
                branches = st.sandbox.fork(b, labels=labels)
                await branches.open()
            try:

                async def leaf(li: int, bsbx) -> Trajectory:
                    tr = st.traj.branch()
                    tr.sandbox_id, tr.group_id, tr.branch_id = bsbx.id, branches.id, ti
                    leaf_st = rollout.RolloutState(bsbx, tr, returned=[list(x) for x in st.returned])
                    await rollout.run_turns(client, task, tok, gen, leaf_st, cfg.rollout, trace=trace, track=f"{track}/b{li}")
                    t = await rollout.finish(leaf_st, trace=trace, track=f"{track}/b{li}")
                    t.meta["returned"] = leaf_st.returned
                    return t

                return list(await asyncio.gather(*(leaf(li, s) for li, s in enumerate(branches))))
            finally:
                await branches.release()

        for leaves in await asyncio.gather(*(one_trunk(i, s) for i, s in enumerate(group))):
            out.extend(leaves)
    return out


async def collect(client, tasks: Sequence[TaskInfo], snaps, tok, gen, cfg: StepConfig, trace: Trace, step: int) -> List[List[Trajectory]]:
    jobs = []
    for gi, (task, snap) in enumerate(zip(tasks, snaps)):
        labels = {"step": str(step), "policy_version": str(gen.policy_version), "prompt": task.id}
        fn = _branched_group if cfg.branch_factor > 1 else _flat_group
        jobs.append(fn(client, task, snap, tok, gen, cfg, trace, labels, gi))
    results = await asyncio.gather(*jobs, return_exceptions=True)
    groups: List[List[Trajectory]] = []
    for task, r in zip(tasks, results):
        if isinstance(r, BaseException):
            # A group that could not start (admission deadline, spawner
            # failure) is an infrastructure loss, never a zero reward.
            t = Trajectory(task_id=task.id, status="infra_error", meta={"error": repr(r)})
            groups.append([t])
        else:
            groups.append(r)
    return groups


def step_metrics(groups: List[List[Trajectory]], cfg: StepConfig) -> Dict[str, float]:
    flat = [t for g in groups for t in g]
    rewards = [[t.reward for t in g] for g in groups]
    valid = [t.reward for t in flat if t.reward is not None]
    m = grpo.summarize_groups(rewards)
    m.update(
        {
            "rollouts": float(len(flat)),
            "reward_mean": sum(valid) / len(valid) if valid else 0.0,
            "pass_rate": sum(1 for r in valid if r >= 1.0) / len(valid) if valid else 0.0,
            "infra_error_rate": sum(1 for t in flat if t.status == "infra_error") / max(len(flat), 1),
            "resumed_rate": sum(1 for t in flat if t.resumed) / max(len(flat), 1),
            "turns_mean": sum(t.turns for t in flat) / max(len(flat), 1),
            "rejected_paths": float(sum(len(t.meta.get("verify", {}).get("rejected_paths", [])) for t in flat)),
            "flagged": float(sum(1 for t in flat if t.meta.get("verify", {}).get("flags"))),
        }
    )
    return m


def build_batch(groups: List[List[Trajectory]], mode: str):
    """Trajectories and advantages for the trainer. Infrastructure errors and
    groups with fewer than two valid rollouts are dropped."""
    trajs: List[Trajectory] = []
    advs: List[float] = []
    for g in groups:
        a = grpo.advantages([t.reward for t in g], mode)
        if a is None:
            continue
        for t, adv in zip(g, a):
            if adv is not None and t.model_tokens > 0:
                trajs.append(t)
                advs.append(adv)
    return trajs, advs


async def run_step(client, task_dirs: Sequence[str], tok, gen, cfg: StepConfig, *, step: int, trainer=None, trace: Optional[Trace] = None) -> StepResult:
    trace = trace or Trace(f"step-{step}")
    t_step = time.monotonic()
    tasks = [load_task(d) for d in task_dirs]
    with trace.span("orchestrator", "prepare", n=len(tasks)):
        snaps = await asyncio.gather(*(client.prepare_task(d) for d in task_dirs))
    with trace.span("orchestrator", "rollouts"):
        groups = await collect(client, tasks, snaps, tok, gen, cfg, trace, step)
    if cfg.check_token_exact:
        for g in groups:
            for t in g:
                if t.spans:
                    t.check()
                    assert_token_exact(t, t.meta.get("returned", []))
    metrics = step_metrics(groups, cfg)
    if trainer is not None:
        trajs, advs = build_batch(groups, cfg.advantage)
        if trajs:
            with trace.span("trainer", "train", rollouts=len(trajs)):
                metrics.update(trainer.step(trajs, advs))
            with trace.span("weight_sync", "weight_sync"):
                gen.policy_version = trainer.version
    metrics["step_seconds"] = time.monotonic() - t_step
    for k, v in trace.breakdown().items():
        metrics[f"seconds_{k}"] = v
    return StepResult(step, [t for g in groups for t in g], [[t.reward for t in g] for g in groups], metrics, trace)


def write_step(out_dir: str, res: StepResult, extra: Optional[dict] = None) -> None:
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, "metrics.jsonl"), "a") as f:
        f.write(json.dumps({"step": res.step, **res.metrics, **(extra or {})}) + "\n")
    res.trace.save(os.path.join(out_dir, f"trace-step{res.step:04d}.json"), {"step": res.step})
    with open(os.path.join(out_dir, "trajectories.jsonl"), "a") as f:
        for t in res.trajectories:
            f.write(json.dumps({"step": res.step, "task": t.task_id, "reward": t.reward, "status": t.status, "turns": t.turns, "resumed": t.resumed, "tokens": len(t.ids), "model_tokens": t.model_tokens, "verify": t.meta.get("verify")}) + "\n")
