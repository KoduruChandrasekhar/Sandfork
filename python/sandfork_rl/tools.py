"""The agent's tool protocol: one module that the orchestrator and the tests
both import, so the parser and the formatter cannot drift.

The model calls one tool per turn, as a JSON object between ``<tool>`` and
``</tool>`` tags. The orchestrator runs it and appends the result between
``<obs>`` and ``</obs>`` tags. A malformed call returns an observation that
explains the error, and still counts as a turn.

    <tool>{"name": "bash", "args": {"cmd": "pytest -q visible"}}</tool>

Tools:

=============  ===========================  ==========================================
``bash``       ``cmd``                      runs with a per-command timeout
``read_file``  ``path``, ``start``, ``end``  size-capped; ``start``/``end`` are lines
``write_file`` ``path``, ``content``         size-capped; paths resolve inside the sandbox
``run_tests``  ``selector`` (optional)       visible tests only
``python``     ``code``                     the stateful kernel (data-analysis)
``calc``       ``expr``                     a stateless arithmetic evaluator (math-tool)
``submit``     ``answer`` (optional)        ends the rollout
=============  ===========================  ==========================================
"""

from __future__ import annotations

import json
import re
import shlex
from dataclasses import dataclass, field
from typing import Any, Dict, Optional, Sequence

TOOL_OPEN, TOOL_CLOSE = "<tool>", "</tool>"
OBS_OPEN, OBS_CLOSE = "<obs>", "</obs>"

#: Characters of a long output kept at each end.
KEEP_CHARS = 1500
MAX_FILE_BYTES = 64 << 10
MAX_WRITE_BYTES = 256 << 10
COMMAND_TIMEOUT_S = 60.0

ALL_TOOLS = ("bash", "read_file", "write_file", "run_tests", "python", "calc", "submit")

SYSTEM_PROMPT = """You are an agent working inside an isolated Linux sandbox.
Call exactly one tool per turn, as JSON between <tool> and </tool>, for example:
<tool>{{"name": "bash", "args": {{"cmd": "ls"}}}}</tool>
The result comes back between <obs> and </obs>.
Available tools: {tools}.
Call submit when you are done."""


@dataclass
class ToolCall:
    name: str
    args: Dict[str, Any] = field(default_factory=dict)
    #: Set when the call could not be parsed; ``name`` is then ``"invalid"``.
    error: Optional[str] = None


_CALL = re.compile(re.escape(TOOL_OPEN) + r"(.*?)" + re.escape(TOOL_CLOSE), re.S)


def parse_tool_call(text: str) -> Optional[ToolCall]:
    """The first tool call in ``text``.

    ``None`` if the text contains no ``<tool>`` tag at all (the model stopped
    without acting). A tag whose body is not a valid call gives a ``ToolCall``
    named ``invalid`` with an explanation, which costs the agent a turn.
    """
    m = _CALL.search(text)
    if m is None:
        if TOOL_OPEN in text:
            return ToolCall("invalid", error="the tool call is missing its closing </tool> tag")
        return None
    body = m.group(1).strip()
    try:
        obj = json.loads(body)
    except json.JSONDecodeError as e:
        return ToolCall("invalid", error=f"the tool call is not valid JSON: {e.msg} at position {e.pos}")
    if not isinstance(obj, dict) or not isinstance(obj.get("name"), str):
        return ToolCall("invalid", error='a tool call is a JSON object with a string "name"')
    args = obj.get("args", {})
    if not isinstance(args, dict):
        return ToolCall("invalid", error='"args" must be a JSON object')
    return ToolCall(obj["name"], args)


def format_call(name: str, **args: Any) -> str:
    """The canonical text of a tool call, as a scripted agent emits it."""
    return f"{TOOL_OPEN}{json.dumps({'name': name, 'args': args}, separators=(',', ':'))}{TOOL_CLOSE}"


def truncate_middle(text: str, keep: int = KEEP_CHARS) -> str:
    """Keep the first and last ``keep`` characters with a marker between."""
    if len(text) <= 2 * keep + 64:
        return text
    omitted = len(text) - 2 * keep
    return f"{text[:keep]}\n... [{omitted} characters omitted] ...\n{text[-keep:]}"


def format_observation(text: str, keep: int = KEEP_CHARS) -> str:
    """Wrap a tool result for the model. Long outputs keep head and tail."""
    return f"{OBS_OPEN}{truncate_middle(text, keep)}{OBS_CLOSE}"


def render_prompt(task) -> str:
    tools = ", ".join(task.tools) if task.tools else ", ".join(ALL_TOOLS)
    return SYSTEM_PROMPT.format(tools=tools) + "\n\n" + task.prompt.strip() + "\n"


def _int(v: Any, default: Optional[int] = None) -> Optional[int]:
    try:
        return int(v)
    except (TypeError, ValueError):
        return default


def _exec_text(r) -> str:
    status = f"exit code {r.exit_code}"
    if r.timed_out:
        status = f"timed out after {r.wall_ms} ms"
    elif r.oom_killed:
        status = "killed: out of memory"
    elif r.signal:
        status = f"killed by signal {r.signal}"
    return f"{r.text.rstrip()}\n[{status}]".lstrip("\n")


