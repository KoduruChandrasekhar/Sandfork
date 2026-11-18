"""The stateful kernel behind the ``python`` tool of data-analysis tasks.

    python sandfork_kernel.py --loader /workspace/load.py

The loader is a Python file. If it defines ``load()``, that function's return
value (a dict) becomes the kernel's namespace; otherwise the file's own
globals do. Loading happens once. Then ``sandfork_zygote.serve()`` is the fork
point: under a zygote every sandbox continues from here with the data already
in memory, copy-on-write.

Protocol, one request per connection on a Unix socket:

    request:  u32 little-endian length, then that many bytes of code
    reply:    u32 little-endian length, one status byte (1 ok, 0 raised),
              then that many bytes of output

Code runs like a notebook cell: statements execute in the persistent
namespace, and a trailing expression's value is printed.
"""

from __future__ import annotations

import argparse
import ast
import contextlib
import io
import os
import runpy
import socket
import struct
import traceback

SOCK = os.environ.get("SANDFORK_KERNEL_SOCK", "/run/sandfork-kernel.sock")
MAX_OUTPUT = 64 * 1024
MAX_CODE = 1 << 20


def run_cell(namespace: dict, code: str) -> tuple[bool, str]:
    out = io.StringIO()
    ok = True
    try:
        tree = ast.parse(code, mode="exec")
        last = None
        if tree.body and isinstance(tree.body[-1], ast.Expr):
            last = ast.Expression(tree.body.pop().value)
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            exec(compile(tree, "<cell>", "exec"), namespace)
            if last is not None:
                value = eval(compile(last, "<cell>", "eval"), namespace)
                if value is not None:
                    print(repr(value))
    except BaseException:  # the agent's code may raise anything, SystemExit included
        ok = False
        out.write(traceback.format_exc(limit=4))
    text = out.getvalue()
    if len(text) > MAX_OUTPUT:
        half = MAX_OUTPUT // 2
        text = text[:half] + f"\n... [{len(text) - MAX_OUTPUT} characters omitted] ...\n" + text[-half:]
    return ok, text


def _read_exact(conn: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("peer closed")
        buf.extend(chunk)
    return bytes(buf)


def serve(namespace: dict, path: str = SOCK) -> None:
    with contextlib.suppress(FileNotFoundError):
        os.unlink(path)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(path)
    os.chmod(path, 0o666)
    server.listen(4)
    while True:
        conn, _ = server.accept()
        with conn:
            try:
                (n,) = struct.unpack("<I", _read_exact(conn, 4))
                if n > MAX_CODE:
                    ok, text = False, "code too large"
                else:
                    ok, text = run_cell(namespace, _read_exact(conn, n).decode("utf-8", errors="replace"))
                body = text.encode("utf-8", errors="replace")
                conn.sendall(struct.pack("<IB", len(body), 1 if ok else 0) + body)
            except (ConnectionError, OSError):
                continue


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--loader", required=True, help="Python file that builds the kernel's namespace")
    ap.add_argument("--no-freeze", action="store_true", help="skip gc.freeze() before forking (for measurement)")
    args = ap.parse_args()

    globs = runpy.run_path(args.loader)
    namespace = globs["load"]() if callable(globs.get("load")) else {k: v for k, v in globs.items() if not k.startswith("__")}
    namespace.setdefault("__name__", "__kernel__")

    import sandfork_zygote

    sandfork_zygote.serve(freeze=not args.no_freeze)
    serve(namespace)


if __name__ == "__main__":
    main()
