"""AST mutation operators, as in mutation testing.

Each operator finds sites in a module's source and yields single-site
mutants as line-level edits (``Mutation``), so a task records exactly what
changed and the inverse edit is the reference fix.

Operators:

* ``flip_compare``: ``<`` to ``<=`` and back, ``>`` to ``>=``, ``==`` to ``!=``.
* ``off_by_one``: ``+ 1`` to ``+ 2`` / ``- 0``, ``- 1`` to ``- 0`` / ``- 2``, an
  integer index or range bound plus or minus one.
* ``swap_args``: swap two positional arguments of a call.
* ``wrong_variable``: replace a name with another name of the same scope
  that is used on the same line or the line above.
* ``delete_statement``: remove an assignment, augmented assignment or
  expression statement (replaced by ``pass`` to keep the block valid).
* ``invert_bool``: ``True`` to ``False``, ``and`` to ``or``, drop a ``not``.
* ``change_constant``: a numeric constant to a neighbouring value.
* ``drop_branch``: replace the body of an ``if`` that returns with ``pass``.
"""

from __future__ import annotations

import ast
import io
import tokenize
from dataclasses import dataclass
from typing import Iterator, List, Optional, Tuple


@dataclass(frozen=True)
class Mutation:
    file: str
    start_line: int
    old: Tuple[str, ...]
    new: Tuple[str, ...]
    operator: str
    description: str

    def to_json(self) -> dict:
        return {"file": self.file, "start_line": self.start_line, "old": list(self.old), "new": list(self.new), "operator": self.operator, "description": self.description}

    def apply(self, text: str) -> str:
        lines = text.split("\n")
        at = self.start_line - 1
        if tuple(lines[at : at + len(self.old)]) != self.old:
            raise ValueError(f"{self.file}:{self.start_line}: source does not match the mutation")
        lines[at : at + len(self.old)] = list(self.new)
        return "\n".join(lines)

    def inverse_patch(self) -> str:
        """The reference fix as a unified diff."""
        n_new, n_old = len(self.new), len(self.old)
        out = [f"--- a/{self.file}", f"+++ b/{self.file}", f"@@ -{self.start_line},{n_new} +{self.start_line},{n_old} @@"]
        out += [f"-{line}" for line in self.new] + [f"+{line}" for line in self.old]
        return "\n".join(out) + "\n"


CMP_SWAPS = {ast.Lt: "<=", ast.LtE: "<", ast.Gt: ">=", ast.GtE: ">", ast.Eq: "!=", ast.NotEq: "=="}
CMP_TEXT = {ast.Lt: "<", ast.LtE: "<=", ast.Gt: ">", ast.GtE: ">=", ast.Eq: "==", ast.NotEq: "!="}


def _segment(src_lines: List[str], node: ast.AST) -> Optional[Tuple[int, int, int, int]]:
    if not hasattr(node, "lineno") or node.end_lineno is None:
        return None
    return node.lineno, node.col_offset, node.end_lineno, node.end_col_offset


def _replace_span(src_lines: List[str], line: int, col: int, end_col: int, text: str) -> Tuple[str, str]:
    old = src_lines[line - 1]
    return old, old[:col] + text + old[end_col:]


def _op_token_col(src_lines: List[str], left: ast.AST, right: ast.AST, op_text: str) -> Optional[Tuple[int, int]]:
    """Find the column of a binary or comparison operator between two
    operands on one line, using the tokenizer so strings never match."""
    if left.end_lineno != right.lineno:
        return None
    line = src_lines[left.end_lineno - 1]
    seg = line[left.end_col_offset : right.col_offset]
    try:
        toks = list(tokenize.generate_tokens(io.StringIO(seg).readline))
    except (tokenize.TokenError, IndentationError):
        return None
    for t in toks:
        if t.type == tokenize.OP and t.string == op_text:
            return left.end_col_offset + t.start[1], left.end_col_offset + t.end[1]
    return None


