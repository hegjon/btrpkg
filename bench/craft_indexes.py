#!/usr/bin/env python3
"""Crafted btrpkg indexes with valid hashes but bad contents, for differential tests of readers.

usage: uv run --with zstandard bench/craft_indexes.py BASE.btridx OUTDIR
Writes OUTDIR/NAME.btridx per case (header, table and section hashes recomputed, so only the
semantic checks can reject them) and OUTDIR/cases.txt with "NAME accept|reject" per line.
Readers (btrpkg dump, libalpm's btrdump) must accept exactly the "accept" ones.
"""
import hashlib, os, struct, sys
import zstandard

HDR, ENT = 256, 88

def sha(b): return hashlib.sha256(b).digest()

def parse(b):
    f = struct.unpack_from("<8sIB3xQQQQQQ", b, 0)
    n, image, el, erl, bl, brl = f[3:9]
    table = b[HDR:HDR + n * ENT]
    ez = b[HDR + n * ENT:HDR + n * ENT + el]
    bz = b[HDR + n * ENT + el:HDR + n * ENT + el + bl]
    chunks = [list(struct.unpack_from("<QIIB7x32s32s", table, i * ENT)) for i in range(n)]
    er = zstandard.ZstdDecompressor().decompress(ez, max_output_size=erl)
    blob = zstandard.ZstdDecompressor().decompress(bz, max_output_size=brl) if brl else b""
    return image, chunks, decode_entries(er), blob

def decode_entries(b):
    o = 0
    def take(n):
        nonlocal o; v = b[o:o + n]; o += n; return v
    def u(fmt):
        s = struct.calcsize(fmt); return struct.unpack("<" + fmt, take(s))[0]
    def by(): return take(u("I"))
    out = []
    for _ in range(u("I")):
        e = dict(kind=u("B"), mode=u("I"), uid=u("I"), gid=u("I"), mtime=u("q"), size=u("Q"), off=u("Q"), small=u("B"),
                 path=by(), target=by())
        e["x"] = [(by(), by()) for _ in range(u("I"))]
        out.append(e)
    assert o == len(b)
    return out

def encode_entries(es):
    w = [struct.pack("<I", len(es))]
    for e in es:
        w.append(struct.pack("<BIIIqQQB", e["kind"], e["mode"], e["uid"], e["gid"], e["mtime"], e["size"], e["off"], e["small"]))
        for s in (e["path"], e["target"]): w.append(struct.pack("<I", len(s)) + s)
        w.append(struct.pack("<I", len(e["x"])))
        for k, v in e["x"]: w.append(struct.pack("<I", len(k)) + k + struct.pack("<I", len(v)) + v)
    return b"".join(w)

def build(image, chunks, es, blob, er=None, hdr_patch=None, ez=None):
    table = b"".join(struct.pack("<QIIB7x32s32s", *c) for c in chunks)
    er = encode_entries(es) if er is None else er
    ez = zstandard.ZstdCompressor(level=3, write_content_size=True).compress(er) if ez is None else ez
    bz = zstandard.ZstdCompressor(level=3, write_content_size=True).compress(blob)
    f = dict(n=len(chunks), image=image, el=len(ez), erl=len(er), bl=len(bz), brl=len(blob), alg=1, ver=1, res=b"\0" * 3, tail=b"\0" * 64)
    if hdr_patch: hdr_patch(f)
    h = struct.pack("<8sIB3sQQQQQQ32s32s32s64s", b"BTRPKG\0\0", f["ver"], f["alg"], f["res"], f["n"], f["image"], f["el"], f["erl"],
                    f["bl"], f["brl"], sha(table), sha(ez), sha(bz), f["tail"])
    return h + sha(h) + table + ez + bz

