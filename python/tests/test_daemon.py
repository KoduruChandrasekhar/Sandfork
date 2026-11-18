"""End-to-end tests against a running sandforkd (``SANDFORK_SOCKET``).

A scripted agent (fixed tool-call sequences) replaces the model, so every
failure is reproducible. Skipped when no daemon is reachable.
"""

import asyncio
import os

import grpc
import pytest

import sandfork
from sandfork.v1 import sandfork_pb2 as pb
from sandfork_rl import orchestrator as orch
from sandfork_rl import rollout, tools
from sandfork_rl.generate import ByteTokenizer, ScriptedGenerator
from sandfork_rl.tasks import load_task

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
BUGFIX = os.path.join(ROOT, "tasks", "py-bugfix", "sample-0001")
DATA = os.path.join(ROOT, "tasks", "data-analysis", "sample-0001")
SOCK = os.environ.get("SANDFORK_SOCKET", "")

pytestmark = [pytest.mark.daemon, pytest.mark.skipif(not SOCK or not os.path.exists(SOCK.removeprefix("unix://")), reason="no sandforkd at SANDFORK_SOCKET")]

FIX = [
    tools.format_call("bash", cmd="python -m pytest -q visible | tail -2"),
    tools.format_call("bash", cmd="sed -i 's/other.hi + 1 == self.hi/other.hi + 1 == self.lo/' src/intervals/core.py"),
    tools.format_call("run_tests"),
    tools.format_call("submit"),
]
GIVE_UP = [tools.format_call("bash", cmd="ls"), tools.format_call("submit")]


def host_pid_of(sandbox_id: str) -> int:
    """The host PID of a sandbox's PID 1 (its shim), found by environment."""
    needle = f"SANDFORK_ID={sandbox_id}\0".encode()
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            with open(f"/proc/{name}/environ", "rb") as f:
                if needle in f.read():
                    return int(name)
        except OSError:
            continue
    raise LookupError(sandbox_id)


def kill_at(target_turn: int):
    """Kill the sandbox's PID 1 with SIGKILL from the host before a turn: a
    real crash, not a release."""
    import signal

    fired = []

    async def fault(turn, sbx):
        if turn == target_turn and not fired:
            fired.append(sbx.id)
            os.kill(host_pid_of(sbx.id), signal.SIGKILL)
            await asyncio.sleep(0.05)

    return fault


@pytest.fixture
async def client():
    async with sandfork.connect(SOCK) as c:
        yield c


async def test_prepare_is_idempotent_by_spec_hash(client):
    a = await client.prepare_task(BUGFIX)
    b = await client.prepare_task(BUGFIX)
    assert a.id == b.id and b.cached


async def test_group_exec_and_release(client):
    snap = await client.prepare_task(BUGFIX)
    async with client.fork_group(snap, 3, mode="f2") as g:
        assert len(g) == 3 and g.mode == "F2"
        outs = await asyncio.gather(*(s.exec("hostname; echo $((6*7))") for s in g))
        assert all(o.ok and o.stdout == b"sandbox\n42\n" for o in outs)
        gid = g.id
    live = [s for s in await client.list_sandboxes() if s.group_id == gid]
    assert live == [], "leaving the block releases every sandbox"


async def test_task_errors_are_not_retried_and_infra_errors_are_classified(client):
    snap = await client.prepare_task(BUGFIX)
    async with client.fork_group(snap, 1) as g:
        r = await g[0].exec("exit 3")
        assert r.exit_code == 3, "a failing command is the trajectory's outcome, not an error"
        with pytest.raises(sandfork.SandforkError) as e:
            await g[0].read_file("does/not/exist")
        assert e.value.is_task
    with pytest.raises(sandfork.SandforkError) as e:
        await client.fork_group("snap-does-not-exist", 1).open()
    assert e.value.error_class == "CLIENT"


async def test_scripted_rollouts_are_token_exact_and_rewarded(client):
    task = load_task(BUGFIX)
    tok = ByteTokenizer()
    snap = await client.prepare_task(BUGFIX)
    async with client.fork_group(snap, 2) as g:
        fixed, lazy = await asyncio.gather(
            rollout.run_rollout(client, task, tok, ScriptedGenerator(tok, FIX), g[0], rollout.RolloutConfig()),
            rollout.run_rollout(client, task, tok, ScriptedGenerator(tok, GIVE_UP), g[1], rollout.RolloutConfig()),
        )
    for t in (fixed, lazy):
        t.check()
        from sandfork_rl.trajectory import assert_token_exact

        assert_token_exact(t, t.meta["returned"])
    assert fixed.reward == 1.0 and fixed.turns == 4
    assert 0.9 < lazy.reward < 1.0, "the untouched task fails only the mutated behaviour"


