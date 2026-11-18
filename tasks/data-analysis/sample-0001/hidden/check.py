"""Grader: compare the submitted number with the expected one."""

import json
import math
import os

here = os.path.dirname(os.path.abspath(__file__))
expected = json.load(open(os.path.join(here, "expected.json")))
try:
    got = float(open("answer.txt").read().strip())
    ok = math.isclose(got, expected["answer"], rel_tol=expected.get("rel_tol", 1e-6), abs_tol=expected.get("abs_tol", 0.01))
except (OSError, ValueError):
    ok = False
print(json.dumps({"passed": int(ok), "total": 1}))
