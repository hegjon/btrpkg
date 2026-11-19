#!/usr/bin/env python3
"""Upgrade download size if btrpkg reused data from the installed version.

usage: delta.py DIR OLD NEW [OLD NEW ...]   (package basenames without extension;
DIR holds NAME.pkg.tar.zst and NAME.btrpkg made by `btrpkg convert -l 19`)

Frames are deterministic (same converter, same level), so an unchanged chunk has
byte-identical frame bytes in both versions and can be matched by hash.
  file reuse  (OSTree-like): a file whose whole frame sequence exists in OLD is reflinked
  chunk reuse (casync-like on fixed 128 KiB boundaries): any frame present in OLD is reflinked
Both always download header + manifest (incl. small-file blob) + chunk table + digest.
Reference: zstd -19 --long=27 --patch-from over the uncompressed tars (a binary delta; the
client would have to rebuild and recompress, which is what btrpkg wants to avoid).
"""
import hashlib, os, struct, subprocess, sys
from compression import zstd

HDR, ENT = 256, 88   # format version 1 (src/format.rs)

def parse(path):
    b = open(path, "rb").read()
    assert b[:8] == b"BTRPKG\0\0" and struct.unpack_from("<I", b, 8)[0] == 1, path
    nchunks, image_len, e_len, e_raw, bl_len, bl_raw = struct.unpack_from("<QQQQQQ", b, 16)
    t0 = HDR
    e0 = t0 + nchunks * ENT
    meta = zstd.decompress(b[e0:e0 + e_len])
    chunks, pos = [], e0 + e_len + bl_len
    for i in range(nchunks):
        off, ulen, elen, comp = struct.unpack_from("<QIIB", b, t0 + i * ENT)
        stored = b[t0 + i * ENT + 24:t0 + i * ENT + 56]
        chunks.append((off, ulen, elen, stored))
        pos += elen
    assert pos == len(b), (path, pos, len(b))
    # entries -> large files (data_off, size)
    n, = struct.unpack_from("<I", meta, 0); p = 4; files = []
    for _ in range(n):
        kind, mode, uid, gid, mtime, size, data_off, small = struct.unpack_from("<BIIIqQQB", meta, p); p += 38
        for _ in range(2):
            l, = struct.unpack_from("<I", meta, p); p += 4 + l
        nx, = struct.unpack_from("<I", meta, p); p += 4
        for _ in range(2 * nx):
            l, = struct.unpack_from("<I", meta, p); p += 4 + l
        if kind == 0 and not small and size:
            files.append((data_off, size))
    by_off = {c[0]: c for c in chunks}
    fkeys = {}  # file key -> (encoded bytes, unencoded bytes); dedup'd files share data_off
    for off, size in set(files):
        seq, o, end = [], off, off + size
        while o < end:
            c = by_off[o]; seq.append(c); o += c[1]
        fkeys[hashlib.sha256(b"".join(c[3] for c in seq)).digest()] = (sum(c[2] for c in seq), size)
    fixed = len(b) - sum(c[2] for c in chunks)
    return dict(size=len(b), fixed=fixed, chunks={c[3]: c[2] for c in chunks},
                uchunks={c[3]: c[1] for c in chunks}, files=fkeys)

def patch_from(d, old, new):
    tmp = os.environ.get("TMPDIR", "/tmp")
    o, n = f"{tmp}/delta-old.tar", f"{tmp}/delta-new.tar"
    subprocess.run(f"zstd -qdcf {d}/{old}.pkg.tar.zst > {o}; zstd -qdcf {d}/{new}.pkg.tar.zst > {n}", shell=True, check=True)
    r = subprocess.run(["zstd", "-q", "-19", "-T0", "--long=27", "-c", f"--patch-from={o}", n], capture_output=True, check=True)
    os.remove(o); os.remove(n)
    return len(r.stdout)

def main():
    d, pairs = sys.argv[1], sys.argv[2:]
    print("old -> new\tpkg.tar.zst\t.pkg.btr\tfile_reuse\tchunk_reuse\tpatch_from\tfiles_same\tbytes_same")
    for old, new in zip(pairs[::2], pairs[1::2]):
        O, N = parse(f"{d}/{old}.btrpkg"), parse(f"{d}/{new}.btrpkg")
        tar = os.path.getsize(f"{d}/{new}.pkg.tar.zst")
        fr = N["fixed"] + sum(e for k, (e, u) in N["files"].items() if k not in O["files"])
        cr = N["fixed"] + sum(e for h, e in N["chunks"].items() if h not in O["chunks"])
        same_f = sum(1 for k in N["files"] if k in O["files"])
        same_b = sum(u for h, u in N["uchunks"].items() if h in O["chunks"]) / max(1, sum(N["uchunks"].values()))
        print(f"{old} -> {new}\t{tar}\t{N['size']}\t{fr}\t{cr}\t{patch_from(d, old, new)}\t{same_f}/{len(N['files'])}\t{same_b:.0%}")

main()
