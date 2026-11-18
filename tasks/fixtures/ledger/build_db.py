"""Seed the ledger database used by the tests: deterministic, and large
enough that preparation takes seconds and a write copies a big file up.

    python build_db.py --txns 150000 --out data/ledger.sqlite
"""

import argparse
import os
import random
import sqlite3
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "src"))

from ledger.store import SCHEMA  # noqa: E402

ACCOUNTS = [("cash", "asset"), ("bank", "asset"), ("receivables", "asset"), ("inventory", "asset"), ("payables", "liability"), ("loan", "liability"),
            ("sales", "income"), ("interest", "income"), ("rent", "expense"), ("wages", "expense"), ("supplies", "expense"), ("power", "expense"), ("owner", "equity")]
FLOWS = [("bank", "sales"), ("cash", "sales"), ("receivables", "sales"), ("rent", "bank"), ("wages", "bank"), ("supplies", "cash"), ("power", "bank"),
         ("inventory", "payables"), ("payables", "bank"), ("bank", "receivables"), ("bank", "interest"), ("bank", "loan"), ("loan", "bank"), ("bank", "owner")]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--txns", type=int, default=150_000)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--out", default="data/ledger.sqlite")
    a = ap.parse_args()
    os.makedirs(os.path.dirname(os.path.abspath(a.out)), exist_ok=True)
    if os.path.exists(a.out):
        os.remove(a.out)
    rng = random.Random(a.seed)
    db = sqlite3.connect(a.out)
    db.executescript(SCHEMA)
    db.executemany("INSERT INTO accounts(id, name, kind) VALUES(?, ?, ?)", [(i + 1, n, k) for i, (n, k) in enumerate(ACCOUNTS)])
    ids = {n: i + 1 for i, (n, _) in enumerate(ACCOUNTS)}
    rows = []
    for txn in range(1, a.txns + 1):
        debit, credit = FLOWS[rng.randrange(len(FLOWS))]
        amount = rng.randrange(100, 500_000)
        day = txn * 365 // a.txns
        rows.append((txn, ids[debit], amount, day))
        rows.append((txn, ids[credit], -amount, day))
    db.executemany("INSERT INTO entries(txn, account, amount, day) VALUES(?, ?, ?, ?)", rows)
    db.commit()
    db.execute("VACUUM")
    db.close()


if __name__ == "__main__":
    main()
