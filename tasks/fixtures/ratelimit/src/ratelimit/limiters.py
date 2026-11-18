"""Rate limiters: a token bucket, a fixed window and a sliding log."""

from __future__ import annotations

from collections import deque
from typing import Callable, Deque, Dict, Hashable


class TokenBucket:
    """Holds up to ``capacity`` tokens and refills ``rate`` tokens per second."""

    def __init__(self, rate: float, capacity: float, clock: Callable[[], float]) -> None:
        if rate <= 0 or capacity <= 0:
            raise ValueError("rate and capacity must be positive")
        self.rate = rate
        self.capacity = capacity
        self._clock = clock
        self._tokens = capacity
        self._last = clock()

    def _refill(self) -> None:
        now = self._clock()
        elapsed = now - self._last
        if elapsed > 0:
            self._tokens = min(self.capacity, self._tokens + elapsed * self.rate)
            self._last = now

    @property
    def tokens(self) -> float:
        self._refill()
        return self._tokens

    def try_acquire(self, n: float = 1) -> bool:
        if n <= 0:
            raise ValueError("n must be positive")
        if n > self.capacity:
            return False
        self._refill()
        if self._tokens >= n:
            self._tokens -= n
            return True
        return False

    def wait_time(self, n: float = 1) -> float:
        """Seconds until ``n`` tokens are available; infinite if never."""
        if n > self.capacity:
            return float("inf")
        self._refill()
        missing = n - self._tokens
        if missing <= 0:
            return 0.0
        return missing / self.rate


class FixedWindow:
    """At most ``limit`` events per window of ``window`` seconds, with windows
    aligned to multiples of ``window``."""

    def __init__(self, limit: int, window: float, clock: Callable[[], float]) -> None:
        if limit <= 0 or window <= 0:
            raise ValueError("limit and window must be positive")
        self.limit = limit
        self.window = window
        self._clock = clock
        self._start = self._window_start(clock())
        self._count = 0

    def _window_start(self, t: float) -> float:
        return (t // self.window) * self.window

    def _roll(self) -> None:
        start = self._window_start(self._clock())
        if start != self._start:
            self._start = start
            self._count = 0

    def allow(self) -> bool:
        self._roll()
        if self._count < self.limit:
            self._count += 1
            return True
        return False

    def remaining(self) -> int:
        self._roll()
        return self.limit - self._count

    def reset_in(self) -> float:
        self._roll()
        return self._start + self.window - self._clock()


class SlidingLog:
    """At most ``limit`` events in any trailing interval of ``window`` seconds."""

    def __init__(self, limit: int, window: float, clock: Callable[[], float]) -> None:
        if limit <= 0 or window <= 0:
            raise ValueError("limit and window must be positive")
        self.limit = limit
        self.window = window
        self._clock = clock
        self._log: Deque[float] = deque()

    def _trim(self, now: float) -> None:
        while self._log and self._log[0] <= now - self.window:
            self._log.popleft()

    def allow(self) -> bool:
        now = self._clock()
        self._trim(now)
        if len(self._log) < self.limit:
            self._log.append(now)
            return True
        return False

    def count(self) -> int:
        self._trim(self._clock())
        return len(self._log)

    def retry_after(self) -> float:
        now = self._clock()
        self._trim(now)
        if len(self._log) < self.limit:
            return 0.0
        return self._log[0] + self.window - now


class KeyedLimiter:
    """One limiter per key, created on first use by ``factory``."""

    def __init__(self, factory: Callable[[], object], max_keys: int = 10_000) -> None:
        if max_keys <= 0:
            raise ValueError("max_keys must be positive")
        self._factory = factory
        self._limiters: Dict[Hashable, object] = {}
        self.max_keys = max_keys

    def allow(self, key: Hashable) -> bool:
        lim = self._limiters.get(key)
        if lim is None:
            if len(self._limiters) >= self.max_keys:
                oldest = next(iter(self._limiters))
                del self._limiters[oldest]
            lim = self._factory()
            self._limiters[key] = lim
        if isinstance(lim, TokenBucket):
            return lim.try_acquire()
        return lim.allow()  # type: ignore[attr-defined]

    def __len__(self) -> int:
        return len(self._limiters)
