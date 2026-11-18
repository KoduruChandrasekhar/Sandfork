"""Reports over a ledger: income statements, running balances, top movers."""

from __future__ import annotations

from typing import Dict, List, Tuple

from .store import Ledger


def income_statement(ledger: Ledger, first_day: int, last_day: int) -> Dict[str, int]:
    """Income, expenses and net for a period. Income accounts carry credits
    (negative amounts), so income is reported as a positive number."""
    income = expenses = 0
    for name, kind in ledger.accounts():
        if kind not in ("income", "expense"):
            continue
        total = sum(a for _, _, a in ledger.transactions(name, first_day, last_day))
        if kind == "income":
            income -= total
        else:
            expenses += total
    return {"income": income, "expenses": expenses, "net": income - expenses}


def running_balance(ledger: Ledger, name: str) -> List[Tuple[int, int]]:
    """(day, balance at the end of that day) for every day with activity."""
    out: List[Tuple[int, int]] = []
    bal = 0
    for _, day, amount in ledger.transactions(name):
        bal += amount
        if out and out[-1][0] == day:
            out[-1] = (day, bal)
        else:
            out.append((day, bal))
    return out


def top_movers(ledger: Ledger, n: int, first_day: int = 0, last_day: int = 1 << 62) -> List[Tuple[str, int]]:
    """The ``n`` accounts with the largest absolute net change in a period,
    largest first, ties broken by name."""
    if n <= 0:
        return []
    rows = ledger.db.execute(
        "SELECT a.name, SUM(e.amount) FROM entries e JOIN accounts a ON a.id = e.account WHERE e.day >= ? AND e.day <= ? GROUP BY a.id",
        (first_day, last_day),
    ).fetchall()
    ranked = sorted(((name, int(total)) for name, total in rows), key=lambda r: (-abs(r[1]), r[0]))
    return ranked[:n]


def days_negative(ledger: Ledger, name: str) -> int:
    """How many active days ended with a negative balance."""
    return sum(1 for _, bal in running_balance(ledger, name) if bal < 0)


def format_cents(amount: int) -> str:
    sign = "-" if amount < 0 else ""
    amount = abs(amount)
    return f"{sign}{amount // 100}.{amount % 100:02d}"
