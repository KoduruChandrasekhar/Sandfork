"""Python wrapper for the Sandfork zygote library.

A task's warm process imports its libraries, loads its data, and then calls
``serve()``. Under a zygote, ``serve()`` runs the fork-hygiene checks and then
never returns in the zygote itself: it returns ``True`` in each forked sandbox,
which carries on as that sandbox's stateful process. Started cold (no zygote),
``serve()`` returns ``False`` at once and the same program simply continues.

    import pandas as pd
    df = pd.read_parquet("data.parquet")      # expensive: done once

    import sandfork_zygote
    sandfork_zygote.serve()                   # fork point
    run_kernel({"df": df})                    # runs in every sandbox

The library is loaded with ``ctypes`` from the directory this file is in.
"""

from __future__ import annotations

import ctypes
import gc
import os
import sys

_LIB = None


class HygieneError(RuntimeError):
    """A fork-hygiene check failed. The message names the check."""


def _lib() -> ctypes.CDLL:
    global _LIB
    if _LIB is None:
        path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "libsandfork_zygote.so")
        # PyDLL keeps the GIL held across the call, as os.fork() does; the
        # fork happens inside it.
        lib = ctypes.PyDLL(path, use_errno=True)
        for fn in (lib.sandfork_zygote_check, lib.sandfork_zygote_serve):
            fn.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_size_t]
            fn.restype = ctypes.c_int
        _LIB = lib
    return _LIB


def check(upper: str | None = None, ctl_fd: int = -1) -> None:
    """Run the hygiene checks without serving. Raises ``HygieneError``."""
    buf = ctypes.create_string_buffer(1024)
    rc = _lib().sandfork_zygote_check(ctl_fd, upper.encode() if upper else None, buf, len(buf))
    if rc != 0:
        raise HygieneError(buf.value.decode(errors="replace"))


def under_zygote() -> bool:
    return "SANDFORK_ZYGOTE_FD" in os.environ


def serve(freeze: bool = True) -> bool:
    """Become a zygote if the daemon asked for one.

    Returns ``True`` in a forked sandbox and ``False`` on a cold start.
    ``freeze=False`` skips ``gc.freeze()``; it exists only to measure what
    freezing saves.
    """
    fd = os.environ.get("SANDFORK_ZYGOTE_FD")
    if fd is None:
        return False
    # Settle the collector before forking. Without this, the first collection
    # in every child walks, and writes to, every shared object, which copies
    # the pages they live on.
    gc.collect()
    if freeze:
        gc.freeze()
    upper = os.environ.get("SANDFORK_ZYGOTE_UPPER", "")
    buf = ctypes.create_string_buffer(1024)
    api = ctypes.pythonapi
    for name in ("PyOS_BeforeFork", "PyOS_AfterFork_Child", "PyOS_AfterFork_Parent"):
        getattr(api, name).restype = None
    lib = _lib()
    # The fork happens inside the library, so the interpreter is told about
    # it exactly as os.fork() would: BeforeFork in the zygote (which then
    # runs only Rust until it exits), AfterFork_Child in every sandbox.
    api.PyOS_BeforeFork()
    rc = lib.sandfork_zygote_serve(int(fd), upper.encode() or None, buf, len(buf))
    if rc == 0:
        api.PyOS_AfterFork_Child()
        for key in ("SANDFORK_ZYGOTE_FD", "SANDFORK_ZYGOTE_UPPER"):
            os.environ.pop(key, None)
        return True
    api.PyOS_AfterFork_Parent()
    if rc == 2:
        sys.stderr.write(f"sandfork_zygote: refused to serve: {buf.value.decode(errors='replace')}\n")
        os._exit(3)
    os._exit(0)
