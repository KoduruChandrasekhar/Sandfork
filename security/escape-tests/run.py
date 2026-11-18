"""The isolation escape suite.

Each test tries one thing from inside a sandbox and must fail. Run against a
daemon at the level under test; CI runs it at L1 and L2.

    python security/escape-tests/run.py --task tasks/py-bugfix/sample-0001 --out escape.json

Exit status is non-zero if any test that must hold at this level escaped.
Results record, per test, whether isolation held, so the level table in
docs/security.md is generated from data.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "