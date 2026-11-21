#!/usr/bin/env python3
"""Medians from bench/proto-bench.sh logs: one table per log, rows = variant, columns = workload.

usage: proto-summary.py LOG...
"""
import statistics, sys
from collections import defaultdict

V = ["http-h1", "http-h2c", "https-h1", "https-h2", "https-h3"]
W = ["btr-reuse", "btr-full", "pkg"]
for path in sys.argv[1:]:
    rows = defaultdict(list)
    head = ""
    for l in open(path):
        if l.startswith("host "): head = l.strip()
        f = l.rstrip("\n").split("\t")
        if len(f) == 9 and f[0] in V and f[3] == "0":
            rows[(f[0], f[1])].append((float(f[4]), float(f[5]) + float(f[6]), int(f[7]), int(f[8])))
    if not rows: continue
    print(f"\n### {path}\n{head}\n")
    print("| variant | " + " | ".join(f"{w} s (cpu s)" for w in W) + " |")
    print("|---|" + "---:|" * len(W))
    for v in V:
        cells = []
        for w in W:
            r = rows.get((v, w))
            if not r: cells.append("-"); continue
            cells.append(f"{statistics.median(x[0] for x in r):.2f} ({statistics.median(x[1] for x in r):.2f})")
        print(f"| {v} | " + " | ".join(cells) + " |")
    for w in W:
        r = next((rows[(v, w)] for v in V if (v, w) in rows), None)
        if r: print(f"{w}: {r[0][2] / 1e6:.0f} MB in {r[0][3]} requests")
