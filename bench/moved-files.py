#!/usr/bin/env python3
"""How much of a package's new version could come from the installed old version, by path vs by
content. For every package with two versions in a repository directory (older -> newer), using
`btrpkg dump` of both indexes:
  same path   new btrpart's data = old file at the same path and offset (what pull checks now)
  anywhere    new btrpart's uncompressed hash appears anywhere in the old version (old cache file
              lookup; what a path-independent installed-file lookup could find)
  moved       anywhere minus same path: data that moved to another path (renamed files)
Bytes are uncompressed (unencoded) btrpart bytes and stored bytes (the download).

  moved-files.py REPODIR... [--btrpkg PATH]
"""
import os, re, subprocess, sys
from collections import defaultdict

args = sys.argv[1:]
B = "btrpkg"
if "--btrpkg" in args:
    i = args.index("--btrpkg"); B = args[i + 1]; del args[i:i + 2]

def vercmp(a, b):
    return int(subprocess.run(["vercmp", a, b], capture_output=True, text=True).stdout.strip())

def dump(p):
    out = subprocess.run([B, "dump", p], capture_output=True, text=True, check=True).stdout
    chunks, files = [], []
    for l in out.splitlines():
        f = l.split()
        if f[0] == "c":
            chunks.append((int(f[1]), int(f[2]), int(f[3]), f[6]))  # image_off, unencoded, stored len, raw
        elif f[0] == "e" and f[1] == "0" and f[8] == "0" and int(f[6]) > 0:  # file (kind 0), not small
            files.append((int(f[7]), int(f[6]), bytes.fromhex(f[9]).decode(errors="replace")))
    # btrpart -> (path, offset in file), first file covering it (like store::chunk_files)
    import bisect
    offs = [c[0] for c in chunks]
    where = {}
    for off, size, path in files:
        k = bisect.bisect_left(offs, off)
        while k < len(chunks) and chunks[k][0] < off + size:
            io = chunks[k][0]
            if io not in where: where[io] = (path, io - off)
            k += 1
    return chunks, where

tot = defaultdict(int); pkgs = 0; top = []
for d in args:
    stems = [f[:-8] for f in os.listdir(d) if f.endswith(".pkg.btr")]
    by = defaultdict(list)
    for s in stems:
        m = re.match(r"(.+)-([^-]+-[^-]+)-([^-]+)$", s)
        if m: by[(m.group(1), m.group(3))].append((m.group(2), s))
    for (name, arch), vs in by.items():
        if len(vs) < 2: continue
        import functools
        vs.sort(key=functools.cmp_to_key(lambda a, b: vercmp(a[0], b[0])))
        (_, old), (_, new) = vs[-2], vs[-1]
        oc, ow = dump(os.path.join(d, old + ".pkg.btr"))
        nc, nw = dump(os.path.join(d, new + ".pkg.btr"))
        old_raw = {raw for (_, _, _, raw) in oc}
        old_at = {}  # (path, offset) -> raw
        for (io, ul, sl, raw) in oc:
            if io in ow: old_at[ow[io]] = raw
        p = defaultdict(int)
        for (io, ul, sl, raw) in nc:
            p["all"] += ul; p["all_s"] += sl
            same = io in nw and old_at.get(nw[io]) == raw
            if same: p["same"] += ul; p["same_s"] += sl
            if raw in old_raw: p["any"] += ul; p["any_s"] += sl
        for k, v in p.items(): tot[k] += v
        pkgs += 1
        top.append((p["any_s"] - p["same_s"], new, p["all_s"], p["same_s"], p["any_s"]))
mb = lambda x: f"{x / 1e6:,.1f} MB"
print(f"{pkgs} packages with an older version")
print(f"new versions' btrparts: {mb(tot['all'])} uncompressed, {mb(tot['all_s'])} to download")
print(f"  same path/offset in the old version: {mb(tot['same'])} ({mb(tot['same_s'])} stored)")
print(f"  same data anywhere in the old version: {mb(tot['any'])} ({mb(tot['any_s'])} stored)")
print(f"  moved (anywhere minus same path): {mb(tot['any'] - tot['same'])} ({mb(tot['any_s'] - tot['same_s'])} stored)")
print("most moved (stored bytes): new version, total, same path, anywhere")
for m, n, a, s, y in sorted(top, reverse=True)[:12]:
    print(f"  {mb(m):>10}  {n}  {mb(a)}  {mb(s)}  {mb(y)}")
