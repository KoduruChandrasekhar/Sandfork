import pytest

from intervals import Interval, IntervalSet


def iset(*pairs):
    return IntervalSet(Interval(lo, hi) for lo, hi in pairs)


def pairs(s):
    return [(iv.lo, iv.hi) for iv in s]


def members(s, lo=-5, hi=40):
    return [x for x in range(lo, hi) if x in s]


# ---------------------------------------------------------------- Interval

def test_empty_interval_is_rejected():
    with pytest.raises(ValueError):
        Interval(3, 2)


def test_single_point_interval():
    iv = Interval(4, 4)
    assert iv.length == 1
    assert iv.contains(4)
    assert not iv.contains(3) and not iv.contains(5)


@pytest.mark.parametrize("lo,hi,length", [(0, 0, 1), (0, 9, 10), (-3, 3, 7), (5, 6, 2)])
def test_length(lo, hi, length):
    assert Interval(lo, hi).length == length


@pytest.mark.parametrize("x,inside", [(1, False), (2, True), (5, True), (8, True), (9, False)])
def test_contains_is_closed_on_both_ends(x, inside):
    assert Interval(2, 8).contains(x) is inside


@pytest.mark.parametrize(
    "a,b,expected",
    [
        ((0, 5), (5, 9), True),
        ((0, 5), (6, 9), False),
        ((0, 5), (2, 3), True),
        ((2, 3), (0, 5), True),
        ((6, 9), (0, 5), False),
        ((0, 0), (0, 0), True),
    ],
)
def test_overlaps(a, b, expected):
    assert Interval(*a).overlaps(Interval(*b)) is expected
    assert Interval(*b).overlaps(Interval(*a)) is expected


@pytest.mark.parametrize(
    "a,b,expected",
    [((0, 5), (6, 9), True), ((6, 9), (0, 5), True), ((0, 5), (7, 9), False), ((0, 5), (5, 9), False), ((0, 5), (2, 3), False)],
)
def test_adjacent(a, b, expected):
    assert Interval(*a).adjacent(Interval(*b)) is expected


@pytest.mark.parametrize(
    "a,b,expected",
    [((0, 5), (3, 9), (3, 5)), ((0, 5), (5, 9), (5, 5)), ((0, 5), (6, 9), None), ((0, 9), (2, 3), (2, 3)), ((2, 3), (0, 9), (2, 3))],
)
def test_intersect(a, b, expected):
    got = Interval(*a).intersect(Interval(*b))
    assert (None if got is None else (got.lo, got.hi)) == expected


def test_hull_spans_both():
    assert Interval(0, 2).hull(Interval(7, 9)) == Interval(0, 9)
    assert Interval(7, 9).hull(Interval(0, 2)) == Interval(0, 9)
    assert Interval(0, 9).hull(Interval(3, 4)) == Interval(0, 9)


@pytest.mark.parametrize("outer,inner,expected", [((0, 9), (2, 3), True), ((0, 9), (0, 9), True), ((0, 9), (5, 10), False), ((2, 3), (0, 9), False)])
def test_covers(outer, inner, expected):
    assert Interval(*outer).covers(Interval(*inner)) is expected


@pytest.mark.parametrize(
    "a,b,expected",
    [
        ((0, 9), (3, 5), [(0, 2), (6, 9)]),
        ((0, 9), (0, 5), [(6, 9)]),
        ((0, 9), (5, 9), [(0, 4)]),
        ((0, 9), (0, 9), []),
        ((0, 9), (-5, 20), []),
        ((0, 9), (10, 20), [(0, 9)]),
        ((0, 9), (9, 20), [(0, 8)]),
        ((0, 9), (-5, 0), [(1, 9)]),
        ((3, 3), (3, 3), []),
    ],
)
def test_subtract(a, b, expected):
    assert [(p.lo, p.hi) for p in Interval(*a).subtract(Interval(*b))] == expected


