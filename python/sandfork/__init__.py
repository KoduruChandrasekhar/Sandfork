"""Python SDK for Sandfork.

    import asyncio
    import sandfork

    async def main():
        async with sandfork.connect() as sf:
            snap = await sf.prepare_task("tasks/py-bugfix/intervals-0042")
            async with sf.fork_group(snap, n=8, mode="auto", labels={"step": "37"}) as group:
                results = await asyncio.gather(*(sbx.exec("pytest -q visible") for sbx in group))
                rewards = await asyncio.gather(*(sbx.verify() for sbx in group))
            # leaving the block releases all eight sandboxes, even after an exception

    asyncio.run(main())

Semantics the SDK guarantees:

* One ``request_id`` per logical call, reused on every retry of that call, so
  the daemon's idempotency cache can make retries safe.
* Retries with exponential backoff only on ``INFRA`` errors and
  ``UNAVAILABLE``, never on ``TASK`` errors.
* Leases renewed in the background while a group is open.
"""

from .client import (
    Client,
    ExecResult,
    Group,
    Sandbox,
    SandforkError,
    Snapshot,
    VerifyResult,
    connect,
    default_socket,
)
from .gym import SandforkEnv

__all__ = [
    "Client",
    "ExecResult",
    "Group",
    "Sandbox",
    "SandforkEnv",
    "SandforkError",
    "Snapshot",
    "VerifyResult",
    "connect",
    "default_socket",
]

__version__ = "0.1.0"
