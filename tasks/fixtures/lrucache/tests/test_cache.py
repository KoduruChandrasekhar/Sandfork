import pytest

from lrucache import LRUCache


class Clock:
    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t


def filled(capacity=3, n=3):
    c = LRUCache(capacity)
    for i in range(n):
        c.put(f"k{i}", i)
    return c


# ---------------------------------------------------------------- construction

@pytest.mark.parametrize("cap", [0, -1])
def test_capacity_must_be_positive(cap):
    with pytest.raises(ValueError):
        LRUCache(cap)


def test_empty_cache():
    c = LRUCache(2)
    assert len(c) == 0 and c.size == 0 and c.keys() == [] and c.oldest() is None
    assert c.get("x") is None and c.get("x", 5) == 5


# ---------------------------------------------------------------- get and put

def test_put_then_get():
    c = LRUCache(2)
    c.put("a", 1)
    assert c.get("a") == 1 and "a" in c and len(c) == 1


def test_overwrite_replaces_value_and_keeps_one_entry():
    c = LRUCache(2)
    c.put("a", 1)
    c.put("a", 2)
    assert c.get("a") == 2 and len(c) == 1 and c.size == 1


def test_least_recently_used_is_evicted():
    c = filled()
    c.put("k3", 3)
    assert "k0" not in c and c.keys() == ["k3", "k2", "k1"]


def test_get_refreshes_recency():
    c = filled()
    c.get("k0")
    c.put("k3", 3)
    assert "k0" in c and "k1" not in c


def test_peek_does_not_refresh_recency():
    c = filled()
    assert c.peek("k0") == 0
    c.put("k3", 3)
    assert "k0" not in c


def test_peek_does_not_count_hits():
    c = filled()
    c.peek("k0")
    c.peek("missing")
    assert c.hits == 0 and c.misses == 0


def test_overwrite_refreshes_recency():
    c = filled()
    c.put("k0", 10)
    c.put("k3", 3)
    assert c.keys() == ["k3", "k0", "k2"]


def test_keys_are_most_recent_first():
    c = filled(capacity=5, n=4)
    c.get("k1")
    assert c.keys() == ["k1", "k3", "k2", "k0"]


def test_items_follow_keys():
    c = filled()
    assert c.items() == [("k2", 2), ("k1", 1), ("k0", 0)]


def test_iteration_matches_keys():
    c = filled()
    assert list(c) == c.keys()


def test_oldest():
    c = filled()
    assert c.oldest() == "k0"
    c.get("k0")
    assert c.oldest() == "k1"


@pytest.mark.parametrize("n", [1, 2, 5, 10])
def test_never_more_than_capacity(n):
    c = LRUCache(3)
    for i in range(n):
        c.put(i, i)
    assert len(c) == min(n, 3)
    assert c.evictions == max(0, n - 3)


def test_capacity_one():
    c = LRUCache(1)
    c.put("a", 1)
    c.put("b", 2)
    assert c.keys() == ["b"] and c.evictions == 1


# ---------------------------------------------------------------- sizes

def test_sized_entries_count_against_capacity():
    c = LRUCache(10)
    c.put("a", 1, size=4)
    c.put("b", 2, size=4)
    assert c.size == 8
    c.put("c", 3, size=4)
    assert c.keys() == ["c", "b"] and c.size == 8


def test_a_large_entry_can_evict_several():
    c = LRUCache(10)
    for k in "abcde":
        c.put(k, k, size=2)
    c.put("big", 0, size=7)
    assert c.keys() == ["big", "e"] and c.size == 9


def test_an_entry_exactly_the_capacity_fits():
    c = LRUCache(5)
    c.put("a", 1, size=5)
    assert c.keys() == ["a"] and c.size == 5


def test_an_entry_larger_than_the_cache_is_refused():
    c = LRUCache(5)
    with pytest.raises(ValueError):
        c.put("a", 1, size=6)


@pytest.mark.parametrize("size", [0, -3])
def test_size_must_be_positive(size):
    with pytest.raises(ValueError):
        LRUCache(5).put("a", 1, size=size)


def test_overwrite_with_a_new_size():
    c = LRUCache(10)
    c.put("a", 1, size=3)
    c.put("a", 1, size=8)
    assert c.size == 8 and len(c) == 1