def test_intervals_order_by_lo_then_hi():
    assert sorted([Interval(5, 6), Interval(1, 9), Interval(1, 2)]) == [Interval(1, 2), Interval(1, 9), Interval(5, 6)]


# ---------------------------------------------------------------- IntervalSet.add

def test_empty_set():
    s = IntervalSet()
    assert len(s) == 0 and s.total() == 0 and s.span() is None and s.gaps() == []
    assert 0 not in s


def test_add_keeps_disjoint_intervals_sorted():
    assert pairs(iset((10, 12), (0, 2), (5, 6))) == [(0, 2), (5, 6), (10, 12)]


def test_add_merges_overlapping():
    assert pairs(iset((0, 5), (3, 9))) == [(0, 9)]


def test_add_merges_adjacent():
    assert pairs(iset((0, 5), (6, 9))) == [(0, 9)]
    assert pairs(iset((6, 9), (0, 5))) == [(0, 9)]


def test_add_does_not_merge_across_a_gap_of_one():
    assert pairs(iset((0, 5), (7, 9))) == [(0, 5), (7, 9)]


def test_add_bridges_several_intervals():
    s = iset((0, 1), (4, 5), (8, 9), (20, 21))
    s.add(Interval(2, 7))
    assert pairs(s) == [(0, 9), (20, 21)]


def test_add_contained_interval_changes_nothing():
    s = iset((0, 9))
    s.add(Interval(3, 4))
    assert pairs(s) == [(0, 9)]


def test_add_covering_interval_swallows_all():
    s = iset((2, 3), (5, 6), (8, 9))
    s.add(Interval(0, 20))
    assert pairs(s) == [(0, 20)]


def test_add_is_idempotent():
    s = iset((0, 3), (7, 9))
    before = pairs(s)
    s.add(Interval(0, 3))
    s.add(Interval(7, 9))
    assert pairs(s) == before


# ---------------------------------------------------------------- membership and lookup

@pytest.mark.parametrize("x,inside", [(-1, False), (0, True), (3, True), (4, False), (6, False), (7, True), (9, True), (10, False)])
def test_membership(x, inside):
    assert (x in iset((0, 3), (7, 9))) is inside


def test_find_returns_the_containing_interval():
    s = iset((0, 3), (7, 9), (20, 30))
    assert s.find(8) == Interval(7, 9)
    assert s.find(20) == Interval(20, 30)
    assert s.find(30) == Interval(20, 30)
    assert s.find(5) is None
    assert s.find(-1) is None
    assert s.find(31) is None


def test_membership_matches_brute_force():
    s = iset((1, 4), (6, 6), (10, 15), (17, 30))
    expected = [x for x in range(-5, 40) if 1 <= x <= 4 or x == 6 or 10 <= x <= 15 or 17 <= x <= 30]
    assert members(s) == expected


def test_total_counts_integers():
    assert iset((0, 3), (7, 9)).total() == 7
    assert iset((5, 5)).total() == 1


def test_span():
    assert iset((3, 4), (10, 12), (-2, 0)).span() == Interval(-2, 12)


def test_gaps():
    assert [(g.lo, g.hi) for g in iset((0, 3), (7, 9), (20, 30)).gaps()] == [(4, 6), (10, 19)]
    assert iset((0, 3)).gaps() == []


def test_gaps_of_single_width():
    assert [(g.lo, g.hi) for g in iset((0, 3), (5, 9)).gaps()] == [(4, 4)]


def test_overlapping_query():
    s = iset((0, 3), (7, 9), (20, 30))
    assert s.overlapping(Interval(3, 7)) == [Interval(0, 3), Interval(7, 9)]
    assert s.overlapping(Interval(4, 6)) == []
    assert s.overlapping(Interval(-10, 100)) == [Interval(0, 3), Interval(7, 9), Interval(20, 30)]
    assert s.overlapping(Interval(30, 40)) == [Interval(20, 30)]


