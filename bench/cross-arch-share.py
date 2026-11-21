#!/usr/bin/env python3
"""How much of one architecture's btrpkg store is already in another's.

  cross-arch-share.py STORE [OTHER_STORE...] --against BTRPARTS_DIR...

For every index in STORE/index, every btrpart counts as shared when the other btrparts
directories hold the same unencoded data (their raw map, .raw/xx/HASH: what repo-update's
publish reuses even when the compressed bytes differ), and as identical when they hold the same
stored bytes (xx/HASH).
Prints totals in encoded (download/storage) bytes, per package kind (-any vs per-arch filename),
and the packages with the most and least sharing. Small files (inside the index) are not chunks;
index bytes are counted apart.
"""
import os, subprocess, sys
from collections import defaultdict

args = sys.argv[1:]
cut = args.index("--against")
stores, against = args[:cut], args[cut + 1:]
BTR = os.environ.get("BTRPKG", "btrpkg")

def has(kind, h):
    return any(os.path.exists(os.path.join(s, kind, h[:2], h)) for s in against)
# kinds: ".raw" (raw map) and "" (the btrparts themselves)

tot = defaultdict(int)
per = []
for store in stores:
    idir = os.path.join(store, "index")
    for f in sorted(os.listdir(idir)):
        if not f.endswith(".btridx"):
            continue
        kind = "any" if f.endswith("-any.btridx") else "arch"
        enc = shared = ident = 0
        for line in subprocess.run([BTR, "dump", os.path.join(idir, f)], capture_output=True, text=True, check=True).stdout.splitlines():
            p = line.split()
            if p[0] != "c":
                continue
            n = int(p[3])
            enc += n
            if has(".raw", p[6]):
                shared += n
            if has("", p[5]):
                ident += n
        ib = os.path.getsize(os.path.join(idir, f))
        for k in (kind, "all"):
            tot[k, "enc"] += enc; tot[k, "shared"] += shared; tot[k, "ident"] += ident
            tot[k, "idx"] += ib; tot[k, "n"] += 1
        per.append((shared / enc if enc else 0, enc, shared, f))

mb = lambda b: f"{b / 1e6:,.0f} MB"
for k in ("any", "arch", "all"):
    e, s, i = tot[k, "enc"], tot[k, "shared"], tot[k, "ident"]
    if not tot[k, "n"]:
        continue
    print(f"{k:4}: {tot[k, 'n']} packages, chunks {mb(e)}: already in the other store {mb(s)} "
          f"({100 * s / e:.1f}%), byte-identical chunks {mb(i)} ({100 * i / e:.1f}%); indexes {mb(tot[k, 'idx'])}")
per.sort()
big = [x for x in per if x[1] > 5e6]
print("largest packages, least shared:")
for r, e, s, f in sorted(big, key=lambda x: -x[1])[:15]:
    print(f"  {f[:-7]:55} {mb(e):>9}  {100 * r:5.1f}%")
