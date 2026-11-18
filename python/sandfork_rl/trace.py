"""Chrome trace events for one RL step, viewable in Perfetto.

One track per rollout (queued for a sandbox, fork, generation per turn,
command per turn, checkpoint, verify), one for the trainer, one for weight
sync. The gaps where the generator sits idle waiting for environments are
visible at a glance.
"""

from __future__ import annotations

import json
import os
import threading
import time
from contextlib import contextmanager
from typing import Any, Dict, Iterator, List, Optional


class Trace:
    def __init__(self, name: str = "sandfork"):
        self.name = name
        self.t0 = time.monotonic()
        self.events: List[Dict[str, Any]] = []
        self._lock = threading.Lock()
        self._tids: Dict[str, int] = {}
        self.totals: Dict[str, float] = {}

    def _tid(self, track: str) -> int:
        with self._lock:
            if track not in self._tids:
                tid = len(self._tids) + 1
                self._tids[track] = tid
                self.events.append({"ph": "M", "name": "thread_name", "pid": 1, "tid": tid, "args": {"name": track}})
                self.events.append({"ph": "M", "name": "thread_sort_index", "pid": 1, "tid": tid, "args": {"sort_index": tid}})
            return self._tids[track]

    def now_us(self) -> float:
        return (time.monotonic() - self.t0) * 1e6

    def complete(self, track: str, name: str, start_us: float, dur_us: float, **args: Any) -> None:
        tid = self._tid(track)
        with self._lock:
            self.events.append({"ph": "X", "name": name, "cat": name.split(":")[0], "pid": 1, "tid": tid, "ts": start_us, "dur": max(dur_us, 0.0), "args": args})
            key = name.split(":")[0]
            self.totals[key] = self.totals.get(key, 0.0) + dur_us / 1e6

    @contextmanager
    def span(self, track: str, name: str, **args: Any) -> Iterator[Dict[str, Any]]:
        start = self.now_us()
        extra: Dict[str, Any] = {}
        try:
            yield extra
        finally:
            self.complete(track, name, start, self.now_us() - start, **{**args, **extra})

    def counter(self, name: str, **values: float) -> None:
        with self._lock:
            self.events.append({"ph": "C", "name": name, "pid": 1, "ts": self.now_us(), "args": values})

    def save(self, path: str, metadata: Optional[Dict[str, Any]] = None) -> str:
        os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
        with open(path, "w") as f:
            json.dump({"traceEvents": self.events, "displayTimeUnit": "ms", "otherData": metadata or {}}, f)
        return path

    def breakdown(self) -> Dict[str, float]:
        """Seconds per category, summed over tracks (concurrent spans add up)."""
        return dict(sorted(self.totals.items()))
