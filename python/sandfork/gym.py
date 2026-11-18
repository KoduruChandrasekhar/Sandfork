"""A Gym-style adapter for frameworks that expect ``reset()`` and ``step()``.

    env = SandforkEnv("tasks/py-bugfix/intervals-0042")
    obs, info = await env.reset()
    while True:
        obs, reward, done, info = await env.step(action_text)
        if done:
            break
    await env.close()

An action is the model's text for one turn, containing one tool call between
``<tool>`` and ``</tool>`` tags. The reward is ``0.0`` until the episode is
done, then the verifier's reward. An infrastructure failure ends the episode
with ``reward = None`` and ``info["infra_error"]``: never a zero.
"""

from __future__ import annotations

from typing import Any, Dict, Optional, Tuple

from .client import Client, Group, Sandbox, connect


class SandforkEnv:
    def __init__(self, task_dir: str, *, addr: Optional[str] = None, mode: str = "auto", max_turns: Optional[int] = None):
        self.task_dir = task_dir
        self.addr = addr
        self.mode = mode
        self.max_turns = max_turns
        self._cm = None
        self._client: Optional[Client] = None
        self._group: Optional[Group] = None
        self.sandbox: Optional[Sandbox] = None
        self.turn = 0
        self.prompt = ""

    async def reset(self) -> Tuple[str, Dict[str, Any]]:
        from sandfork_rl import tools
        from sandfork_rl.tasks import load_task

        await self.close()
        if self._client is None:
            self._cm = connect(self.addr)
            self._client = await self._cm.__aenter__()
        task = load_task(self.task_dir)
        self.task = task
        self.max_turns = self.max_turns or task.max_turns
        snap = await self._client.prepare_task(self.task_dir)
        self._group = await self._client.fork_group(snap, 1, mode=self.mode).open()
        self.sandbox = self._group[0]
        self.turn = 0
        self.prompt = tools.render_prompt(task)
        return self.prompt, {"sandbox": self.sandbox.id, "snapshot": snap.id}

    async def step(self, action: str) -> Tuple[str, Optional[float], bool, Dict[str, Any]]:
        from sandfork_rl import tools
        from sandfork import SandforkError

        assert self.sandbox is not None, "call reset() first"
        self.turn += 1
        call = tools.parse_tool_call(action)
        info: Dict[str, Any] = {"turn": self.turn}
        try:
            if call is not None and call.name == "submit":
                return await self._finish(call, info)
            obs = await tools.execute(self.sandbox, call, allowed=self.task.tools)
        except SandforkError as e:
            if e.is_infra:
                info["infra_error"] = str(e)
                return "", None, True, info
            obs = f"error: {e.message}"
        if self.turn >= (self.max_turns or 1):
            return await self._finish(None, info, obs)
        return tools.format_observation(obs), 0.0, False, info

    async def _finish(self, call, info, obs: str = "") -> Tuple[str, Optional[float], bool, Dict[str, Any]]:
        from sandfork_rl import tools

        if call is not None:
            await tools.execute(self.sandbox, call, allowed=self.task.tools)
        v = await self.sandbox.verify()
        info["verify"] = v
        if v.infra_error:
            info["infra_error"] = v.error
        return tools.format_observation(obs) if obs else "", v.reward, True, info

    async def close(self) -> None:
        if self._group is not None:
            await self._group.release()
            self._group = None
            self.sandbox = None

    async def aclose(self) -> None:
        await self.close()
        if self._cm is not None:
            await self._cm.__aexit__(None, None, None)
            self._cm = None
            self._client = None
