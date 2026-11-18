"""E5 (simulated): difficulty-band scheduling against uniform sampling.

A simulation of the algorithm in crates/sandforkd/src/scheduler.rs (decayed
Beta posteriors, Thompson sampling of 1 - p^G - (1-p)^G) against tasks with
known pass rates, with a policy that slowly improves. It measures the
fraction of useful groups (groups with reward variance) and the
sandbox-seconds spent per useful group. A real-model E5 needs the GPU run.

    python bench/e5_sim.py --seeds 3
"""

import argparse
import json
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from run import save  # noqa: E402

DECAY = 0.95


def run(strategy: str, seed: int, n_tasks=200, steps=150, P=8, G=8, sandbox_s=20.0):
    rng = random.Random(seed)
    # Pass rates skewed toward hard tasks, as in the plan's model choice band.
    base = [min(0.99, max(0.0, rng.betavariate(0.6, 1.4))) for _ in range(n_tasks)]
    post = [[1.0, 1.0] for _ in range(n_tasks)]
    useful_curve, useful_total, groups = [], 0, 0
    for step in range(steps):
        drift = 0.25 * step / steps  # the policy improves
        for p in post:
            p[0], p[1] = max(0.5, p[0] * DECAY), max(0.5, p[1] * DECAY)
        if strategy == "uniform":
            picks = rng.sample(range(n_tasks), P)
        else:
            scored = []
            for i, (a, b) in enumerate(post):
                q = rng.betavariate(a, b)
                scored.append((1 - q**G - (1 - q) ** G, i))
            picks = [i for _, i in sorted(scored, reverse=True)[:P]]
        useful = 0
        for i in picks:
            p = min(0.99, base[i] + drift * (1 - base[i]))
            passes = sum(rng.random() < p for _ in range(G))
            post[i][0] += passes
            post[i][1] += G - passes
            useful += 0 < passes < G
        useful_total += useful
        groups += P
        useful_curve.append(useful / P)
    frac = useful_total / groups
    return {"strategy": strategy, "seed": seed, "useful_fraction": frac, "sandbox_seconds_per_useful_group": (G * sandbox_s * groups) / max(useful_total, 1), "useful_curve": useful_curve}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seeds", type=int, default=3)
    a = ap.parse_args()
    rows = [run(s, seed) for s in ("band", "uniform") for seed in range(1, a.seeds + 1)]
    for r in rows:
        print(json.dumps({k: v for k, v in r.items() if k != "useful_curve"}))
    save("e5", {"rows": rows, "simulated": True, "note": "simulation of scheduler.rs against known pass rates; 20 sandbox-seconds per rollout assumed"})


if __name__ == "__main__":
    main()
