#!/usr/bin/env python3
"""Set installed files' modification times from their package's mtree in pacman's local database.

usage: fix-mtimes.py PKG...     (as root; for files whose content is right but whose mtime was
                                 stamped by `snapper undochange`)
Only files whose size matches the mtree get a new time, symlinks included (not followed).
"""
import glob, gzip, os, re, sys

n = skipped = 0
for pkg in sys.argv[1:]:
    d = [p for p in glob.glob(f"/var/lib/pacman/local/{pkg}-*") if os.path.basename(p).rsplit("-", 2)[0] == pkg]
    if not d:
        print(f"{pkg}: not installed", file=sys.stderr); continue
    defaults = {}
    for line in gzip.open(os.path.join(d[0], "mtree"), "rt"):
        f = line.split()
        if not f or f[0].startswith("#"): continue
        if f[0] == "/set":
            defaults.update(tuple(kv.split("=", 1)) for kv in f[1:] if "=" in kv); continue
        attrs = dict(defaults); attrs.update(tuple(kv.split("=", 1)) for kv in f[1:] if "=" in kv)
        # mtree escapes bytes outside printable ASCII, spaces included, as \NNN (octal)
        rel = re.sub(rb"\\([0-7]{3})", lambda m: bytes([int(m.group(1), 8)]), f[0].encode()).decode("utf-8", "surrogateescape")
        rel = rel.removeprefix("./")
        if "/" not in rel and rel.startswith("."):
            # package metadata; -Qkk checks .INSTALL and .CHANGELOG as the copies in the local db
            dbname = {".INSTALL": "install", ".CHANGELOG": "changelog"}.get(rel)
            if not dbname: continue
            path = os.path.join(d[0], dbname)
        else:
            path = "/" + rel
        if "time" not in attrs: continue
        try:
            st = os.lstat(path)
        except FileNotFoundError:
            skipped += 1; continue
        if attrs.get("type", "file") == "file" and "size" in attrs and int(attrs["size"]) != st.st_size:
            skipped += 1; continue
        sec, _, frac = attrs["time"].partition(".")
        ns = int(sec) * 10**9 + int((frac + "000000000")[:9])
        os.utime(path, ns=(st.st_atime_ns, ns), follow_symlinks=False)
        n += 1
print(f"set {n} times, skipped {skipped}")
