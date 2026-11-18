"""Build the kernel's namespace: the dataset, parsed and ready.

This is the expensive part a zygote pays once. SANDFORK_DATA_LAYOUT=objects
loads the same data as a list of dicts instead of columns; it exists to
measure how much faster CPython un-shares object-heavy data.
"""

import os

import numpy as np
import pandas as pd

REGIONS = ["north", "south", "east", "west", "central", "coastal", "inland", "metro"]


def load() -> dict:
    here = os.path.dirname(os.path.abspath(__file__))
    raw = np.load(os.path.join(here, "data.npz"))
    df = pd.DataFrame({name: raw[name] for name in raw.files})
    df["region"] = pd.Categorical.from_codes(df["region"], REGIONS)
    df["revenue"] = df["units"] * df["price"]
    namespace = {"df": df, "pd": pd, "np": np, "REGIONS": REGIONS}
    if os.environ.get("SANDFORK_DATA_LAYOUT") == "objects":
        namespace["rows"] = df.to_dict("records")
    return namespace