async def test_exec_retried_with_the_same_request_id_never_runs_twice(client):
    """The connection-dropping counter test: the client cancels each call at a
    random point and retries with the same request id. A counter file in the
    sandbox proves no command ever ran twice."""
    import random

    snap = await client.prepare_task(BUGFIX)
    rng = random.Random(7)
    async with client.fork_group(snap, 1) as g:
        sbx = g[0]
        for i in range(12):
            req = pb.ExecRequest(request_id=f"counter-{i}-{os.getpid()}", sandbox_id=sbx.id, shell="sleep 0.05; echo x >> /workspace/counter; wc -l < /workspace/counter", timeout=None)

            async def consume():
                out = b""
                async for ev in client._stub.Exec(req):
                    if ev.WhichOneof("event") == "stdout":
                        out += ev.stdout
                return out

            task = asyncio.create_task(consume())
            await asyncio.sleep(rng.random() * 0.08)
            task.cancel()  # the connection drops somewhere in the call
            try:
                await task
            except (asyncio.CancelledError, grpc.RpcError):
                pass
            # The retry attaches to the original command, or replays its result.
            out = await consume()
            assert out.strip() == str(i + 1).encode(), f"call {i}: counter says {out!r}"
        lines = (await sbx.read_file("counter")).count(b"\n")
        assert lines == 12, f"12 logical calls ran {lines} times"


async def test_write_file_is_idempotent(client):
    snap = await client.prepare_task(BUGFIX)
    async with client.fork_group(snap, 1) as g:
        req = pb.WriteFileRequest(request_id=f"w-{os.getpid()}", sandbox_id=g[0].id, path="log.txt", data=b"once\n", append=True)
        for _ in range(3):
            await client._stub.WriteFile(req)
        assert await g[0].read_file("log.txt") == b"once\n"


async def test_an_abandoned_group_is_reclaimed_when_its_lease_expires(client):
    snap = await client.prepare_task(BUGFIX)
    req = pb.ForkGroupRequest(request_id=f"lease-{os.getpid()}", snapshot_id=snap.id, n=2, lease=sandfork.client._duration(2.0))
    g = await client._stub.ForkGroup(req)
    # The "orchestrator" crashes: nobody renews or releases.
    for _ in range(80):
        if not [s for s in await client.list_sandboxes() if s.group_id == g.id]:
            break
        await asyncio.sleep(0.1)
    assert not [s for s in await client.list_sandboxes() if s.group_id == g.id], "the group must be reclaimed within its lease"


async def test_a_killed_sandbox_resumes_from_its_last_checkpoint(client):
    task = load_task(BUGFIX)
    tok = ByteTokenizer()
    snap = await client.prepare_task(BUGFIX)
    script = [FIX[0], FIX[1], tools.format_call("bash", cmd="echo after-restore"), FIX[2], FIX[3]]
    cfg = rollout.RolloutConfig(checkpoint_every=2)
    async with client.fork_group(snap, 1) as g:
        # Kill the sandbox before turn 2's command; the checkpoint after turn 1
        # holds the fix, so the restored rollout still passes.
        t = await rollout.run_rollout(client, task, tok, ScriptedGenerator(tok, script), g[0], cfg, fault=kill_at(2))
    assert t.resumed and t.status == "ok", t.meta
    assert t.reward == 1.0
    t.check()


async def test_without_a_checkpoint_a_dead_sandbox_is_an_infra_error_not_a_zero(client):
    task = load_task(BUGFIX)
    tok = ByteTokenizer()
    snap = await client.prepare_task(BUGFIX)
    async with client.fork_group(snap, 1) as g:
        t = await rollout.run_rollout(client, task, tok, ScriptedGenerator(tok, FIX), g[0], rollout.RolloutConfig(checkpoint_every=0), fault=kill_at(1))
    assert t.status == "infra_error" and t.reward is None


async def test_branching_shares_the_trunk(client):
    tok = ByteTokenizer()
    cfg = orch.StepConfig(prompts_per_step=1, group_size=4, branch_factor=2, branch_turn=2)
    gen = ScriptedGenerator(tok, FIX)
    res = await orch.run_step(client, [BUGFIX], tok, gen, cfg, step=0)
    assert len(res.trajectories) == 4
    assert all(t.reward == 1.0 for t in res.trajectories)
    by_trunk = {}
    for t in res.trajectories:
        by_trunk.setdefault(t.branch_id, []).append(t)
    assert sorted(len(v) for v in by_trunk.values()) == [2, 2], "two trunks, each with two branches"
    for leaves in by_trunk.values():
        cut = leaves[0].spans[4].start  # prompt + two turns
        assert leaves[0].ids[:cut] == leaves[1].ids[:cut], "branches share their trunk token for token"
        assert leaves[0].sandbox_id != leaves[1].sandbox_id, "each branch runs in its own sandbox"
        assert all(t.branch_turn == 1 for t in leaves)
    assert gen.prefill_cached > 0, "branches reuse the trunk's prefix on the model side"


async def test_zygote_forks_share_state_but_not_writes(client):
    snap = await client.prepare_task(DATA)
    async with client.fork_group(snap, 2, mode="f3") as g:
        assert g.mode == "F3"
        ok, out = await g[0].eval("len(df)")
        assert ok and out.strip() == "200000"
        await g[0].eval("df['mine'] = 1; secret = 7")
        ok, out = await g[1].eval("('mine' in df.columns, 'secret' in dir())")
        assert out.strip() == "(False, False)"


async def test_gym_adapter(client):
    env = sandfork.SandforkEnv(BUGFIX, addr=SOCK)
    obs, info = await env.reset()
    assert "Failing test" in obs
    for action in FIX:
        obs, reward, done, info = await env.step(action)
        if done:
            break
    assert done and reward == 1.0
    await env.aclose()
