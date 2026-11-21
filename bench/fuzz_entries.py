#!/usr/bin/env python3
"""Random edits inside an index's decompressed entries (and blob), hashes recomputed, for
differential/sanitizer runs of index readers.

usage: uv run --with zstandard bench/fuzz_entries.py BASE.btridx OUTDIR N [SEED]
Writes OUTDIR/fNNNNN.btridx. Each variant flips, overwrites, inserts or deletes a few bytes of the
entries section (sometimes the blob), then re-compresses and rebuilds the header hashes.
"""
import hashlib, os, random, struct, sys
import zstandard

HDR, ENT = 256, 88

def sha(b): return hashlib.sha256(b).digest()

base, out, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
rng = random.Random(int(sys.argv[4]) if len(sys.argv) > 4 else 1)
os.makedirs(out, exist_ok=True)
b = open(base, "rb").read()
nch, image, el, erl, bl, brl = struct.unpack_from("<QQQQQQ", b, 16)
table = b[HDR:HDR + nch * ENT]
ez = b[HDR + nch * ENT:HDR + nch * ENT + el]
bz = b[HDR + nch * ENT + el:HDR + nch * ENT + el + bl]
er = zstandard.ZstdDecompressor().decompress(ez, max_output_size=erl)
blob = zstandard.ZstdDecompressor().decompress(bz, max_output_size=brl) if brl else b""
interesting = [0, 1, 2, 3, 0x2e, 0x2f, 0x7f, 0x80, 0xff]

def edit(x):
    x = bytearray(x)
    for _ in range(rng.choice([1, 1, 2, 3, 8])):
        if not x: x.append(0); continue
        op, i = rng.randrange(5), rng.randrange(len(x))
        if op == 0: x[i] ^= 1 << rng.randrange(8)
        elif op == 1: x[i] = rng.choice(interesting)
        elif op == 2: x[i:i] = bytes(rng.choice(interesting) for _ in range(rng.choice([1, 4, 8])))
        elif op == 3: del x[i:i + rng.choice([1, 4, 8])]
        else:  # overwrite a little-endian u32/u64 with an edge value
            v = rng.choice([0, 1, 0xffffffff, 0x7fffffff, len(x), len(x) + 1, 4096, 2**63])
            w = rng.choice([4, 8])
            x[i:i + w] = (v & (2 ** (8 * w) - 1)).to_bytes(w, "little")
    return bytes(x)

c = zstandard.ZstdCompressor(level=1, write_content_size=True)
for k in range(n):
    e2 = edit(er) if rng.random() < 0.9 else er
    b2 = edit(blob) if rng.random() < 0.2 else blob
    ez2, bz2 = c.compress(e2), c.compress(b2)
    h = bytearray(b[:HDR])
    struct.pack_into("<QQQQ", h, 32, len(ez2), len(e2), len(bz2), len(b2))
    h[96:128] = sha(ez2); h[128:160] = sha(bz2); h[224:256] = sha(bytes(h[:224]))
    open(f"{out}/f{k:05d}.btridx", "wb").write(bytes(h) + table + ez2 + bz2)
print(f"{n} variants in {out}")
