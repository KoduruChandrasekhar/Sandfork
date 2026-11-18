import math

import pytest

from ratelimit import FixedWindow, KeyedLimiter, SlidingLog, TokenBucket


class Clock:
    def __init__(self, t=0.0):
        self.t = t

    def __call__(self):
        return self.t


# ---------------------------------------------------------------- token bucket

@pytest.mark.parametrize("rate,cap", [(0, 1), (1, 0), (-1, 5)])
def test_bucket_rejects_bad_parameters(rate, cap):
    with pytest.raises(ValueError):
        TokenBucket(rate, cap, Clock())


def test_bucket_starts_full():
    assert TokenBucket(1, 5, Clock()).tokens == 5


def test_bucket_drains():
    b = TokenBucket(1, 3, Clock())
    assert [b.try_acquire() for _ in range(4)] == [True, True, True, False]


def test_bucket_refills_over_time():
    clock = Clock()
    b = TokenBucket(2, 4, clock)
    for _ in range(4):
        b.try_acquire()
    clock.t = 1.0
    assert b.tokens == pytest.approx(2)
    assert b.try_acquire(2) and not b.try_acquire()


def test_bucket_never_exceeds_capacity():
    clock = Clock()
    b = TokenBucket(10, 3, clock)
    clock.t = 100
    assert b.tokens == 3


def test_bucket_multi_token_acquire():
    b = TokenBucket(1, 5, Clock())
    assert b.try_acquire(5) and not b.try_acquire(1)


def test_bucket_exact_amount_succeeds():
    clock = Clock()
    b = TokenBucket(1, 2, clock)
    b.try_acquire(2)
    clock.t = 1.0
    assert b.try_acquire(1)


def test_bucket_request_larger_than_capacity_never_succeeds():
    clock = Clock()
    b = TokenBucket(1, 2, clock)
    clock.t = 1000
    assert not b.try_acquire(3)
    assert math.isinf(b.wait_time(3))


@pytest.mark.parametrize("n", [0, -1])
def test_bucket_n_must_be_positive(n):
    with pytest.raises(ValueError):
        TokenBucket(1, 2, Clock()).try_acquire(n)


def test_bucket_failed_acquire_takes_nothing():
    b = TokenBucket(1, 3, Clock())
    b.try_acquire(2)
    assert not b.try_acquire(2)
    assert b.tokens == pytest.approx(1)


def test_bucket_wait_time():
    clock = Clock()
    b = TokenBucket(2, 4, clock)
    assert b.wait_time(1) == 0.0
    b.try_acquire(4)
    assert b.wait_time(1) == pytest.approx(0.5)
    assert b.wait_time(3) == pytest.approx(1.5)


def test_bucket_partial_refill():
    clock = Clock()
    b = TokenBucket(4, 4, clock)
    b.try_acquire(4)
    clock.t = 0.25
    assert b.tokens == pytest.approx(1)


def test_bucket_clock_going_backwards_does_not_add_tokens():
    clock = Clock(10)
    b = TokenBucket(1, 5, clock)
    b.try_acquire(5)
    clock.t = 5
    assert b.tokens == 0


# ---------------------------------------------------------------- fixed window

@pytest.mark.parametrize("limit,window", [(0, 1), (1, 0)])
def test_window_rejects_bad_parameters(limit, window):
    with pytest.raises(ValueError):
        FixedWindow(limit, window, Clock())


def test_window_allows_up_to_the_limit():
    w = FixedWindow(3, 10, Clock())
    assert [w.allow() for _ in range(4)] == [True, True, True, False]


def test_window_resets_at_the_boundary():
    clock = Clock(5)
    w = FixedWindow(2, 10, clock)
    w.allow(), w.allow()
    clock.t = 9.99
    assert not w.allow()
    clock.t = 10.0
    assert w.allow()


def test_window_remaining():
    w = FixedWindow(3, 10, Clock())
    w.allow()
    assert w.remaining() == 2


