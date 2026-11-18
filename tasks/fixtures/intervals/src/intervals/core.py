"""Closed integer intervals and normalised sets of them."""

from __future__ import annotations

from bisect import bisect_right
from dataclasses import dataclass
from typing import Iterable, Iterator, List, Optional


@dataclass(frozen=True, order=True)
class Interval:
    """A closed interval [lo, hi] over the integers."""

    lo: int
    hi: int

    def __post_init__(self) -> None:
        if self.lo > self.hi:
            raise ValueError(f"empty interval: lo={self.lo} > hi={self.hi}")

    @property
    def length(self) -> int:
        return self.hi - self.lo + 1

    def contains(self, x: int) -> bool:
        return self.lo <= x <= self.hi

    def covers(self, other: "Interval") -> bool:
        return self.lo <= other.lo and other.hi <= self.hi

    def overlaps(self, other: "Interval") -> bool:
        return self.lo <= other.hi and other.lo <= self.hi

    def adjacent(self, other: "Interval") -> bool:
        return self.hi + 1 == other.lo or other.hi + 1 == self.lo

    def intersect(self, other: "Interval") -> Optional["Interval"]:
        lo = max(self.lo, other.lo)
        hi = min(self.hi, other.hi)
        if lo > hi:
            return None
        return Interval(lo, hi)

    def hull(self, other: "Interval") -> "Interval":
        return Interval(min(self.lo, other.lo), max(self.hi, other.hi))

    def subtract(self, other: "Interval") -> List["Interval"]:
        """The parts of this interval that are not in ``other``."""
        if not self.overlaps(other):
            return [self]
        parts = []
        if self.lo < other.lo:
            parts.append(Interval(self.lo, other.lo - 1))
        if other.hi < self.hi:
            parts.append(Interval(other.hi + 1, self.hi))
        return parts


class IntervalSet:
    """A set of integers stored as sorted, disjoint, non-adjacent intervals."""

    def __init__(self, intervals: Iterable[Interval] = ()) -> None:
        self._items: List[Interval] = []
        for iv in intervals:
            self.add(iv)

    def __iter__(self) -> Iterator[Interval]:
        return iter(self._items)

    def __len__(self) -> int:
        return len(self._items)

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, IntervalSet):
            return NotImplemented
        return self._items == other._items

    def __repr__(self) -> str:
        inner = ", ".join(f"[{iv.lo}, {iv.hi}]" for iv in self._items)
        return f"IntervalSet({inner})"

    def add(self, iv: Interval) -> None:
        """Insert an interval, merging everything it overlaps or touches."""
        merged = iv
        kept: List[Interval] = []
        for cur in self._items:
            if cur.overlaps(merged) or cur.adjacent(merged):
                merged = merged.hull(cur)
            else:
                kept.append(cur)
        kept.append(merged)
        kept.sort()
        self._items = kept

    def remove(self, iv: Interval) -> None:
        """Delete every integer of ``iv`` from the set."""
        out: List[Interval] = []
        for cur in self._items:
            out.extend(cur.subtract(iv))
        self._items = out

    def find(self, x: int) -> Optional[Interval]:
        """The interval containing ``x``, or None."""
        idx = bisect_right(self._items, Interval(x, x + (1 << 62))) - 1
        if idx >= 0 and self._items[idx].contains(x):
            return self._items[idx]
        return None

    def __contains__(self, x: int) -> bool:
        return self.find(x) is not None

    def total(self) -> int:
        """How many integers the set holds."""
        return sum(iv.length for iv in self._items)

    def span(self) -> Optional[Interval]:
        if not self._items:
            return None
        return Interval(self._items[0].lo, self._items[-1].hi)

    def gaps(self) -> List[Interval]:
        """The holes between consecutive intervals."""
        out = []
        for left, right in zip(self._items, self._items[1:]):
            out.append(Interval(left.hi + 1, right.lo - 1))
        return out

    def overlapping(self, iv: Interval) -> List[Interval]:
        return [cur for cur in self._items if cur.overlaps(iv)]

    def union(self, other: "IntervalSet") -> "IntervalSet":
        result = IntervalSet(self._items)
        for iv in other:
            result.add(iv)
        return result

    def intersection(self, other: "IntervalSet") -> "IntervalSet":
        result = IntervalSet()
        i = j = 0
        a, b = self._items, other._items
        while i < len(a) and j < len(b):
            common = a[i].intersect(b[j])
            if common is not None:
                result.add(common)
            if a[i].hi < b[j].hi:
                i += 1
            else:
                j += 1
        return result

    def difference(self, other: "IntervalSet") -> "IntervalSet":
        result = IntervalSet(self._items)
        for iv in other:
            result.remove(iv)
        return result

    def complement(self, within: Interval) -> "IntervalSet":
        """Everything in ``within`` that is not in this set."""
        result = IntervalSet([within])
        for iv in self._items:
            result.remove(iv)
        return result
