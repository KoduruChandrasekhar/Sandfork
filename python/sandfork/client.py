"""The asyncio client for the ``sandfork.v1`` gRPC service."""

from __future__ import annotations

import asyncio
import os
import random
import uuid
from contextlib import asynccontextmanager
from dataclasses import dataclass, field
from typing import AsyncIterator, Awaitable, Callable, Dict, List, Optional, Sequence, TypeVar, Union

import grpc
from google.protobuf import duration_pb2

from .v1 import sandfork_pb2 as pb
from .v1 import sandfork_pb2_grpc as pb_grpc

T = TypeVar("T")

MODES = {
    "auto": pb.AUTO,
    "f1": pb.F1_COLD,
    "cold": pb.F1_COLD,
    "f2": pb.F2_FS,
    "fs": pb.F2_FS,
    "f3": pb.F3_ZYGOTE,
    "zygote": pb.F3_ZYGOTE,
    "f4": pb.F4_RESTORE,
    "restore": pb.F4_RESTORE,
}
MODE_NAMES = {pb.AUTO: "auto", pb.F1_COLD: "F1", pb.F2_FS: "F2", pb.F3_ZYGOTE: "F3", pb.F4_RESTORE: "F4"}
MAX_MESSAGE = 16 << 20


def default_socket() -> str:
    """``$SANDFORK_SOCKET``, else the daemon's default path."""
    env = os.environ.get("SANDFORK_SOCKET")
    if env:
        return env
    run = os.environ.get("XDG_RUNTIME_DIR")
    if run:
        return os.path.join(run, "sandfork.sock")
    return f"/tmp/sandfork-{os.environ.get('UID', 'user')}.sock"


def new_request_id() -> str:
    return uuid.uuid4().hex


def _duration(seconds: Optional[float]) -> Optional[duration_pb2.Duration]:
    if seconds is None:
        return None
    d = duration_pb2.Duration()
    d.FromNanoseconds(int(seconds * 1e9))
    return d


class SandforkError(Exception):
    """An error from the daemon, with its class: ``TASK``, ``INFRA`` or ``CLIENT``."""

    def __init__(self, error_class: str, code: str, message: str, status: Optional[grpc.StatusCode] = None):
        super().__init__(f"{error_class}/{code}: {message}")
        self.error_class = error_class
        self.code = code
        self.message = message
        self.status = status

    @property
    def is_task(self) -> bool:
        return self.error_class == "TASK"

    @property
    def is_infra(self) -> bool:
        return self.error_class == "INFRA"

    @property
    def retryable(self) -> bool:
        return self.is_infra or self.status in (grpc.StatusCode.UNAVAILABLE,)

    @classmethod
    def from_rpc(cls, e: grpc.RpcError) -> "SandforkError":
        md = {}
        try:
            md = {k: v for k, v in (e.trailing_metadata() or ())}
        except Exception:
            pass
        status = e.code() if hasattr(e, "code") else None
        klass = md.get("sandfork-error-class")
        if not klass:
            klass = "INFRA" if status in (grpc.StatusCode.UNAVAILABLE, grpc.StatusCode.DEADLINE_EXCEEDED, grpc.StatusCode.INTERNAL) else "CLIENT"
        details = e.details() if hasattr(e, "details") else str(e)
        return cls(klass, md.get("sandfork-error-code", str(status.name.lower() if status else "rpc")), details or "", status)


@dataclass
class Snapshot:
    id: str
    task_id: str
    kind: str
    layers: List[str]
    cached: bool = False
    prepare_micros: int = 0
    bytes_written: int = 0

    @classmethod
    def from_pb(cls, s: pb.Snapshot) -> "Snapshot":
        return cls(s.id, s.task_id, s.kind, list(s.layers), s.cached, s.prepare_micros, s.bytes_written)


@dataclass
class ExecResult:
    exit_code: int
    stdout: bytes
    stderr: bytes
    signal: int = 0
    timed_out: bool = False
    truncated: bool = False
    oom_killed: bool = False
    wall_ms: int = 0
    cpu_ms: int = 0
    max_rss_kb: int = 0
    dropped_bytes: int = 0
    tail_stdout: bytes = b""
    tail_stderr: bytes = b""

    @property
    def ok(self) -> bool:
        return self.exit_code == 0 and not self.timed_out and self.signal == 0

    @property
    def text(self) -> str:
        """stdout and stderr, decoded, with a marker where output was dropped."""
        out = self.stdout.decode(errors="replace")
        if self.truncated and self.tail_stdout:
            out += f"\n... [{self.dropped_bytes} bytes omitted] ...\n" + self.tail_stdout.decode(errors="replace")
        err = self.stderr.decode(errors="replace")
        if self.truncated and self.tail_stderr:
            err += "\n... [output omitted] ...\n" + self.tail_stderr.decode(errors="replace")
        return out + err


