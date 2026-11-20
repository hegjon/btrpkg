#!/usr/bin/env python3
"""Median tables from bench/lab-upgrade.sh and bench/lab-fresh.sh logs.

usage: lab-summary.py RESULTS_DIR [PREFIX...]   (default prefixes: upgrade fresh upgrade2 fresh2)
Reads PREFIX-HOST.log files; prints one markdown table per prefix.
"""
import glob, os, statistics, sys

def rows(path):
    out = {}
    for l in open(path):
        f = l.rstrip("\n").split("\t")
        if f[0] in ("pacman", "btrpkg") and len(f) >= 7:
            out.setdefault(f[0], []).append(f)
    return out

def med(vals): return statistics.median(vals)

def mb(x): return f"{x / 1e6:,.0f} MB"

d = sys.argv[1]
prefixes = sys.argv[2:] or ["upgrade", "fresh", "upgrade2", "fresh2"]
order = ["p14s", "t460s", "t490", "x200"]
for pre in prefixes:
    files = {os.path.basename(p)[len(pre) + 1:-4]: p for p in glob.glob(f"{d}/{pre}-*.log")}
    if not files: continue
    up = pre.startswith("upgrade")
    print(f"\n### {pre}\n")
    if up:
        print("| host | pacman s | btrpkg s | downloaded pacman / btrpkg | written pacman / btrpkg | space pacman / btrpkg | -Qkk |")
    else:
        print("| host | pacman s | btrpkg s | written pacman / btrpkg | cache+root pacman / btrpkg | root pacman / btrpkg |")
    print("|---|---:|---:|---:|---:|---:|---:|" if up else "|---|---:|---:|---:|---:|---:|")
    for h in sorted(files, key=lambda h: order.index(h) if h in order else 99):
        r = rows(files[h])
        if not r.get("pacman") or not r.get("btrpkg"): continue
        P, B = r["pacman"], r["btrpkg"]
        g = lambda rs, i: med([float(x[i]) for x in rs])
        n = f" (n={len(P)}/{len(B)})"
        if up:
            bad = sum(int(x[7]) for x in P + B)
            fails = sum(int(x[2]) != 0 for x in P + B)
            print(f"| {h}{n} | {g(P,3):.2f} | {g(B,3):.2f} | {mb(g(P,4))} / {mb(g(B,4))} | {mb(g(P,5))} / {mb(g(B,5))} | {mb(g(P,6))} / {mb(g(B,6))} | {'clean' if bad == 0 and fails == 0 else f'{bad} altered, {fails} failed'} |")
        else:
            print(f"| {h}{n} | {g(P,3):.2f} | {g(B,3):.2f} | {mb(g(P,4))} / {mb(g(B,4))} | {mb(g(P,5))} / {mb(g(B,5))} | {mb(g(P,6))} / {mb(g(B,6))} |")
