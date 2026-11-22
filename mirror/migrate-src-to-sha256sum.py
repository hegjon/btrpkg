#!/usr/bin/env python3
"""One-off: NAME.pkg.btr.src (the SHA-256 of the package file an index was made from) becomes
PKGFILE.sha256sum beside the package file, in sha256sum's format ("HASH  PKGFILE").

  migrate-src-to-sha256sum.py ROOT

Every directory under ROOT with .src files is done under its .lock (the mirror's). A .src whose
package file is gone is removed (repo-update only looks at package files that are there).
"""
import fcntl, os, re, sys

root = sys.argv[1]
done = gone = 0
for d, subdirs, files in os.walk(root):
    subdirs[:] = [s for s in subdirs if s != "btrparts" and not s.startswith(".")]
    srcs = [f for f in files if f.endswith(".pkg.btr.src")]
    if not srcs:
        continue
    lock = open(os.path.join(d, ".lock"), "a")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)  # a running mirror holds it: fail
    for f in srcs:
        p = os.path.join(d, f)
        h = open(p).read().strip()
        stem = f[: -len(".pkg.btr.src")]
        pkg = next((f"{stem}.pkg.tar.{e}" for e in ("zst", "xz", "gz") if os.path.isfile(os.path.join(d, f"{stem}.pkg.tar.{e}"))), None)
        if pkg and re.fullmatch(r"[0-9a-f]{64}", h):
            out = os.path.join(d, pkg + ".sha256sum")
            with open(out + ".tmp", "w") as o:
                o.write(f"{h}  {pkg}\n")
            os.rename(out + ".tmp", out)
            done += 1
        else:
            gone += 1
        os.remove(p)
    lock.close()
print(f"{done} renamed to PKGFILE.sha256sum, {gone} removed (no package file)")