@dataclass
class VerifyResult:
    status: str
    reward: Optional[float]
    passed: int
    total: int
    verifier_version: str
    duration_ms: int
    files: int = 0
    lines_added: int = 0
    lines_removed: int = 0
    rejected_paths: List[str] = field(default_factory=list)
    flags: List[str] = field(default_factory=list)
    error: Optional[str] = None
    output_tail: str = ""

    @property
    def infra_error(self) -> bool:
        return self.status == "infra_error"

    @classmethod
    def from_pb(cls, v: pb.VerifyResult) -> "VerifyResult":
        d = v.diff
        return cls(
            status=v.status,
            reward=v.reward if v.HasField("reward") else None,
            passed=v.passed,
            total=v.total,
            verifier_version=v.verifier_version,
            duration_ms=v.duration_ms,
            files=d.files,
            lines_added=d.lines_added,
            lines_removed=d.lines_removed,
            rejected_paths=list(d.rejected_paths),
            flags=list(d.flags),
            error=f"{v.error.code}: {v.error.message}" if v.HasField("error") else None,
            output_tail=v.output_tail,
        )


class Sandbox:
    """A handle on one live sandbox."""

    def __init__(self, client: "Client", info: pb.Sandbox):
        self._c = client
        self.info = info
        self.id = info.id
        self.group_id = info.group_id
        self.task_id = info.task_id
        self.mode = MODE_NAMES.get(info.mode, "?")
        self.fork_micros = info.fork_micros

    def __repr__(self) -> str:
        return f"Sandbox({self.id}, task={self.task_id}, mode={self.mode})"

    async def exec(
        self,
        command: Union[str, Sequence[str]],
        *,
        timeout: float = 60.0,
        cwd: str = "",
        env: Optional[Dict[str, str]] = None,
        max_output: int = 64 << 10,
        stdin: bytes = b"",
        on_output: Optional[Callable[[str, bytes], None]] = None,
    ) -> ExecResult:
        """Run a command. A string runs through ``/bin/sh -c``. Output can be
        observed as it arrives with ``on_output(stream, chunk)``."""
        req = pb.ExecRequest(
            request_id=new_request_id(),
            sandbox_id=self.id,
            cwd=cwd,
            timeout=_duration(timeout),
            max_output_bytes=max_output,
            env=env or {},
            stdin=stdin,
        )
        if isinstance(command, str):
            req.shell = command
        else:
            req.argv.args.extend(command)

        async def once() -> ExecResult:
            out, err = bytearray(), bytearray()
            # A retry with the same request id replays the original command's
            # events from the start, so the buffers are reset on every attempt.
            async for ev in self._c._stub.Exec(req, timeout=timeout + 60):
                kind = ev.WhichOneof("event")
                if kind == "stdout":
                    out.extend(ev.stdout)
                    if on_output:
                        on_output("stdout", ev.stdout)
                elif kind == "stderr":
                    err.extend(ev.stderr)
                    if on_output:
                        on_output("stderr", ev.stderr)
                elif kind == "exit":
                    x = ev.exit
                    return ExecResult(
                        exit_code=x.exit_code,
                        stdout=bytes(out),
                        stderr=bytes(err),
                        signal=x.signal,
                        timed_out=x.timed_out,
                        truncated=x.truncated,
                        oom_killed=x.oom_killed,
                        wall_ms=x.wall_ms,
                        cpu_ms=x.user_ms + x.sys_ms,
                        max_rss_kb=x.max_rss_kb,
                        dropped_bytes=x.dropped_bytes,
                        tail_stdout=x.tail_stdout,
                        tail_stderr=x.tail_stderr,
                    )
            raise SandforkError("INFRA", "exec_no_exit", "the command's stream ended without an exit event")

        return await self._c._retry(once)

    async def read_file(self, path: str, *, max_bytes: int = 1 << 20) -> bytes:
        data = bytearray()
        while len(data) < max_bytes:
            want = min(256 << 10, max_bytes - len(data))
            chunk = await self._c._retry(lambda: self._c._stub.ReadFile(pb.ReadFileRequest(sandbox_id=self.id, path=path, offset=len(data), max_bytes=want)))
            data.extend(chunk.data)
            if chunk.eof or not chunk.data:
                break
        return bytes(data)

    async def write_file(self, path: str, data: Union[bytes, str], *, mode: int = 0, append: bool = False) -> int:
        if isinstance(data, str):
            data = data.encode()
        req = pb.WriteFileRequest(request_id=new_request_id(), sandbox_id=self.id, path=path, data=data, mode=mode, append=append)
        r = await self._c._retry(lambda: self._c._stub.WriteFile(req))
        return r.bytes_written

    async def eval(self, code: str, *, timeout: float = 30.0) -> "tuple[bool, str]":
        """Run code in the task's stateful kernel. Returns (ok, output)."""
        req = pb.EvalRequest(request_id=new_request_id(), sandbox_id=self.id, code=code, timeout=_duration(timeout))
        r = await self._c._retry(lambda: self._c._stub.Eval(req, timeout=timeout + 30))
        return r.ok, r.output

    async def snapshot(self, kind: str = "turn") -> Snapshot:
        req = pb.SnapshotRequest(request_id=new_request_id(), sandbox_id=self.id, kind=kind)
        return Snapshot.from_pb(await self._c._retry(lambda: self._c._stub.Snapshot(req)))

    async def checkpoint(self, turn: int = 0) -> str:
        req = pb.CheckpointRequest(request_id=new_request_id(), sandbox_id=self.id, turn=turn)
        return (await self._c._retry(lambda: self._c._stub.Checkpoint(req))).id

    def fork(self, n: int, **kw) -> "Group":
        """Branch this sandbox into ``n`` copies of its current state."""
        return self._c.fork(sandbox_id=self.id, n=n, **kw)

    async def verify(self) -> VerifyResult:
        req = pb.VerifyRequest(request_id=new_request_id(), sandbox_id=self.id)
        return VerifyResult.from_pb(await self._c._retry(lambda: self._c._stub.Verify(req, timeout=600)))

    async def diff(self, text: bool = False) -> pb.DiffResult:
        return await self._c._retry(lambda: self._c._stub.Diff(pb.DiffRequest(sandbox_id=self.id, text=text)))

    async def release(self) -> None:
        await self._c._stub.Release(pb.ReleaseRequest(sandbox_id=self.id))


