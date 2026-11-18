"""Generate the sales dataset, deterministically, as columnar arrays.

Run once as a preparation step; the result is part of the prepared snapshot.
"""

import argparse

import numpy as np

REGIONS = ["north", "south", "east", "west", "central", "coastal", "inland", "metro"]


def generate(rows: int, seed: int) -> dict:
    rng = np.random.default_rng(seed)
    return {
        "region": rng.integers(0, len(REGIONS), rows, dtype=np.int16),
        "product": rng.integers(0, 50, rows, dtype=np.int16),
        "units": rng.integers(1, 40, rows, dtype=np.int32),
        "price": np.round(rng.uniform(1.0, 500.0, rows), 2),
        "day": rng.integers(0, 365, rows, dtype=np.int16),
        "returned": rng.random(rows) < 0.05,
    }


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--rows", type=int, default=200_000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--out", default="data.npz")
    args = ap.parse_args()
    np.savez(args.out, **generate(args.rows, args.seed))
