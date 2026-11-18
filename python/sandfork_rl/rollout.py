"""The multi-turn rollout, with crash recovery from checkpoints.

    ids = tokenize_prompt(task)              # chat template applied exactly once
    for each turn:
        out  = generate(ids)                 # token IDs in, token IDs out
        ids += out.token_ids                 # mask 1, behaviour logprobs kept
        call = parse(out)
        if no call or submit: break
        obs  = execute(call)                 # through Sandfork
        ids += tokenize(obs)                 # mask 0
        every k turns: checkpoint

If the sandbox dies for an infrastructure reason, the rollout is restored from
its last checkpoint at turn j: the trajectory is truncated to turn j and
continues from there with the current policy. Every kept token still carries
the log-probability of the policy that generated it, so the rollout remains a
valid sample. It is marked ``resumed``. If there is no checkpoint, or the
restore fails, the rollout is an infrastructure error with no reward: never a
reward of zero.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, List, Optional

from . import tools
from .tasks import TaskInfo
from .trace import Trace
from .trajectory import Trajectory


@dataclass
class RolloutConfig:
    max_new_tokens: int = 512
    max_context: int = 8192
    temperature: float = 1.0
    checkpoint_every: int = 2
    command_timeout_s: float = 60.0
    #: Retry an infrastructure failure once from the last checkpoint.
    resume_attempts: int = 1


@dataclass
class RolloutState:
    """What the orchestrator needs to resume or branch a rollout."""

    sandbox: object
    traj: Trajectory
    checkpoint: Optional[str] = None
    checkpoint_turn: int = -1
    done: bool = False
    #: Groups created by restores, which the orchestrator must release.
    extra_groups: List[object] = field(default_factory=list)
    returned: List[List[int]] = field(default_factory=list)


def start(task: TaskInfo, tok, sandbox, group_id: str = "") -> RolloutState:
    traj = Trajectory(task_id=task.id, sandbox_id=sandbox.id, group_id=group_id)
    traj.add_prompt(tok.chat_prompt(tools.SYSTEM_PROMPT.format(tools=", ".join(task.tools or tools.ALL_TOOLS)), task.prompt.strip()))
    return RolloutState(sandbox, traj)


async def _recover(client, st: RolloutState, cfg: RolloutConfig, trace: Optional[Trace], track: str) -> bool:
    """Restore the rollout's last checkpoint into a fresh sandbox."""
    from sandfork import SandforkError

    if st.checkpoint is None:
        return False
    try:
        with (trace.span(track, "restore", checkpoint=st.checkpoint) if trace else _null()):
            group = await client.restore(st.checkpoint, n=1).open()
    except SandforkError:
        return False
    st.extra_groups.append(group)
    st.sandbox = group[0]
    st.traj.truncate_to_turn(st.checkpoint_turn)
    st.traj.sandbox_id = st.sandbox.id
    st.returned = st.returned[: st.checkpoint_turn + 1]
    return True


def sandbox_lost(e) -> bool:
    """An infrastructure error, or the sandbox no longer existing (reclaimed
    after a lease expiry or a daemon restart): either way the rollout lost its
    environment, which says nothing about the policy."""
    return e.is_infra or e.code == "not_found"


class _null:
    def __enter__(self):
        return {}

    def __exit__(self, *a):
        return False


