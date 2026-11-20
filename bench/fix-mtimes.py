#!/usr/bin/env python3
"""Set installed files' modification times from their package's mtree in pacman's local database.

usage: fix-mtimes.py PKG...     (as root; for files whose content is right but whose mtime was
                                 stamped by `snapper undochange`)
Only files whose size matches the mtree get a new time, symlinks included (not followed).
"""
import glob, gzip, os, sys

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
        path = "/" + f[0].removeprefix("./")
        if os.path.basename(path).startswith(".") and path.count("/") == 1: continue  # .PKGINFO etc.
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
