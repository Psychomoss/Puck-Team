#!/usr/bin/env python3
"""canon.py OUT DEST: self-checks an output folder, writes sorted canonical CSVs to DEST, prints their SHA-256."""
import hashlib
from pathlib import Path
import sys

import polars as pl

sys.path.insert(0, str(Path(__file__).parent))
from run_fixtures import SCHEMAS, load_actual

out, dest = map(Path, sys.argv[1:])
dest.mkdir(parents=True, exist_ok=True)
t = {name: load_actual(out / name, schema) for name, schema in SCHEMAS.items()}
u, c = t["user_daily"], t["campaign_daily"]
ok = True
for col in ("sessions", "revenue_cents"):
    if u[col].sum() != c[col].sum():
        ok = False
        print(f"SELF-CHECK FAIL: {col} sums differ {u[col].sum()} vs {c[col].sum()}")
credited = c.filter(pl.col("campaign_id") != "organic")["sessions"].sum()
if c["assisted_sessions"].sum() < credited:
    ok = False
    print(f"SELF-CHECK FAIL: assisted {c['assisted_sessions'].sum()} < credited {credited}")
for name, df in t.items():
    keys = [k for k in ("user_id", "campaign_id", "day") if k in df.columns]
    if df.height != df.n_unique(subset=keys):
        ok = False
        print(f"SELF-CHECK FAIL: duplicate keys in {name}")
    path = dest / f"{name}.csv"
    df.sort(list(df.columns)).write_csv(path)
    print(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {name}  rows={df.height}")
sys.exit(0 if ok else 1)