async def run_turns(
    client,
    task: TaskInfo,
    tok,
    gen,
    st: RolloutState,
    cfg: RolloutConfig,
    *,
    until_turn: Optional[int] = None,
    trace: Optional[Trace] = None,
    track: str = "rollout",
    fault: Optional[Callable[[int], bool]] = None,
) -> RolloutState:
    """Run turns until the rollout ends, or until ``until_turn`` turns have
    been completed (for branching). ``fault`` is failure injection for the
    recovery tests: an async callable ``fault(turn, sandbox)`` run before each
    turn's command."""
    from sandfork import SandforkError

    traj = st.traj
    resumes = 0
    turn = traj.turns
    while turn < task.max_turns and not st.done:
        if until_turn is not None and turn >= until_turn:
            return st
        with (trace.span(track, f"generate:t{turn}") if trace else _null()) as sp:
            out = await gen.generate(traj.ids, max_new_tokens=cfg.max_new_tokens, stop=[tools.TOOL_CLOSE], temperature=cfg.temperature, turn=turn)
            sp["tokens"] = len(out.token_ids)
            sp["cached_prompt_tokens"] = out.cached_prompt_tokens
        traj.add_generated(out.token_ids, out.logprobs, out.policy_version, turn)
        st.returned.append(list(out.token_ids))
        traj.meta.setdefault("entropy", []).append(out.entropy)
        traj.turns = turn + 1
        call = tools.parse_tool_call(tok.decode(out.token_ids))
        if call is None or call.name == "submit":
            if call is not None:
                try:
                    await tools.execute(st.sandbox, call, allowed=task.tools)
                except SandforkError as e:
                    if sandbox_lost(e):
                        traj.status = "infra_error"
                        traj.meta["error"] = str(e)
                        st.done = True
                        return st
            st.done = True
            break
        try:
            if fault is not None:
                await fault(turn, st.sandbox)
            with (trace.span(track, f"command:t{turn}", tool=call.name) if trace else _null()):
                obs = await tools.execute(st.sandbox, call, allowed=task.tools, timeout=cfg.command_timeout_s)
        except SandforkError as e:
            if not sandbox_lost(e):
                obs = f"error: {e.message}"
            elif resumes < cfg.resume_attempts and await _recover(client, st, cfg, trace, track):
                resumes += 1
                turn = traj.turns
                continue
            else:
                traj.status = "infra_error"
                traj.meta["error"] = str(e)
                st.done = True
                return st
        traj.add_observation(tok.encode(tools.format_observation(obs)), turn)
        if len(traj.ids) > cfg.max_context:
            st.done = True
            break
        if cfg.checkpoint_every and (turn + 1) % cfg.checkpoint_every == 0:
            try:
                with (trace.span(track, f"checkpoint:t{turn}") if trace else _null()):
                    st.checkpoint = await st.sandbox.checkpoint(turn)
                    st.checkpoint_turn = turn
            except SandforkError:
                pass  # a failed checkpoint costs recoverability, not the rollout
        turn += 1
    st.done = True
    return st


async def finish(st: RolloutState, *, trace: Optional[Trace] = None, track: str = "rollout") -> Trajectory:
    """Verify the final state. Hitting the turn or time limit is the agent's
    outcome: whatever state exists is graded, so partial credit is possible."""
    traj = st.traj
    if traj.status == "infra_error":
        traj.reward = None
        return traj
    from sandfork import SandforkError

    with (trace.span(track, "verify") if trace else _null()) as sp:
        try:
            v = await st.sandbox.verify()
        except SandforkError as e:
            traj.reward, traj.status = None, "infra_error"
            traj.meta["error"] = str(e)
            return traj
        sp["reward"] = v.reward
    traj.reward = v.reward
    traj.status = "infra_error" if v.infra_error else v.status
    traj.meta["verify"] = {"passed": v.passed, "total": v.total, "verifier": v.verifier_version, "rejected_paths": v.rejected_paths, "flags": v.flags, "error": v.error}
    return traj


async def run_rollout(client, task: TaskInfo, tok, gen, sandbox, cfg: RolloutConfig, *, trace: Optional[Trace] = None, track: str = "rollout", fault=None) -> Trajectory:
    st = start(task, tok, sandbox, getattr(sandbox, "group_id", ""))
    try:
        await run_turns(client, task, tok, gen, st, cfg, trace=trace, track=track, fault=fault)
        traj = await finish(st, trace=trace, track=track)
        traj.meta["returned"] = st.returned
        return traj
    finally:
        for g in st.extra_groups:
            await g.release()
