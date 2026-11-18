from intervals import Interval, IntervalSet


def iset(*pairs):
    return IntervalSet(Interval(lo, hi) for lo, hi in pairs)


def pairs(s):
    return [(iv.lo, iv.hi) for iv in s]


def test_add_merges_adjacent():
    assert pairs(iset((0, 5), (6, 9))) == [(0, 9)]
    assert pairs(iset((6, 9), (0, 5))) == [(0, 9)]
