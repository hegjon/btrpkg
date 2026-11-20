#!/usr/bin/env python3
"""Would SQLite work as the .btridx format? Size and read time against format version 1.

usage: sqlite_index.py DIR OUTDIR    (DIR holds version-1 .btrpkg files; writes OUTDIR/<name>.{a,b}.sqlite)

Variants per package:
  v1  the current index: header | chunk table | entries (zstd -19) | small-file blob (zstd -19)
  A   SQLite, the natural schema: tables chunks, entries, small files as one BLOB row each
  B   SQLite, chunks + entries tables, small-file blob as one zstd -19 BLOB (solid compression kept)
Transport size for A and B = the .sqlite file through zstd -19 (a database can't be fetched uncompressed
without losing the download).
Read = open + read every entry and chunk row (A, B) or check hashes + zstd-decode + parse (v1, in Python).
"""
import hashlib, os, sqlite3, struct, sys, time
from compression import zstd

HDR, ENT = 256, 88

def parse_index(b):
    assert b[:8] == b"BTRPKG\0\0" and struct.unpack_from("<I", b, 8)[0] == 1
    nchunks, image_len, e_len, e_raw, bl_len, bl_raw = struct.unpack_from("<QQQQQQ", b, 16)
    t0 = HDR; e0 = t0 + nchunks * ENT; b0 = e0 + e_len; end = b0 + bl_len
    for (s, e), off in (((t0, e0), 64), ((e0, b0), 96), ((b0, end), 128)):
        assert hashlib.sha256(b[s:e]).digest() == b[off:off + 32]
    chunks = []
    for i in range(nchunks):
        p = t0 + i * ENT
        off, ulen, elen, comp = struct.unpack_from("<QIIB", b, p)
        chunks.append((off, ulen, elen, comp, b[p + 24:p + 56], b[p + 56:p + 88]))
    meta = zstd.decompress(b[e0:b0]); blob = zstd.decompress(b[b0:end]) if bl_len else b""
    n, = struct.unpack_from("<I", meta, 0); p = 4; ents = []
    for _ in range(n):
        kind, mode, uid, gid, mtime, size, data_off, small = struct.unpack_from("<BIIIqQQB", meta, p); p += 38
        strs = []
        for _ in range(2):
            l, = struct.unpack_from("<I", meta, p); strs.append(meta[p + 4:p + 4 + l]); p += 4 + l
        nx, = struct.unpack_from("<I", meta, p); p += 4; xs = []
        for _ in range(2 * nx):
            l, = struct.unpack_from("<I", meta, p); xs.append(meta[p + 4:p + 4 + l]); p += 4 + l
        ents.append((kind, mode, uid, gid, mtime, size, data_off, small, strs[0], strs[1], b"".join(xs) or None))
    return end, chunks, ents, blob, b[b0:end]

SCHEMA = """
CREATE TABLE chunks(image_off INTEGER PRIMARY KEY, ulen INT, elen INT, comp INT, stored_sha BLOB, raw_sha BLOB);
CREATE TABLE entries(id INTEGER PRIMARY KEY, kind INT, mode INT, uid INT, gid INT, mtime INT, size INT,
  data_off INT, small INT, path BLOB, target BLOB, xattrs BLOB);
"""

def build(path, chunks, ents, blob, blob_z, variant):
    if os.path.exists(path): os.unlink(path)
    db = sqlite3.connect(path)
    db.executescript("PRAGMA page_size=4096; PRAGMA journal_mode=OFF;" + SCHEMA)
    db.executemany("INSERT INTO chunks VALUES(?,?,?,?,?,?)", chunks)
    db.executemany("INSERT INTO entries(kind,mode,uid,gid,mtime,size,data_off,small,path,target,xattrs) "
                   "VALUES(?,?,?,?,?,?,?,?,?,?,?)", ents)
    if variant == "a":
        db.execute("CREATE TABLE small(entry INTEGER PRIMARY KEY, data BLOB)")
        rows, seen = [], set()
        for i, e in enumerate(ents, 1):
            if e[0] == 0 and e[7] and e[6] not in seen:
                seen.add(e[6]); rows.append((i, blob[e[6]:e[6] + e[5]]))
        db.executemany("INSERT INTO small VALUES(?,?)", rows)
    else:
        db.execute("CREATE TABLE blob(data BLOB)"); db.execute("INSERT INTO blob VALUES(?)", (blob_z,))
    db.commit(); db.execute("VACUUM"); db.close()

def read_sqlite(path, variant):
    db = sqlite3.connect(f"file:{path}?mode=ro&immutable=1", uri=True)
    n = len(db.execute("SELECT * FROM entries").fetchall()) + len(db.execute("SELECT * FROM chunks").fetchall())
    if variant == "a":
        n += sum(len(r[0]) for r in db.execute("SELECT data FROM small"))
    else:
        n += len(zstd.decompress(db.execute("SELECT data FROM blob").fetchone()[0]) or b"")
    db.close(); return n

def main():
    src, out = sys.argv[1], sys.argv[2]; os.makedirs(out, exist_ok=True)
    tot = dict(v1=0, a=0, az=0, b=0, bz=0, ents=0, chunks=0)
    t_v1 = t_a = t_b = 0.0
    names = sorted(f for f in os.listdir(src) if f.endswith(".btrpkg"))
    for f in names:
        b = open(os.path.join(src, f), "rb").read()
        idx_len, chunks, ents, blob, blob_z = parse_index(b)
        idx = b[:idx_len]
        tot["v1"] += idx_len; tot["ents"] += len(ents); tot["chunks"] += len(chunks)
        for v in "ab":
            p = os.path.join(out, f"{f[:-7]}.{v}.sqlite")
            build(p, chunks, ents, blob, blob_z, v)
            raw = open(p, "rb").read()
            tot[v] += len(raw); tot[v + "z"] += len(zstd.compress(raw, 19))
        t = time.perf_counter(); parse_index(idx); t_v1 += time.perf_counter() - t
        for v in "ab":
            p = os.path.join(out, f"{f[:-7]}.{v}.sqlite")
            t = time.perf_counter(); read_sqlite(p, v); dt = time.perf_counter() - t
            if v == "a": t_a += dt
            else: t_b += dt
    mb = lambda x: f"{x / 1e6:.1f} MB"
    print(f"{len(names)} packages, {tot['ents']} entries, {tot['chunks']} chunks")
    print(f"v1 index            {mb(tot['v1'])}   read {t_v1 * 1e3:.0f} ms (Python)")
    print(f"A sqlite            {mb(tot['a'])} raw, {mb(tot['az'])} zstd-19 ({tot['az'] / tot['v1'] - 1:+.0%})   read {t_a * 1e3:.0f} ms")
    print(f"B sqlite+zstd blob  {mb(tot['b'])} raw, {mb(tot['bz'])} zstd-19 ({tot['bz'] / tot['v1'] - 1:+.0%})   read {t_b * 1e3:.0f} ms")

if __name__ == "__main__":
    main()