def test_window_remaining_after_reset():
    clock = Clock()
    w = FixedWindow(3, 10, clock)
    for _ in range(3):
        w.allow()
    clock.t = 25
    assert w.remaining() == 3


def test_window_reset_in():
    clock = Clock(13)
    w = FixedWindow(1, 10, clock)
    assert w.reset_in() == pytest.approx(7)
    clock.t = 20
    assert w.reset_in() == pytest.approx(10)


def test_window_alignment():
    clock = Clock(27)
    w = FixedWindow(1, 10, clock)
    w.allow()
    clock.t = 29.9
    assert not w.allow()
    clock.t = 30
    assert w.allow()


# ---------------------------------------------------------------- sliding log

@pytest.mark.parametrize("limit,window", [(0, 1), (2, -1)])
def test_log_rejects_bad_parameters(limit, window):
    with pytest.raises(ValueError):
        SlidingLog(limit, window, Clock())


def test_log_allows_up_to_the_limit():
    s = SlidingLog(2, 10, Clock())
    assert [s.allow(), s.allow(), s.allow()] == [True, True, False]


def test_log_slides():
    clock = Clock()
    s = SlidingLog(2, 10, clock)
    s.allow()
    clock.t = 5
    s.allow()
    clock.t = 9.9
    assert not s.allow()
    clock.t = 10
    assert s.allow(), "the event at t=0 left the window at t=10"
    assert not s.allow()


def test_log_does_not_reset_at_aligned_boundaries():
    clock = Clock(9)
    s = SlidingLog(1, 10, clock)
    s.allow()
    clock.t = 11
    assert not s.allow(), "unlike a fixed window, the log looks back a full window"


def test_log_count():
    clock = Clock()
    s = SlidingLog(5, 10, clock)
    for t in (0, 1, 2):
        clock.t = t
        s.allow()
    clock.t = 11
    assert s.count() == 1


def test_log_rejected_events_are_not_recorded():
    clock = Clock()
    s = SlidingLog(1, 10, clock)
    s.allow()
    clock.t = 5
    s.allow()
    clock.t = 10
    assert s.allow(), "the rejected event at t=5 must not extend the block"


def test_log_retry_after():
    clock = Clock()
    s = SlidingLog(2, 10, clock)
    assert s.retry_after() == 0.0
    s.allow()
    clock.t = 3
    s.allow()
    clock.t = 4
    assert s.retry_after() == pytest.approx(6)


# ---------------------------------------------------------------- keyed

def test_keyed_limits_each_key_separately():
    clock = Clock()
    k = KeyedLimiter(lambda: FixedWindow(1, 10, clock))
    assert k.allow("a") and k.allow("b")
    assert not k.allow("a") and not k.allow("b")
    assert len(k) == 2


def test_keyed_with_buckets():
    clock = Clock()
    k = KeyedLimiter(lambda: TokenBucket(1, 2, clock))
    assert [k.allow("x") for _ in range(3)] == [True, True, False]


def test_keyed_evicts_the_oldest_key():
    clock = Clock()
    k = KeyedLimiter(lambda: FixedWindow(1, 10, clock), max_keys=2)
    k.allow("a")
    k.allow("b")
    k.allow("c")
    assert len(k) == 2
    assert k.allow("a"), "a was evicted, so it starts with a fresh limiter"


def test_keyed_reusing_a_key_does_not_evict():
    clock = Clock()
    k = KeyedLimiter(lambda: TokenBucket(1, 5, clock), max_keys=2)
    for _ in range(4):
        k.allow("a")
        k.allow("b")
    assert len(k) == 2


def test_keyed_max_keys_must_be_positive():
    with pytest.raises(ValueError):
        KeyedLimiter(lambda: None, max_keys=0)


@pytest.mark.parametrize("limit", [1, 3, 7])
def test_window_and_log_agree_within_one_window(limit):
    clock = Clock(0)
    w = FixedWindow(limit, 100, clock)
    s = SlidingLog(limit, 100, clock)
    for i in range(limit + 3):
        clock.t = i
        assert w.allow() == s.allow()