async def execute(sbx, call: Optional[ToolCall], *, allowed: Sequence[str] = (), timeout: float = COMMAND_TIMEOUT_S) -> str:
    """Run one tool call in a sandbox and return the observation text.

    Task-level failures (a failing command, a missing file, a bad path) are
    observations. Infrastructure errors propagate as ``SandforkError``.
    """
    from sandfork import SandforkError

    if call is None:
        return "error: no tool call found; call exactly one tool per turn, between <tool> and </tool>"
    if call.error:
        return f"error: {call.error}"
    if allowed and call.name not in allowed:
        return f"error: tool {call.name!r} is not available here; available: {', '.join(allowed)}"
    a = call.args
    try:
        if call.name == "bash":
            cmd = a.get("cmd")
            if not isinstance(cmd, str) or not cmd.strip():
                return 'error: bash needs a non-empty "cmd" string'
            return _exec_text(await sbx.exec(cmd, timeout=min(float(a.get("timeout", timeout)), timeout)))
        if call.name == "read_file":
            path = a.get("path")
            if not isinstance(path, str) or not path:
                return 'error: read_file needs a "path"'
            data = await sbx.read_file(path, max_bytes=MAX_FILE_BYTES + 1)
            text = data[:MAX_FILE_BYTES].decode(errors="replace")
            start, end = _int(a.get("start")), _int(a.get("end"))
            if start is not None or end is not None:
                lines = text.splitlines()
                s = max(1, start or 1)
                e = min(len(lines), end or len(lines))
                text = "\n".join(f"{i:>5}  {lines[i - 1]}" for i in range(s, e + 1))
            elif len(data) > MAX_FILE_BYTES:
                text += f"\n... [file truncated at {MAX_FILE_BYTES} bytes]"
            return text
        if call.name == "write_file":
            path, content = a.get("path"), a.get("content")
            if not isinstance(path, str) or not isinstance(content, str):
                return 'error: write_file needs a "path" and a "content" string'
            if len(content.encode()) > MAX_WRITE_BYTES:
                return f"error: content is larger than {MAX_WRITE_BYTES} bytes"
            n = await sbx.write_file(path, content)
            return f"wrote {n} bytes to {path}"
        if call.name == "run_tests":
            sel = a.get("selector", "")
            if not isinstance(sel, str):
                return 'error: "selector" must be a string'
            target = "visible/" + sel.lstrip("/").removeprefix("visible/") if sel else "visible"
            return _exec_text(await sbx.exec(f"python -m pytest -q -x --no-header -p no:cacheprovider {shlex.quote(target)}", timeout=timeout))
        if call.name == "python":
            code = a.get("code")
            if not isinstance(code, str):
                return 'error: python needs a "code" string'
            ok, out = await sbx.eval(code, timeout=timeout)
            return out if ok else f"{out}\n[raised an exception]"
        if call.name == "calc":
            expr = a.get("expr")
            if not isinstance(expr, str) or len(expr) > 500:
                return 'error: calc needs a short "expr" string'
            r = await sbx.exec(["python3", "-I", "-c", CALC_SOURCE, expr], timeout=10)
            return r.text.strip()
        if call.name == "submit":
            ans = a.get("answer")
            if ans is not None:
                await sbx.write_file("answer.txt", str(ans).strip() + "\n")
            return "submitted"
    except SandforkError as e:
        if e.is_infra or e.code == "not_found":
            raise
        return f"error: {e.message}"
    return f"error: unknown tool {call.name!r}; available: {', '.join(allowed or ALL_TOOLS)}"


#: Evaluated inside the sandbox: arithmetic only, no names, no calls except a
#: few math functions.
CALC_SOURCE = r"""
import ast, math, operator, sys
OPS = {ast.Add: operator.add, ast.Sub: operator.sub, ast.Mult: operator.mul, ast.Div: operator.truediv,
       ast.FloorDiv: operator.floordiv, ast.Mod: operator.mod, ast.Pow: operator.pow, ast.USub: operator.neg, ast.UAdd: operator.pos}
FUNCS = {"sqrt": math.sqrt, "abs": abs, "round": round, "floor": math.floor, "ceil": math.ceil, "log": math.log, "exp": math.exp}
def ev(n):
    if isinstance(n, ast.Constant) and isinstance(n.value, (int, float)): return n.value
    if isinstance(n, ast.BinOp) and type(n.op) in OPS:
        if isinstance(n.op, ast.Pow) and abs(ev(n.right)) > 100: raise ValueError("exponent too large")
        return OPS[type(n.op)](ev(n.left), ev(n.right))
    if isinstance(n, ast.UnaryOp) and type(n.op) in OPS: return OPS[type(n.op)](ev(n.operand))
    if isinstance(n, ast.Call) and isinstance(n.func, ast.Name) and n.func.id in FUNCS and not n.keywords:
        return FUNCS[n.func.id](*[ev(a) for a in n.args])
    raise ValueError("only arithmetic is allowed")
try:
    print(ev(ast.parse(sys.argv[1], mode="eval").body))
except Exception as e:
    print(f"error: {e}")
"""
