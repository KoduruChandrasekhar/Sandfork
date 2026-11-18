"""A least-recently-used cache with a size budget and optional expiry."""

from __future__ import annotations

from typing import Callable, Dict, Generic, Hashable, Iterator, List, Optional, Tuple, TypeVar

K = TypeVar("K", bound=Hashable)
V = TypeVar("V")


class _Node:
    __slots__ = ("key", "value", "size", "expires", "prev", "next")

    def __init__(self, key, value, size: int, expires: Optional[float]) -> None:
        self.key = key
        self.value = value
        self.size = size
        self.expires = expires
        self.prev: Optional["_Node"] = None
        self.next: Optional["_Node"] = None


class LRUCache(Generic[K, V]):
    """Evicts the least recently used entries once the total size exceeds
    ``capacity``. Each entry has a size (1 unless given) and an optional
    time-to-live measured with ``clock``."""

    def __init__(self, capacity: int, clock: Callable[[], float] = lambda: 0.0) -> None:
        if capacity <= 0:
            raise ValueError("capacity must be positive")
        self.capacity = capacity
        self._clock = clock
        self._map: Dict[K, _Node] = {}
        self._head: Optional[_Node] = None  # most recently used
        self._tail: Optional[_Node] = None  # least recently used
        self._size = 0
        self.hits = 0
        self.misses = 0
        self.evictions = 0

    # ------------------------------------------------------------ list plumbing

    def _unlink(self, node: _Node) -> None:
        if node.prev is not None:
            node.prev.next = node.next
        else:
            self._head = node.next
        if node.next is not None:
            node.next.prev = node.prev
        else:
            self._tail = node.prev
        node.prev = node.next = None

    def _push_front(self, node: _Node) -> None:
        node.prev = None
        node.next = self._head
        if self._head is not None:
            self._head.prev = node
        self._head = node
        if self._tail is None:
            self._tail = node

    def _expired(self, node: _Node) -> bool:
        return node.expires is not None and self._clock() >= node.expires

    def _drop(self, node: _Node) -> None:
        self._unlink(node)
        del self._map[node.key]
        self._size -= node.size

    # ------------------------------------------------------------ public API

    def __len__(self) -> int:
        return len(self._map)

    @property
    def size(self) -> int:
        return self._size

    def __contains__(self, key: K) -> bool:
        node = self._map.get(key)
        return node is not None and not self._expired(node)

    def get(self, key: K, default: Optional[V] = None) -> Optional[V]:
        node = self._map.get(key)
        if node is None:
            self.misses += 1
            return default
        if self._expired(node):
            self._drop(node)
            self.misses += 1
            return default
        self._unlink(node)
        self._push_front(node)
        self.hits += 1
        return node.value

    def peek(self, key: K, default: Optional[V] = None) -> Optional[V]:
        """Read without changing recency or the hit counters."""
        node = self._map.get(key)
        if node is None or self._expired(node):
            return default
        return node.value

    def put(self, key: K, value: V, size: int = 1, ttl: Optional[float] = None) -> None:
        if size <= 0:
            raise ValueError("size must be positive")
        if size > self.capacity:
            raise ValueError("entry is larger than the cache")
        old = self._map.get(key)
        if old is not None:
            self._drop(old)
        expires = None if ttl is None else self._clock() + ttl
        node = _Node(key, value, size, expires)
        self._map[key] = node
        self._push_front(node)
        self._size += size
        self._evict()

    def _evict(self) -> None:
        while self._size > self.capacity and self._tail is not None:
            self._drop(self._tail)
            self.evictions += 1

    def pop(self, key: K, default: Optional[V] = None) -> Optional[V]:
        node = self._map.get(key)
        if node is None:
            return default
        self._drop(node)
        return default if self._expired(node) else node.value

    def clear(self) -> None:
        self._map.clear()
        self._head = self._tail = None
        self._size = 0

    def purge_expired(self) -> int:
        """Remove every expired entry. Returns how many were removed."""
        dead = [n for n in self._map.values() if self._expired(n)]
        for n in dead:
            self._drop(n)
        return len(dead)

    def resize(self, capacity: int) -> None:
        if capacity <= 0:
            raise ValueError("capacity must be positive")
        self.capacity = capacity
        self._evict()

    def keys(self) -> List[K]:
        """Keys from most to least recently used, skipping expired ones."""
        out = []
        node = self._head
        while node is not None:
            if not self._expired(node):
                out.append(node.key)
            node = node.next
        return out

    def items(self) -> List[Tuple[K, V]]:
        return [(k, self._map[k].value) for k in self.keys()]

    def __iter__(self) -> Iterator[K]:
        return iter(self.keys())

    def oldest(self) -> Optional[K]:
        node = self._tail
        while node is not None and self._expired(node):
            node = node.prev
        return None if node is None else node.key

    def hit_rate(self) -> float:
        total = self.hits + self.misses
        return 0.0 if total == 0 else self.hits / total

    def get_or_compute(self, key: K, compute: Callable[[K], V], size: int = 1) -> V:
        if key in self:
            return self.get(key)  # type: ignore[return-value]
        value = compute(key)
        self.put(key, value, size)
        self.misses += 1
        return value