class Group:
    """A gang-admitted group of sandboxes. Use as an async context manager:
    leaving the block releases every sandbox, even after an exception."""

    def __init__(self, client: "Client", start: Callable[[], Awaitable[pb.Group]], lease_s: float):
        self._c = client
        self._start = start
        self._lease_s = lease_s
        self._renewer: Optional[asyncio.Task] = None
        self.info: Optional[pb.Group] = None
        self.sandboxes: List[Sandbox] = []

    @property
    def id(self) -> str:
        return self.info.id if self.info else ""

    @property
    def queued_micros(self) -> int:
        return self.info.queued_micros if self.info else 0

    @property
    def fork_micros(self) -> int:
        return self.info.fork_micros if self.info else 0

    @property
    def mode(self) -> str:
        return MODE_NAMES.get(self.info.mode, "?") if self.info else "?"

    def __iter__(self):
        return iter(self.sandboxes)

    def __len__(self) -> int:
        return len(self.sandboxes)

    def __getitem__(self, i: int) -> Sandbox:
        return self.sandboxes[i]

    async def open(self) -> "Group":
        self.info = await self._start()
        self.sandboxes = [Sandbox(self._c, s) for s in self.info.sandboxes]
        self._renewer = asyncio.create_task(self._renew())
        return self

    async def _renew(self) -> None:
        token = self.info.lease.token if self.info else 0
        while True:
            await asyncio.sleep(max(1.0, self._lease_s / 3))
            try:
                await self._c._stub.RenewLease(pb.RenewLeaseRequest(group_id=self.id, extend=_duration(self._lease_s), token=token))
            except grpc.RpcError:
                pass  # the next attempt may succeed; release still happens on exit

    async def release(self) -> None:
        if self._renewer:
            self._renewer.cancel()
            self._renewer = None
        if self.info:
            # Release must complete even if the caller is being cancelled.
            await asyncio.shield(self._c._stub.Release(pb.ReleaseRequest(group_id=self.id)))
            self.info = None

    async def __aenter__(self) -> "Group":
        return await self.open()

    async def __aexit__(self, *exc) -> None:
        await self.release()


