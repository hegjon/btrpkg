#!/usr/bin/env python3
"""Could the whole index be one zstd-compressed CBOR document? Size and read time against version 1.

usage: uv run --with cbor2 python bench/cbor_index.py DIR    (DIR holds version-1 .btrpkg files)

  v1        the current index (table stored raw, entries and blob as separate zstd -19 frames)
  cbor-arr  one CBOR doc {v, chunks: [[off, ulen, elen, comp, sha, rawsha]...], entries: [[...]...], blob: bstr},
            canonical encoding, whole doc through zstd -19
  cbor-map  the same with entries as maps with text keys (the usual self-describing CBOR style)
  cbor-noff cbor-arr without image offsets (derivable from the lengths)
"""
import os, sys, time
import cbor2
from compression import zstd
sys.path.insert(0, os.path.dirname(__file__))
from sqlite_index import parse_index

KEYS = ("kind", "mode", "uid", "gid", "mtime", "size", "data_off", "small", "path", "target", "xattrs")

def doc(chunks, ents, blob, style):
    cs = [list(c[1:]) if style == "noff" else list(c) for c in chunks]
    es = [dict(zip(KEYS, e)) if style == "map" else list(e) for e in ents]
    return {"v": 1, "chunks": cs, "entries": es, "blob": blob}

def main():
    src = sys.argv[1]
    tot = dict(v1=0, arr=0, map=0, noff=0, arr_raw=0)
    tread = dict(v1=0.0, arr=0.0)
    names = sorted(f for f in os.listdir(src) if f.endswith(".btrpkg"))
    for f in names:
        b = open(os.path.join(src, f), "rb").read()
        end, chunks, ents, blob, _ = parse_index(b)
        tot["v1"] += end
        t = time.perf_counter(); parse_index(b[:end]); tread["v1"] += time.perf_counter() - t
        for style in ("arr", "map", "noff"):
            raw = cbor2.dumps(doc(chunks, ents, blob, style), canonical=True)
            z = zstd.compress(raw, 19)
            tot[style] += len(z)
            if style == "arr":
                tot["arr_raw"] += len(raw)
                t = time.perf_counter(); cbor2.loads(zstd.decompress(z)); tread["arr"] += time.perf_counter() - t
    mb = lambda x: f"{x / 1e6:.2f} MB"
    print(f"{len(names)} packages")
    print(f"v1         {mb(tot['v1'])}                read {tread['v1'] * 1e3:.0f} ms (hash checks + zstd + Python struct parse)")
    print(f"cbor-arr   {mb(tot['arr'])} ({tot['arr'] / tot['v1'] - 1:+.1%}), {mb(tot['arr_raw'])} before zstd   read {tread['arr'] * 1e3:.0f} ms (zstd + cbor2 C decoder)")
    print(f"cbor-map   {mb(tot['map'])} ({tot['map'] / tot['v1'] - 1:+.1%})")
    print(f"cbor-noff  {mb(tot['noff'])} ({tot['noff'] / tot['v1'] - 1:+.1%})")

if __name__ == "__main__":
    main()