def test_resize_down_evicts():
    c = filled(capacity=5, n=5)
    c.resize(2)
    assert c.keys() == ["k4", "k3"] and c.capacity == 2


def test_resize_up_keeps_everything():
    c = filled()
    c.resize(10)
    c.put("k3", 3)
    assert len(c) == 4


def test_resize_must_be_positive():
    with pytest.raises(ValueError):
        filled().resize(0)


# ---------------------------------------------------------------- pop and clear

def test_pop_removes_and_returns():
    c = filled()
    assert c.pop("k1") == 1 and "k1" not in c and len(c) == 2 and c.size == 2


def test_pop_missing_returns_default():
    assert filled().pop("nope", "d") == "d"


def test_pop_head_and_tail_keep_the_list_consistent():
    c = filled()
    c.pop("k2")
    c.pop("k0")
    assert c.keys() == ["k1"] and c.oldest() == "k1"
    c.put("x", 9)
    assert c.keys() == ["x", "k1"]


def test_clear():
    c = filled()
    c.clear()
    assert len(c) == 0 and c.size == 0 and c.keys() == []
    c.put("a", 1)
    assert c.keys() == ["a"]


# ---------------------------------------------------------------- expiry

def test_entries_expire():
    clock = Clock()
    c = LRUCache(5, clock)
    c.put("a", 1, ttl=10)
    clock.t = 9.9
    assert c.get("a") == 1
    clock.t = 10.0
    assert c.get("a") is None and "a" not in c


def test_an_expired_get_is_a_miss_and_removes_the_entry():
    clock = Clock()
    c = LRUCache(5, clock)
    c.put("a", 1, ttl=1)
    clock.t = 2
    c.get("a")
    assert c.misses == 1 and len(c) == 0


def test_entries_without_ttl_never_expire():
    clock = Clock()
    c = LRUCache(5, clock)
    c.put("a", 1)
    clock.t = 1e9
    assert c.get("a") == 1


def test_expired_keys_are_skipped():
    clock = Clock()
    c = LRUCache(5, clock)
    c.put("a", 1, ttl=1)
    c.put("b", 2)
    clock.t = 5
    assert c.keys() == ["b"] and c.oldest() == "b"


def test_purge_expired():
    clock = Clock()
    c = LRUCache(5, clock)
    c.put("a", 1, ttl=1)
    c.put("b", 2, ttl=3)
    c.put("c", 3)
    clock.t = 2
    assert c.purge_expired() == 1 and len(c) == 2 and c.size == 2


def test_peek_of_an_expired_entry():
    clock = Clock()
    c = LRUCache(5, clock)
    c.put("a", 1, ttl=1)
    clock.t = 1
    assert c.peek("a", "gone") == "gone"


def test_pop_of_an_expired_entry_returns_default():
    clock = Clock()
    c = LRUCache(5, clock)
    c.put("a", 1, ttl=1)
    clock.t = 3
    assert c.pop("a", "d") == "d" and len(c) == 0


def test_overwrite_resets_ttl():
    clock = Clock()
    c = LRUCache(5, clock)
    c.put("a", 1, ttl=2)
    clock.t = 1.5
    c.put("a", 2, ttl=2)
    clock.t = 3
    assert c.get("a") == 2


# ---------------------------------------------------------------- counters

def test_hits_misses_and_rate():
    c = filled()
    c.get("k0")
    c.get("k1")
    c.get("zzz")
    assert (c.hits, c.misses) == (2, 1)
    assert c.hit_rate() == pytest.approx(2 / 3)


def test_hit_rate_of_an_unused_cache():
    assert LRUCache(1).hit_rate() == 0.0


def test_get_or_compute_computes_once():
    calls = []
    c = LRUCache(3)

    def compute(k):
        calls.append(k)
        return k * 2

    assert c.get_or_compute(4, compute) == 8
    assert c.get_or_compute(4, compute) == 8
    assert calls == [4] and c.hits == 1 and c.misses == 1


def test_get_or_compute_respects_capacity():
    c = LRUCache(2)
    for i in range(4):
        c.get_or_compute(i, lambda k: k)
    assert c.keys() == [3, 2]


def test_eviction_counter():
    c = LRUCache(2)
    for i in range(5):
        c.put(i, i)
    assert c.evictions == 3
