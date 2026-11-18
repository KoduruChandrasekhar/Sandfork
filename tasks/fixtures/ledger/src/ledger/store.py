"""A double-entry ledger over SQLite."""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass
from typing import Dict, Iterable, List, Optional, Tuple

SCHEMA = """
CREATE TABLE IF NOT EXISTS accounts(id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, kind TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS entries(id INTEGER PRIMARY KEY, txn INTEGER NOT NULL, account INTEGER NOT NULL, amount INTEGER NOT NULL, day INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS entries_account ON entries(account, day);
CREATE INDEX IF NOT EXISTS entries_txn ON entries(txn);
"""

KINDS = ("asset", "liability", "income", "expense", "equity")


@dataclass(frozen=True)
class Posting:
    account: str
    amount: int  # cents; debits positive, credits negative


class LedgerError(ValueError):
    pass


class Ledger:
    def __init__(self, path: str = ":memory:", readonly: bool = False) -> None:
        uri = f"file:{path}?mode=ro" if readonly else path
        self.db = sqlite3.connect(uri, uri=readonly)
        if not readonly:
            self.db.executescript(SCHEMA)

    def close(self) -> None:
        self.db.close()

    def open_account(self, name: str, kind: str) -> int:
        if kind not in KINDS:
            raise LedgerError(f"unknown account kind {kind!r}")
        if not name or name != name.strip():
            raise LedgerError("account names must be non-empty and unpadded")
        try:
            cur = self.db.execute("INSERT INTO accounts(name, kind) VALUES(?, ?)", (name, kind))
        except sqlite3.IntegrityError:
            raise LedgerError(f"account {name!r} already exists") from None
        return int(cur.lastrowid)

    def _account_id(self, name: str) -> int:
        row = self.db.execute("SELECT id FROM accounts WHERE name = ?", (name,)).fetchone()
        if row is None:
            raise LedgerError(f"no account {name!r}")
        return int(row[0])

    def post(self, postings: Iterable[Posting], day: int) -> int:
        """Record one balanced transaction. Returns its transaction number."""
        items = list(postings)
        if len(items) < 2:
            raise LedgerError("a transaction needs at least two postings")
        if sum(p.amount for p in items) != 0:
            raise LedgerError("postings must balance to zero")
        if any(p.amount == 0 for p in items):
            raise LedgerError("zero postings are not allowed")
        if day < 0:
            raise LedgerError("day must be non-negative")
        ids = [self._account_id(p.account) for p in items]
        txn = (self.db.execute("SELECT COALESCE(MAX(txn), 0) FROM entries").fetchone()[0]) + 1
        self.db.executemany("INSERT INTO entries(txn, account, amount, day) VALUES(?, ?, ?, ?)", [(txn, a, p.amount, day) for a, p in zip(ids, items)])
        return txn

    def balance(self, name: str, through_day: Optional[int] = None) -> int:
        aid = self._account_id(name)
        if through_day is None:
            row = self.db.execute("SELECT COALESCE(SUM(amount), 0) FROM entries WHERE account = ?", (aid,)).fetchone()
        else:
            row = self.db.execute("SELECT COALESCE(SUM(amount), 0) FROM entries WHERE account = ? AND day <= ?", (aid, through_day)).fetchone()
        return int(row[0])

    def balances(self, kind: Optional[str] = None) -> Dict[str, int]:
        q = "SELECT a.name, COALESCE(SUM(e.amount), 0) FROM accounts a LEFT JOIN entries e ON e.account = a.id"
        args: Tuple = ()
        if kind is not None:
            q += " WHERE a.kind = ?"
            args = (kind,)
        q += " GROUP BY a.id ORDER BY a.name"
        return {name: int(total) for name, total in self.db.execute(q, args)}

    def trial_balance(self) -> int:
        """Sum of every entry; zero for a consistent ledger."""
        return int(self.db.execute("SELECT COALESCE(SUM(amount), 0) FROM entries").fetchone()[0])

    def transactions(self, name: str, first_day: int = 0, last_day: Optional[int] = None) -> List[Tuple[int, int, int]]:
        """(txn, day, amount) for one account, in order."""
        aid = self._account_id(name)
        last = last_day if last_day is not None else 1 << 62
        rows = self.db.execute("SELECT txn, day, amount FROM entries WHERE account = ? AND day >= ? AND day <= ? ORDER BY day, txn", (aid, first_day, last))
        return [(int(t), int(d), int(a)) for t, d, a in rows]

    def accounts(self) -> List[Tuple[str, str]]:
        return [(n, k) for n, k in self.db.execute("SELECT name, kind FROM accounts ORDER BY name")]
