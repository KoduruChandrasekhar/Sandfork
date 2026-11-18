import os
import sqlite3

import pytest

from ledger import Ledger, LedgerError, Posting, days_negative, format_cents, income_statement, running_balance, top_movers

HERE = os.path.dirname(os.path.abspath(__file__))
SEEDED = os.environ.get("LEDGER_DB") or next(
    (p for p in (os.path.join(HERE, "..", "workspace", "data", "ledger.sqlite"), os.path.join(HERE, "..", "data", "ledger.sqlite"), "/workspace/data/ledger.sqlite") if os.path.exists(p)),
    None,
)


@pytest.fixture
def book():
    lg = Ledger()
    for name, kind in [("cash", "asset"), ("bank", "asset"), ("sales", "income"), ("rent", "expense"), ("wages", "expense"), ("loan", "liability")]:
        lg.open_account(name, kind)
    yield lg
    lg.close()


def pay(lg, debit, credit, amount, day):
    return lg.post([Posting(debit, amount), Posting(credit, -amount)], day)


@pytest.fixture(scope="module")
def seeded():
    if SEEDED is None:
        pytest.skip("the seeded database is built by the task's preparation step")
    lg = Ledger(SEEDED, readonly=True)
    yield lg
    lg.close()


# ---------------------------------------------------------------- accounts

def test_open_and_list_accounts(book):
    assert ("cash", "asset") in book.accounts() and len(book.accounts()) == 6


def test_accounts_are_sorted(book):
    names = [n for n, _ in book.accounts()]
    assert names == sorted(names)


@pytest.mark.parametrize("kind", ["assets", "revenue", ""])
def test_unknown_kind_is_rejected(book, kind):
    with pytest.raises(LedgerError):
        book.open_account("x", kind)


@pytest.mark.parametrize("name", ["", " cash", "cash "])
def test_bad_names_are_rejected(book, name):
    with pytest.raises(LedgerError):
        book.open_account(name, "asset")


def test_duplicate_account_is_rejected(book):
    with pytest.raises(LedgerError):
        book.open_account("cash", "asset")


def test_unknown_account_in_a_posting(book):
    with pytest.raises(LedgerError):
        pay(book, "cash", "nowhere", 100, 1)


# ---------------------------------------------------------------- posting

def test_post_updates_both_sides(book):
    pay(book, "cash", "sales", 1500, 3)
    assert book.balance("cash") == 1500 and book.balance("sales") == -1500


def test_transactions_are_numbered_in_order(book):
    assert pay(book, "cash", "sales", 1, 1) == 1
    assert pay(book, "cash", "sales", 1, 1) == 2


def test_unbalanced_transaction_is_rejected(book):
    with pytest.raises(LedgerError):
        book.post([Posting("cash", 100), Posting("sales", -99)], 1)


def test_single_posting_is_rejected(book):
    with pytest.raises(LedgerError):
        book.post([Posting("cash", 0)], 1)


def test_zero_posting_is_rejected(book):
    with pytest.raises(LedgerError):
        book.post([Posting("cash", 100), Posting("sales", -100), Posting("rent", 0)], 1)


def test_negative_day_is_rejected(book):
    with pytest.raises(LedgerError):
        pay(book, "cash", "sales", 5, -1)


def test_day_zero_is_allowed(book):
    pay(book, "cash", "sales", 5, 0)
    assert book.balance("cash") == 5


def test_three_way_split(book):
    book.post([Posting("rent", 700), Posting("wages", 300), Posting("bank", -1000)], 4)
    assert (book.balance("rent"), book.balance("wages"), book.balance("bank")) == (700, 300, -1000)


def test_rejected_transaction_writes_nothing(book):
    with pytest.raises(LedgerError):
        book.post([Posting("cash", 100), Posting("ghost", -100)], 1)
    assert book.trial_balance() == 0 and book.balance("cash") == 0


def test_trial_balance_is_zero(book):
    for i in range(20):
        pay(book, "cash", "sales", 100 + i, i)
    assert book.trial_balance() == 0


# ---------------------------------------------------------------- balances

def test_balance_through_a_day(book):
    pay(book, "cash", "sales", 100, 1)
    pay(book, "cash", "sales", 200, 2)
    pay(book, "cash", "sales", 400, 3)
    assert book.balance("cash", through_day=2) == 300
    assert book.balance("cash", through_day=0) == 0
    assert book.balance("cash", through_day=3) == 700


