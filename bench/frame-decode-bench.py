#!/usr/bin/python3
"""Userspace decode speed of a transport's btrparts (one core, Python 3.14's compression.zstd,
i.e. libzstd).

    frame-decode-bench.py DUMPDIR ROUNDS TRANSPORT...

DUMPDIR holds `btrpkg dump` output per transport (NAME.dump). Reads every zstd btrpart into
memory, then decodes them all ROUNDS times and prints the best round: unencoded bytes, seconds,
MB/s. Raw btrparts are skipped (nothing to decode).
"""
import os, sys, time
from compression import zstd

dumpdir, rounds, files = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
frames, total = [], 0
for f in files:
    lines = open(os.path.join(dumpdir, os.path.basename(f) + ".dump")).read().splitlines()
    il = int(lines[1].split()[8])  # header ... index N base M
    data = open(f, "rb").read()
    off = il
    for l in lines:
        if not l.startswith("c "): continue
        _, _, ulen, elen, comp, _, _ = l.split()
        ulen, elen = int(ulen), int(elen)
        if comp == "2":
            frames.append(data[off:off + elen]); total += ulen
        off += elen
best = None
for _ in range(rounds):
    t = time.perf_counter()
    for fr in frames: zstd.decompress(fr)
    dt = time.perf_counter() - t
    best = dt if best is None or dt < best else best
print(f"{len(frames)} frames, {total/1e6:.1f} MB unencoded, best of {rounds}: {best:.3f} s, {total/best/1e6:.0f} MB/s")
