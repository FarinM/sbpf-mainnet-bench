#!/usr/bin/env python3
"""Compare base and patched runner results.

Reads results/base-*.jsonl and results/patch-*.jsonl, reports the median time
per fixture for each side, and fails when the two sides disagree on compute
units or execution result (which would mean the patch changed behavior).
"""

import glob
import json
import statistics
import sys

DIRECTORY = sys.argv[1] if len(sys.argv) > 1 else "results"


def load(prefix):
    runs = {}
    for path in sorted(glob.glob(f"{DIRECTORY}/{prefix}-*.jsonl")):
        for line in open(path):
            line = line.strip()
            if not line:
                continue
            data = json.loads(line)
            runs.setdefault(data["fixture"], []).append(data)
    return runs


base = load("base")
patch = load("patch")
problems = []
rows = []

for name in sorted(base):
    base_runs = base[name]
    patch_runs = patch.get(name, [])
    if not patch_runs:
        problems.append(f"{name}: missing from patched results")
        continue

    base_ns = statistics.median(run["ns_median"] for run in base_runs)
    patch_ns = statistics.median(run["ns_median"] for run in patch_runs)
    base_cu = {run["cu"] for run in base_runs}
    patch_cu = {run["cu"] for run in patch_runs}
    base_result = {run["result"] for run in base_runs}
    patch_result = {run["result"] for run in patch_runs}

    if base_result != {"ok"}:
        problems.append(f"{name}: base result {base_result}")
    if patch_result != base_result:
        problems.append(f"{name}: result differs {base_result} vs {patch_result}")
    if base_cu != patch_cu:
        problems.append(f"{name}: compute units differ {base_cu} vs {patch_cu}")
    if len(base_cu) > 1:
        problems.append(f"{name}: base compute units unstable {base_cu}")

    rows.append((name, base_ns, patch_ns, patch_ns / base_ns, min(base_cu)))

width = max((len(row[0]) for row in rows), default=7)
print(f"{'fixture':{width}s}  {'base':>10s}  {'patched':>10s}  {'patched/base':>12s}  {'cu':>9s}")
for name, base_ns, patch_ns, ratio, cu in rows:
    print(f"{name:{width}s}  {base_ns / 1000:10.2f}u  {patch_ns / 1000:10.2f}u  {ratio:11.3f}  {cu:9d}")

if problems:
    print("\nPROBLEMS")
    for problem in problems:
        print(f"  - {problem}")
    sys.exit(1)

print("\nall fixtures produce identical compute units and results")