def test_balances_include_unused_accounts(book):
    pay(book, "cash", "sales", 10, 1)
    b = book.balances()
    assert b["loan"] == 0 and b["cash"] == 10 and len(b) == 6


def test_balances_by_kind(book):
    pay(book, "rent", "bank", 70, 1)
    pay(book, "wages", "bank", 30, 1)
    assert book.balances("expense") == {"rent": 70, "wages": 30}


def test_transactions_filtered_by_period(book):
    for d in range(1, 6):
        pay(book, "cash", "sales", d * 10, d)
    assert [day for _, day, _ in book.transactions("cash", 2, 4)] == [2, 3, 4]
    assert [day for _, day, _ in book.transactions("cash", 4)] == [4, 5]


def test_transactions_ordered_by_day_then_txn(book):
    pay(book, "cash", "sales", 1, 5)
    pay(book, "cash", "sales", 2, 3)
    pay(book, "cash", "sales", 3, 5)
    assert [(d, a) for _, d, a in book.transactions("cash")] == [(3, 2), (5, 1), (5, 3)]


# ---------------------------------------------------------------- reports

def test_income_statement(book):
    pay(book, "bank", "sales", 1000, 1)
    pay(book, "rent", "bank", 300, 2)
    pay(book, "wages", "bank", 200, 9)
    assert income_statement(book, 0, 5) == {"income": 1000, "expenses": 300, "net": 700}
    assert income_statement(book, 0, 9) == {"income": 1000, "expenses": 500, "net": 500}


def test_income_statement_period_is_inclusive(book):
    pay(book, "bank", "sales", 50, 5)
    assert income_statement(book, 5, 5)["income"] == 50
    assert income_statement(book, 6, 9)["income"] == 0


def test_running_balance_collapses_days(book):
    pay(book, "cash", "sales", 100, 1)
    pay(book, "rent", "cash", 30, 1)
    pay(book, "cash", "sales", 5, 4)
    assert running_balance(book, "cash") == [(1, 70), (4, 75)]


def test_days_negative(book):
    pay(book, "rent", "bank", 100, 1)
    pay(book, "bank", "sales", 300, 2)
    pay(book, "wages", "bank", 500, 3)
    assert days_negative(book, "bank") == 2


def test_top_movers(book):
    pay(book, "bank", "sales", 1000, 1)
    pay(book, "rent", "bank", 300, 2)
    pay(book, "cash", "sales", 50, 3)
    assert top_movers(book, 2) == [("sales", -1050), ("bank", 700)]


def test_top_movers_ties_break_by_name(book):
    pay(book, "rent", "cash", 10, 1)
    pay(book, "wages", "bank", 10, 1)
    assert [n for n, _ in top_movers(book, 4)] == ["bank", "cash", "rent", "wages"]


def test_top_movers_period_and_n(book):
    pay(book, "bank", "sales", 1000, 1)
    pay(book, "rent", "cash", 5, 8)
    assert top_movers(book, 1, 5, 9) == [("cash", -5)]
    assert top_movers(book, 0) == []


@pytest.mark.parametrize("cents,text", [(0, "0.00"), (5, "0.05"), (123, "1.23"), (100000, "1000.00"), (-250, "-2.50"), (-5, "-0.05")])
def test_format_cents(cents, text):
    assert format_cents(cents) == text


# ---------------------------------------------------------------- the seeded book

def test_seeded_trial_balance_is_zero(seeded):
    assert seeded.trial_balance() == 0


def test_seeded_has_all_accounts(seeded):
    assert len(seeded.accounts()) == 13


def test_seeded_income_statement_balances(seeded):
    s = income_statement(seeded, 0, 365)
    assert s["net"] == s["income"] - s["expenses"] and s["income"] > 0


def test_seeded_balance_through_matches_transactions(seeded):
    total = sum(a for _, d, a in seeded.transactions("cash", 0, 100))
    assert seeded.balance("cash", through_day=100) == total


def test_seeded_running_balance_ends_at_the_balance(seeded):
    rb = running_balance(seeded, "receivables")
    assert rb[-1][1] == seeded.balance("receivables")


def test_seeded_top_movers_are_sorted(seeded):
    movers = top_movers(seeded, 5)
    assert [abs(m) for _, m in movers] == sorted((abs(m) for _, m in movers), reverse=True)


def test_seeded_readonly(seeded):
    with pytest.raises(sqlite3.OperationalError):
        seeded.db.execute("DELETE FROM entries")
