#!/usr/bin/env python3
"""Rewrite a pacman sync database (tar, any compression tarfile reads) as an uncompressed tar
whose headers no longer depend on who ran repo-add: owner and group root/0, everything else
(names, order, modes, mtimes, contents) unchanged. repo-add re-creates the whole tar on every
update with the owner of whoever ran it, so without this every zchunk chunk differs between two
versions of the database.

normalize.py IN OUT           write the normalized tar
normalize.py --entries IN DIR  write each regular file's data to DIR/N (dictionary training)
"""
import io
import os
import sys
import tarfile


def normalize(src, dst):
    tmp = dst + ".tmp"
    with tarfile.open(src) as t, tarfile.open(tmp, "w", format=t.format) as o:
        for m in t:
            f = t.extractfile(m) if m.isfile() else None
            data = f.read() if f else None
            m.uid = m.gid = 0
            m.uname = m.gname = "root"
            o.addfile(m, io.BytesIO(data) if data is not None else None)
    os.replace(tmp, dst)


def entries(src, out):
    os.makedirs(out, exist_ok=True)
    n = 0
    with tarfile.open(src) as t:
        for m in t:
            if m.isfile():
                f = t.extractfile(m)
                with open(os.path.join(out, str(n)), "wb") as o:
                    o.write(f.read() if f else b"")
                n += 1
    print(n)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--entries":
        entries(sys.argv[2], sys.argv[3])
    elif len(sys.argv) == 3:
        normalize(sys.argv[1], sys.argv[2])
    else:
        sys.exit(__doc__)