class Client:
    def __init__(self, channel: grpc.aio.Channel, *, retries: int = 4, backoff: float = 0.05):
        self._channel = channel
        self._stub = pb_grpc.SandforkStub(channel)
        self._retries = retries
        self._backoff = backoff

    async def close(self) -> None:
        await self._channel.close()

    async def __aenter__(self) -> "Client":
        return self

    async def __aexit__(self, *exc) -> None:
        await self.close()

    async def _retry(self, call: Callable[[], Awaitable[T]]) -> T:
        """Retry on INFRA and UNAVAILABLE with exponential backoff. ``call``
        must reuse one request id across attempts."""
        attempt = 0
        while True:
            try:
                return await call()
            except grpc.RpcError as e:
                err = SandforkError.from_rpc(e)
                attempt += 1
                if not err.retryable or attempt > self._retries:
                    raise err from None
                await asyncio.sleep(self._backoff * (2 ** (attempt - 1)) * (0.5 + random.random()))

    async def prepare_task(self, task_dir: str, *, force: bool = False) -> Snapshot:
        req = pb.PrepareTaskRequest(request_id=new_request_id(), task_dir=os.path.abspath(task_dir), force=force)
        return Snapshot.from_pb(await self._retry(lambda: self._stub.PrepareTask(req, timeout=1800)))

    def fork_group(
        self,
        snapshot: Union[Snapshot, str],
        n: int,
        *,
        mode: str = "auto",
        lease: float = 120.0,
        admission_deadline: float = 60.0,
        labels: Optional[Dict[str, str]] = None,
        memory_mb: int = 0,
    ) -> Group:
        snap_id = snapshot.id if isinstance(snapshot, Snapshot) else snapshot
        req = pb.ForkGroupRequest(
            request_id=new_request_id(),
            snapshot_id=snap_id,
            n=n,
            mode=MODES[mode.lower()],
            admission_deadline=_duration(admission_deadline),
            lease=_duration(lease),
            labels=labels or {},
        )
        if memory_mb:
            req.limits.memory_mb = memory_mb
        return Group(self, lambda: self._retry(lambda: self._stub.ForkGroup(req, timeout=admission_deadline + 600)), lease)

    def fork(self, *, n: int, snapshot_id: str = "", sandbox_id: str = "", lease: float = 120.0, admission_deadline: float = 60.0, labels: Optional[Dict[str, str]] = None) -> Group:
        req = pb.ForkRequest(request_id=new_request_id(), n=n, admission_deadline=_duration(admission_deadline), lease=_duration(lease), labels=labels or {})
        if sandbox_id:
            req.sandbox_id = sandbox_id
        else:
            req.snapshot_id = snapshot_id
        return Group(self, lambda: self._retry(lambda: self._stub.Fork(req, timeout=admission_deadline + 600)), lease)

    def restore(self, checkpoint_id: str, *, n: int = 1, lease: float = 120.0, labels: Optional[Dict[str, str]] = None) -> Group:
        req = pb.RestoreRequest(request_id=new_request_id(), checkpoint_id=checkpoint_id, n=n, lease=_duration(lease), labels=labels or {})
        return Group(self, lambda: self._retry(lambda: self._stub.Restore(req, timeout=600)), lease)

    async def stats(self) -> Dict[str, float]:
        return dict((await self._stub.Stats(pb.StatsRequest())).values)

    async def list_sandboxes(self) -> List[pb.SandboxInfo]:
        return list((await self._stub.ListSandboxes(pb.ListSandboxesRequest())).sandboxes)

    async def register_tasks(self, task_dirs: Sequence[str]) -> int:
        r = await self._stub.RegisterTasks(pb.RegisterTasksRequest(task_dirs=[os.path.abspath(d) for d in task_dirs]))
        return r.registered

    async def pick_tasks(self, n: int, *, group_size: int = 8, strategy: str = "band", family: str = "", seed: int = 0, new_epoch: bool = False) -> List[pb.TaskPick]:
        r = await self._stub.PickTasks(pb.PickTasksRequest(n=n, group_size=group_size, strategy=strategy, family=family, seed=seed, new_epoch=new_epoch))
        return list(r.tasks)

    async def gc(self, *, grace_seconds: int = 0, budget_bytes: int = 0) -> pb.GcResult:
        return await self._stub.Gc(pb.GcRequest(grace_seconds=grace_seconds, budget_bytes=budget_bytes))


def _channel(addr: str) -> grpc.aio.Channel:
    opts = [("grpc.max_receive_message_length", MAX_MESSAGE), ("grpc.max_send_message_length", MAX_MESSAGE)]
    if addr.startswith(("http://", "https://")):
        target = addr.split("://", 1)[1]
        if addr.startswith("https://"):
            return grpc.aio.secure_channel(target, grpc.ssl_channel_credentials(), options=opts)
        return grpc.aio.insecure_channel(target, options=opts)
    path = addr[len("unix://") :] if addr.startswith("unix://") else addr
    # gRPC would send the socket path as the HTTP/2 :authority, which is not
    # a valid authority and which the daemon's HTTP/2 server rejects.
    opts.append(("grpc.default_authority", "localhost"))
    return grpc.aio.insecure_channel(f"unix://{os.path.abspath(path)}", options=opts)


@asynccontextmanager
async def connect(addr: Optional[str] = None, **kw) -> AsyncIterator[Client]:
    """Connect to a daemon: a Unix socket path (default), ``unix:///path``,
    or an ``https://`` URL for a remote daemon behind TLS."""
    client = Client(_channel(addr or default_socket()), **kw)
    try:
        yield client
    finally:
        await client.close()
