#!/usr/bin/env python3
"""Compare two pacstrapped roots: compare-roots.py A B

Package-owned files (every path in A's local database) must be identical in type, mode, owner,
size, symlink target, xattrs, content (sha256) and mtime; for backup files (%BACKUP%, which
scriptlets and hooks edit after install: passwd, shells...) everything but the mtime. Other files (made by hooks:
ld.so.cache, font and icon caches, man-db, sysusers' passwd/shadow...) are compared without
mtime and listed. The local databases must match apart from %INSTALLDATE% and %VALIDATION%.
Exit 1 if a package-owned file or a database entry differs."""
import hashlib, os, stat, sys

SKIP = ("proc", "sys", "dev", "run", "tmp", "var/cache/pacman/pkg", "var/log", "var/lib/pacman/sync",
        "var/lib/pacman/local", "var/tmp", "etc/pacman.d/gnupg")   # the local database is compared on its own
FIELDS = ("type", "mode", "uid", "gid", "size/target", "sha256", "mtime", "xattrs")


def walk(root):
    out = {}
    for d, dirs, files in os.walk(root):
        rel = os.path.relpath(d, root)
        rel = "" if rel == "." else rel
        dirs[:] = [x for x in dirs if os.path.join(rel, x) not in SKIP]
        for n in dirs + files:
            out[os.path.join(rel, n)] = os.path.join(d, n)
    return out


def info(p, with_mtime):
    st = os.lstat(p)
    t = stat.S_IFMT(st.st_mode)
    r: list = [t, stat.S_IMODE(st.st_mode), st.st_uid, st.st_gid]
    if stat.S_ISLNK(st.st_mode):
        r.append(os.readlink(p))
    elif stat.S_ISREG(st.st_mode):
        h = hashlib.sha256()
        with open(p, "rb") as f:
            for b in iter(lambda: f.read(1 << 20), b""):
                h.update(b)
        r += [st.st_size, h.hexdigest()]
        if with_mtime:
            r.append(int(st.st_mtime))
    try:
        r.append(sorted((k, os.getxattr(p, k, follow_symlinks=False)) for k in os.listxattr(p, follow_symlinks=False)))
    except OSError:
        pass
    return r


def db(root):
    base = os.path.join(root, "var/lib/pacman/local")
    out = {}
    for e in sorted(os.listdir(base)):
        d = os.path.join(base, e)
        if not os.path.isdir(d):
            continue
        for f in sorted(os.listdir(d)):
            data = open(os.path.join(d, f), "rb").read()
            if f == "desc":
                lines, skip = [], False
                for l in data.split(b"\n"):
                    if l in (b"%INSTALLDATE%", b"%VALIDATION%"):
                        skip = True
                        continue
                    if skip:
                        skip = l != b""
                        continue
                    lines.append(l)
                data = b"\n".join(lines)
            out[f"{e}/{f}"] = data
    return out


def pkgfiles(root):
    s, backup = set(), set()
    base = os.path.join(root, "var/lib/pacman/local")
    for e in os.listdir(base):
        fp = os.path.join(base, e, "files")
        if not os.path.exists(fp):
            continue
        sec = None
        for l in open(fp, errors="surrogateescape").read().split("\n"):   # %FILES% and %BACKUP%
            if l.startswith("%"):
                sec = l
            elif l and sec == "%FILES%":
                s.add(l.rstrip("/"))
            elif l and sec == "%BACKUP%":
                backup.add(l.split("\t")[0])
    return s, backup


a, b = sys.argv[1:3]
owned, backup = pkgfiles(a)
ta, tb = walk(a), walk(b)
bad, gen = [], []
for rel in sorted(set(ta) | set(tb)):
    own = rel in owned
    if rel not in ta or rel not in tb:
        (bad if own else gen).append((rel, "only in " + (a if rel in ta else b)))
        continue
    ia, ib = info(ta[rel], own and rel not in backup), info(tb[rel], own and rel not in backup)
    if ia != ib:
        what = [FIELDS[i] if i < len(FIELDS) else str(i) for i in range(max(len(ia), len(ib)))
                if i >= len(ia) or i >= len(ib) or ia[i] != ib[i]]
        (bad if own else gen).append((rel, "differs (" + ",".join(what) + ")"))
da, dbb = db(a), db(b)
dbdiff = sorted(k for k in set(da) | set(dbb) if da.get(k) != dbb.get(k))
nown = sum(1 for r in ta if r in owned)
print(f"entries: {len(ta)} / {len(tb)}; package-owned: {nown}; local db entries: {len(da)}")
print(f"package-owned differences: {len(bad)}; database differences: {len(dbdiff)}; other (hook-made) differences: {len(gen)}")
for r, why in bad[:30]:
    print("  OWNED", why, r)
for k in dbdiff[:20]:
    print("  DB", k)
for r, why in gen[:40]:
    print("  other", why, r)
sys.exit(1 if bad or dbdiff else 0)