class _Sites(ast.NodeVisitor):
    def __init__(self, src: str, file: str):
        self.src = src
        self.lines = src.split("\n")
        self.file = file
        self.out: List[Mutation] = []
        self._funcs: List[ast.AST] = []

    def _single(self, line: int, new_line: str, op: str, desc: str) -> None:
        old = self.lines[line - 1]
        if new_line != old:
            self.out.append(Mutation(self.file, line, (old,), (new_line,), op, desc))

    def visit_FunctionDef(self, node: ast.FunctionDef) -> None:
        self._funcs.append(node)
        self.generic_visit(node)
        self._funcs.pop()

    visit_AsyncFunctionDef = visit_FunctionDef  # type: ignore[assignment]

    def visit_Compare(self, node: ast.Compare) -> None:
        if len(node.ops) == 1 and type(node.ops[0]) in CMP_SWAPS:
            span = _op_token_col(self.lines, node.left, node.comparators[0], CMP_TEXT[type(node.ops[0])])
            if span:
                old, new = _replace_span(self.lines, node.left.end_lineno, span[0], span[1], CMP_SWAPS[type(node.ops[0])])
                self._single(node.left.end_lineno, new, "flip_compare", f"{CMP_TEXT[type(node.ops[0])]} became {CMP_SWAPS[type(node.ops[0])]}")
        self.generic_visit(node)

    def visit_BinOp(self, node: ast.BinOp) -> None:
        if isinstance(node.op, (ast.Add, ast.Sub)) and isinstance(node.right, ast.Constant) and node.right.value == 1 and type(node.right.value) is int:
            seg = _segment(self.lines, node.right)
            if seg and seg[0] == seg[2]:
                for repl in ("2", "0"):
                    old, new = _replace_span(self.lines, seg[0], seg[1], seg[3], repl)
                    self._single(seg[0], new, "off_by_one", f"an off-by-one: 1 became {repl}")
        self.generic_visit(node)

    def visit_Subscript(self, node: ast.Subscript) -> None:
        sl = node.slice
        if isinstance(sl, ast.Constant) and type(sl.value) is int and -3 <= sl.value <= 3:
            seg = _segment(self.lines, sl)
            if seg and seg[0] == seg[2]:
                for delta in (1, -1):
                    v = sl.value + delta
                    old, new = _replace_span(self.lines, seg[0], seg[1], seg[3], str(v))
                    self._single(seg[0], new, "off_by_one", f"index {sl.value} became {v}")
        self.generic_visit(node)

    def visit_Call(self, node: ast.Call) -> None:
        args = node.args
        if len(args) >= 2 and not any(isinstance(a, ast.Starred) for a in args):
            a, b = args[0], args[1]
            sa, sb = _segment(self.lines, a), _segment(self.lines, b)
            if sa and sb and sa[0] == sa[2] == sb[0] == sb[2]:
                line = self.lines[sa[0] - 1]
                ta, tb = line[sa[1] : sa[3]], line[sb[1] : sb[3]]
                if ta != tb:
                    new = line[: sa[1]] + tb + line[sa[3] : sb[1]] + ta + line[sb[3] :]
                    self._single(sa[0], new, "swap_args", f"swapped arguments {ta} and {tb}")
        self.generic_visit(node)

    def visit_Name(self, node: ast.Name) -> None:
        if isinstance(node.ctx, ast.Load) and self._funcs and node.id not in ("self", "None", "True", "False"):
            fn = self._funcs[-1]
            names = sorted({n.id for n in ast.walk(fn) if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load)} | {a.arg for a in fn.args.args})
            near = {n.id for n in ast.walk(fn) if isinstance(n, ast.Name) and n.lineno in (node.lineno, node.lineno - 1)}
            for other in names:
                if other != node.id and other in near and other not in ("self", "cls") and not other[0].isupper():
                    old, new = _replace_span(self.lines, node.lineno, node.col_offset, node.end_col_offset, other)
                    self._single(node.lineno, new, "wrong_variable", f"{node.id} became {other}")
                    break
        self.generic_visit(node)

    def visit_Attribute(self, node: ast.Attribute) -> None:
        # self.lo vs self.hi, node.prev vs node.next: the classic wrong variable.
        pairs = {"lo": "hi", "hi": "lo", "prev": "next", "next": "prev", "head": "tail", "tail": "head", "hits": "misses", "misses": "hits", "rate": "capacity", "capacity": "rate"}
        if isinstance(node.ctx, ast.Load) and node.attr in pairs and node.end_lineno == node.lineno:
            line = self.lines[node.lineno - 1]
            end = node.end_col_offset
            start = end - len(node.attr)
            if line[start:end] == node.attr:
                new = line[:start] + pairs[node.attr] + line[end:]
                self._single(node.lineno, new, "wrong_variable", f".{node.attr} became .{pairs[node.attr]}")
        self.generic_visit(node)

    def visit_Constant(self, node: ast.Constant) -> None:
        if isinstance(node.value, bool):
            seg = _segment(self.lines, node)
            if seg and seg[0] == seg[2]:
                old, new = _replace_span(self.lines, seg[0], seg[1], seg[3], str(not node.value))
                self._single(seg[0], new, "invert_bool", f"{node.value} became {not node.value}")
        elif type(node.value) in (int, float) and node.value not in (1,) and abs(node.value) < 1e6:
            seg = _segment(self.lines, node)
            if seg and seg[0] == seg[2] and self.lines[seg[0] - 1][seg[1] : seg[3]] == repr(node.value):
                v = node.value + 1 if type(node.value) is int else node.value * 2
                old, new = _replace_span(self.lines, seg[0], seg[1], seg[3], repr(v))
                self._single(seg[0], new, "change_constant", f"constant {node.value!r} became {v!r}")

    def visit_BoolOp(self, node: ast.BoolOp) -> None:
        a, b = node.values[0], node.values[1]
        word = "and" if isinstance(node.op, ast.And) else "or"
        if a.end_lineno == b.lineno:
            line = self.lines[a.end_lineno - 1]
            gap = line[a.end_col_offset : b.col_offset]
            i = gap.find(f" {word} ")
            if i >= 0:
                start = a.end_col_offset + i + 1
                new = line[:start] + ("or" if word == "and" else "and") + line[start + len(word) :]
                self._single(a.end_lineno, new, "invert_bool", f"{word} became {'or' if word == 'and' else 'and'}")
        self.generic_visit(node)

    def visit_UnaryOp(self, node: ast.UnaryOp) -> None:
        if isinstance(node.op, ast.Not) and node.lineno == node.operand.lineno:
            line = self.lines[node.lineno - 1]
            if line[node.col_offset : node.col_offset + 4] == "not ":
                new = line[: node.col_offset] + line[node.col_offset + 4 :]
                self._single(node.lineno, new, "invert_bool", "a `not` was dropped")
        self.generic_visit(node)

    def _stmt(self, node: ast.stmt) -> None:
        if node.lineno == node.end_lineno and not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant)):
            line = self.lines[node.lineno - 1]
            indent = line[: len(line) - len(line.lstrip())]
            self._single(node.lineno, indent + "pass", "delete_statement", f"deleted `{line.strip()}`")

    def visit_Assign(self, node: ast.Assign) -> None:
        if self._funcs:
            self._stmt(node)
        self.generic_visit(node)

    def visit_AugAssign(self, node: ast.AugAssign) -> None:
        if self._funcs:
            self._stmt(node)
        self.generic_visit(node)

    def visit_Expr(self, node: ast.Expr) -> None:
        if self._funcs and isinstance(node.value, ast.Call):
            self._stmt(node)
        self.generic_visit(node)

    def visit_If(self, node: ast.If) -> None:
        if self._funcs and len(node.body) == 1 and isinstance(node.body[0], (ast.Return, ast.Raise)) and node.body[0].lineno == node.body[0].end_lineno and node.body[0].lineno != node.lineno:
            stmt = node.body[0]
            line = self.lines[stmt.lineno - 1]
            indent = line[: len(line) - len(line.lstrip())]
            self._single(stmt.lineno, indent + "pass", "drop_branch", f"dropped `{line.strip()}` from an if")
        self.generic_visit(node)


def mutants(src: str, file: str) -> List[Mutation]:
    """Every single-site mutant of a module, deduplicated, in source order.
    Each one is checked to still parse."""
    v = _Sites(src, file)
    v.visit(ast.parse(src))
    seen = set()
    out: List[Mutation] = []
    for m in sorted(v.out, key=lambda m: (m.start_line, m.operator, m.new)):
        key = (m.start_line, m.new)
        if key in seen:
            continue
        seen.add(key)
        try:
            ast.parse(m.apply(src))
        except (SyntaxError, ValueError):
            continue
        out.append(m)
    return out


def iter_operators(muts: List[Mutation]) -> Iterator[Tuple[str, int]]:
    counts: dict = {}
    for m in muts:
        counts[m.operator] = counts.get(m.operator, 0) + 1
    yield from sorted(counts.items())