# ---------------------------------------------------------------- remove

def test_remove_from_the_middle_splits():
    s = iset((0, 9))
    s.remove(Interval(3, 5))
    assert pairs(s) == [(0, 2), (6, 9)]


def test_remove_trims_the_edges():
    s = iset((0, 9), (20, 29))
    s.remove(Interval(8, 21))
    assert pairs(s) == [(0, 7), (22, 29)]


def test_remove_everything():
    s = iset((0, 9), (20, 29))
    s.remove(Interval(-5, 50))
    assert pairs(s) == [] and s.total() == 0


def test_remove_single_point():
    s = iset((0, 2))
    s.remove(Interval(1, 1))
    assert pairs(s) == [(0, 0), (2, 2)]


def test_remove_exact_interval():
    s = iset((0, 2), (5, 7))
    s.remove(Interval(5, 7))
    assert pairs(s) == [(0, 2)]


def test_remove_outside_is_a_no_op():
    s = iset((0, 2), (5, 7))
    s.remove(Interval(3, 4))
    s.remove(Interval(8, 100))
    assert pairs(s) == [(0, 2), (5, 7)]


def test_remove_then_add_restores():
    s = iset((0, 9))
    s.remove(Interval(4, 6))
    s.add(Interval(4, 6))
    assert pairs(s) == [(0, 9)]


# ---------------------------------------------------------------- set algebra

A = [(0, 5), (10, 15), (20, 25)]
B = [(3, 12), (14, 14), (24, 30), (40, 41)]


def brute(ps):
    return {x for lo, hi in ps for x in range(lo, hi + 1)}


def test_union_matches_brute_force():
    assert set(members(iset(*A).union(iset(*B)), -5, 60)) == brute(A) | brute(B)


def test_intersection_matches_brute_force():
    assert set(members(iset(*A).intersection(iset(*B)), -5, 60)) == brute(A) & brute(B)


def test_intersection_exact_shape():
    assert pairs(iset(*A).intersection(iset(*B))) == [(3, 5), (10, 12), (14, 14), (24, 25)]


def test_difference_matches_brute_force():
    assert set(members(iset(*A).difference(iset(*B)), -5, 60)) == brute(A) - brute(B)
    assert set(members(iset(*B).difference(iset(*A)), -5, 60)) == brute(B) - brute(A)


def test_set_operations_do_not_mutate_their_operands():
    a, b = iset(*A), iset(*B)
    a.union(b), a.intersection(b), a.difference(b)
    assert pairs(a) == A and pairs(b) == B


def test_intersection_with_empty_is_empty():
    assert pairs(iset(*A).intersection(IntervalSet())) == []
    assert pairs(IntervalSet().intersection(iset(*A))) == []


def test_intersection_is_commutative():
    assert iset(*A).intersection(iset(*B)) == iset(*B).intersection(iset(*A))


def test_complement_within_a_range():
    s = iset((2, 3), (7, 9))
    assert pairs(s.complement(Interval(0, 10))) == [(0, 1), (4, 6), (10, 10)]
    assert pairs(s.complement(Interval(2, 9))) == [(4, 6)]
    assert pairs(IntervalSet().complement(Interval(0, 3))) == [(0, 3)]


def test_complement_of_complement():
    s = iset((2, 3), (7, 9))
    w = Interval(0, 12)
    assert s.complement(w).complement(w) == s


def test_equality_and_repr():
    assert iset((0, 2), (3, 5)) == iset((0, 5))
    assert iset((0, 2)) != iset((0, 3))
    assert repr(iset((0, 2), (9, 9))) == "IntervalSet([0, 2], [9, 9])"
    assert iset((0, 1)).__eq__(42) is NotImplemented


def test_len_counts_intervals_not_integers():
    assert len(iset((0, 9), (20, 29), (40, 49))) == 3