def main():
    base, out = sys.argv[1], sys.argv[2]
    os.makedirs(out, exist_ok=True)
    image, chunks, es, blob = parse(open(base, "rb").read())
    large = next(i for i, e in enumerate(es) if e["kind"] == 0 and not e["small"])
    small = next((i for i, e in enumerate(es) if e["kind"] == 0 and e["small"] and e["size"] > 0), None)
    d = next(i for i, e in enumerate(es) if e["kind"] == 1)
    cases = []

    def case(name, expect, **kw):
        c = [list(x) for x in kw.pop("chunks", chunks)]
        e = [dict(x) for x in kw.pop("es", es)]
        fn = kw.pop("f", None)
        if fn: fn(c, e)
        open(f"{out}/{name}.btridx", "wb").write(build(kw.pop("image", image), c, e, kw.pop("blob", blob), **kw))
        cases.append((name, expect))

    def ent(i, **v):
        def f(c, e): e[i].update(v)
        return f
    def add(**v):
        def f(c, e):
            x = dict(kind=1, mode=0o755, uid=0, gid=0, mtime=0, size=0, off=0, small=0, path=b"zz", target=b"", x=[]); x.update(v); e.append(x)
        return f

    case("identity", "accept")
    case("reencoded", "accept", f=lambda c, e: None)  # same content, our own encoding
    for p in [b"", b"/etc/passwd", b"a/../../x", b"./x", b"a//b", b"a/", b"x\0y", b"..", b"a/."]:
        case("path_" + (p.hex() or "empty"), "reject", f=ent(d, path=p))
    case("symlink_no_target", "reject", f=add(kind=2, path=b"zzlink", target=b""))
    case("symlink_abs_ok", "accept", f=add(kind=2, path=b"zzlink", target=b"/etc/passwd"))
    case("symlink_nul", "reject", f=add(kind=2, path=b"zzlink", target=b"a\0b"))
    case("through_symlink", "reject", f=lambda c, e: (add(kind=2, path=b"zzl", target=b"/tmp")(c, e), add(kind=1, path=b"zzl/x")(c, e)))
    case("through_file", "reject", f=lambda c, e: add(kind=1, path=es[large]["path"] + b"/x")(c, e))
    case("dir_over_file", "reject", f=lambda c, e: add(kind=1, path=es[large]["path"])(c, e))
    case("hardlink_ok", "accept", f=add(kind=3, path=b"zzhl", target=es[large]["path"]))
    case("hardlink_missing", "reject", f=add(kind=3, path=b"zzhl", target=b"no/such"))
    case("hardlink_to_dir", "reject", f=add(kind=3, path=b"zzhl", target=es[d]["path"]))
    case("hardlink_unsafe", "reject", f=add(kind=3, path=b"zzhl", target=b"../x"))
    case("hardlink_forward", "reject", f=lambda c, e: e.insert(0, dict(kind=3, mode=0o644, uid=0, gid=0, mtime=0, size=0, off=0, small=0, path=b"zzhl", target=es[large]["path"], x=[])))
    case("kind_4", "reject", f=ent(d, kind=4))
    case("small_flag_2", "reject", f=ent(d, small=2))
    case("xattr_empty_name", "reject", f=ent(d, x=[(b"", b"v")]))
    case("xattr_nul_name", "reject", f=ent(d, x=[(b"user.a\0b", b"v")]))
    case("xattr_ok", "accept", f=ent(d, x=[(b"user.a", b"\0\1\2")]))
    case("large_unaligned", "reject", f=ent(large, off=es[large]["off"] + 1))
    case("large_past_image", "reject", f=ent(large, off=image))
    case("large_size0", "reject", f=ent(large, size=0))
    case("large_size_huge", "reject", f=ent(large, size=2**64 - 1))
    case("large_off_overflow", "reject", f=ent(large, off=2**64 - 4096))
    if small is not None:
        case("small_past_blob", "reject", f=ent(small, off=len(blob)))
        case("small_overflow", "reject", f=ent(small, off=2**64 - 1))
        case("small_end_exact", "accept", f=ent(small, off=len(blob) - es[small]["size"]))
    case("entries_trailing", "reject", er=encode_entries(es) + b"\0")
    case("entries_count_high", "reject", er=struct.pack("<I", len(es) + 1) + encode_entries(es)[4:])
    case("entries_count_huge", "reject", er=struct.pack("<I", 2**32 - 1) + encode_entries(es)[4:])
    ez2 = zstandard.ZstdCompressor(level=3).compress(encode_entries(es))
    case("entries_two_frames", "reject", ez=ez2 + ez2, er=encode_entries(es) * 2)
    case("entries_extra_frame", "reject", ez=ez2 + zstandard.ZstdCompressor(level=3).compress(b""), er=encode_entries(es))
    case("entries_trailing_garbage", "reject", ez=ez2 + b"junk", er=encode_entries(es))
    case("entries_raw_len_short", "reject", hdr_patch=lambda f: f.update(erl=f["erl"] - 1))
    case("entries_raw_len_long", "reject", hdr_patch=lambda f: f.update(erl=f["erl"] + 1))
    case("blob_raw_len_long", "reject", hdr_patch=lambda f: f.update(brl=f["brl"] + 1))
    case("hdr_version2", "reject", hdr_patch=lambda f: f.update(ver=2))
    case("hdr_alg2", "reject", hdr_patch=lambda f: f.update(alg=2))
    case("hdr_reserved", "reject", hdr_patch=lambda f: f.update(res=b"\0\1\0"))
    case("hdr_tail", "reject", hdr_patch=lambda f: f.update(tail=b"\0" * 63 + b"\1"))
    case("hdr_nchunks_huge", "reject", hdr_patch=lambda f: f.update(n=2**40))
    case("hdr_entries_raw_cap", "reject", hdr_patch=lambda f: f.update(erl=(256 << 20) + 1))
    case("hdr_blob_raw_cap", "reject", hdr_patch=lambda f: f.update(brl=(1 << 30) + 1))
    case("hdr_entries_len_huge", "reject", hdr_patch=lambda f: f.update(el=2**33))
    case("hdr_image_long", "reject", hdr_patch=lambda f: f.update(image=f["n"] * 131072 + 4096))
    # chunk table
    def ch(i, k, v):
        def f(c, e): c[i][k] = v
        return f
    z = next(i for i, c in enumerate(chunks) if c[3] == 2)
    case("chunk_gap", "reject", f=ch(1 if len(chunks) > 1 else 0, 0, chunks[1 if len(chunks) > 1 else 0][0] + 4096))
    case("chunk_len0", "reject", f=ch(z, 1, 0))
    case("chunk_len_unaligned", "reject", f=ch(z, 1, chunks[z][1] - 1))
    case("chunk_comp1", "reject", f=ch(z, 3, 1))
    case("chunk_zstd_not_smaller", "reject", f=ch(z, 2, chunks[z][1]))
    case("chunk_zstd_enc0", "reject", f=ch(z, 2, 0))
    case("chunk_raw_hash_differs", "reject", f=lambda c, e: (c[z].__setitem__(3, 0), c[z].__setitem__(2, c[z][1])))
    case("chunk_too_long", "reject", f=ch(z, 1, 131072 + 4096))
    case("chunks_short_of_image", "reject", image=image + 4096)
    case("chunk_reserved", "reject", f=lambda c, e: None, chunks=chunks)  # placeholder replaced below
    # reserved chunk bytes need a raw table edit
    raw = bytearray(build(image, chunks, es, blob))
    t0 = HDR + z * ENT + 17
    raw[t0] = 1
    tbl = bytes(raw[HDR:HDR + len(chunks) * ENT])
    hdr = bytearray(raw[:HDR]); hdr[64:96] = sha(tbl); hdr[224:256] = sha(bytes(hdr[:224]))
    open(f"{out}/chunk_reserved.btridx", "wb").write(bytes(hdr) + bytes(raw[HDR:]))
    with open(f"{out}/cases.txt", "w") as f:
        for n, e in cases: f.write(f"{n} {e}\n")
    print(f"{len(cases)} cases in {out}")

main()
