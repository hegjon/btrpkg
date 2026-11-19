#!/usr/bin/env python3
"""Compare two installed trees: type, mode, owner, size, mtime, content, symlink target, xattrs, hardlinks."""
import hashlib, os, stat, sys

a, b = sys.argv[1], sys.argv[2]

def walk(top):
    out = {}
    for d, dirs, files in os.walk(top):
        for n in dirs + files:
            p = os.path.join(d, n)
            out[os.path.relpath(p, top)] = p
    return out

def digest(p):
    h = hashlib.sha256()
    with open(p, 'rb') as f:
        for blk in iter(lambda: f.read(1 << 20), b''):
            h.update(blk)
    return h.hexdigest()

def xattrs(p):
    try:
        return {k: os.getxattr(p, k, follow_symlinks=False) for k in os.listxattr(p, follow_symlinks=False)}
    except OSError:
        return {}

ta, tb = walk(a), walk(b)
problems = []
only_a = sorted(set(ta) - set(tb)); only_b = sorted(set(tb) - set(ta))
problems += [f"only in pacman tree: {p}" for p in only_a] + [f"only in btrpkg tree: {p}" for p in only_b]
counts = {"file": 0, "dir": 0, "link": 0, "bytes": 0, "xattr": 0}
for rel in sorted(set(ta) & set(tb)):
    sa, sb = os.lstat(ta[rel]), os.lstat(tb[rel])
    if stat.S_IFMT(sa.st_mode) != stat.S_IFMT(sb.st_mode):
        problems.append(f"type differs: {rel}"); continue
    for field in ("st_mode", "st_uid", "st_gid", "st_nlink"):
        if getattr(sa, field) != getattr(sb, field):
            problems.append(f"{field} differs: {rel} {getattr(sa, field)} vs {getattr(sb, field)}")
    xa, xb = xattrs(ta[rel]), xattrs(tb[rel])
    if xa != xb:
        problems.append(f"xattrs differ: {rel} {xa} vs {xb}")
    counts["xattr"] += bool(xa)
    if stat.S_ISDIR(sa.st_mode):
        counts["dir"] += 1
        continue
    if int(sa.st_mtime) != int(sb.st_mtime):
        problems.append(f"mtime differs: {rel}")
    if stat.S_ISLNK(sa.st_mode):
        counts["link"] += 1
        if os.readlink(ta[rel]) != os.readlink(tb[rel]):
            problems.append(f"symlink target differs: {rel}")
    elif stat.S_ISREG(sa.st_mode):
        counts["file"] += 1; counts["bytes"] += sa.st_size
        if sa.st_size != sb.st_size or digest(ta[rel]) != digest(tb[rel]):
            problems.append(f"content differs: {rel}")
print(f"compared: {counts}")
print(f"problems: {len(problems)}")
for p in problems[:50]:
    print("  " + p)
sys.exit(1 if problems else 0)
