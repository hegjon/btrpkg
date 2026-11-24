# btrdb: thin sync databases (prototype)

`pacman -Sy` downloads a changed sync database whole (Arch's `extra.db`: 9 MB, many times a day).
btrdb publishes the same database as one small index, `REPO.btrdb`, that names each package's
`desc` (and `files`) entry by the SHA-256 of a blob stored once in a content-addressed `db/`
directory. A client downloads the index on every change and only the blobs it doesn't have, then
rebuilds the `REPO.db` tar pacman reads. A client that has an older index downloads only an
index patch: the new index as a zstd frame that uses its old index as the prefix
(`zstd --patch-from`), a few hundred bytes. Compared in a trial with `zdb/` (zchunk).

```
btrdb/                          (~/Work/btrpkg-repo/btrdb, served at :1764/btrdb/)
├── version                     "0\n"
├── x86_64/
│   ├── core.btrdb, extra.btrdb the index, zstd-compressed whole; Last-Modified = upstream's db
│   ├── *.btrdb.sig             signify signature over the decompressed index (btrpkg sign / verify);
│   │                           Last-Modified = upstream's db: the client's change marker
│   ├── extra.btrdb.patch/H     the current index as a patch against an older one, H = SHA-256 of
│   │                           that older decompressed index; one per index of the last 7 days
│   └── extra.btrdb.<ts>        those older indexes (at least 3), for making the patches
└── db/                         desc/files blobs, NOCOMPRESS
    └── 12/1234abcd…            zstd with the format's dictionary; named by the SHA-256 of
                                the stored (compressed) bytes
```

Format version 0 (unstable), little endian, inside the zstd frame:

```
header  magic "BTRDB\0\0\0" | version u16 = 0 | flags u16 = 0 | package_count u32 | timestamp u64
entry   name_len u8, name | version_len u8, version | arch_len u8, arch | desc_hash[32] | files_hash[32] (zeros: none)
```

Entries are sorted by name (unique); a reader rejects anything else, trailing data, unsafe names
and a missing desc hash. The dictionaries (`dicts/desc-dict-v0.zst`, `files-dict-v0.zst`, 110 KiB,
trained on Arch's core + extra of 2026-11-24) belong to the format version and ship with the
client: a new dictionary changes every blob, so it needs a new version.

- `btrdb.py publish ROOT ARCH REPO REPO.db [--files REPO.files] [--key SEC] [--keep-hours H]`:
  blobs first, then a patch from every kept older index (7 days by default; zstd 12, about 9 ms
  each for extra),
  signature, index, and the new patch directory in place of the old; blobs no kept index names go
  after an hour. `mirror/mirror-all.sh` runs it every 10 minutes for core
  and extra from zdb's `.upstream/` copies.
- `btrdb.py sync URL SYNCDIR REPO [--files] [--measure-full]`: conditional GET of `REPO.btrdb.sig`
  (304: nothing to do); with an older index, `REPO.btrdb.patch/HASH` applied to it; otherwise (no
  index, no patch for it, or a result that fails the signature, e.g. a publish in between) the
  whole index. `btrpkg verify` on the result, then the missing blobs (8 parallel HTTP/1.1
  connections, each checked against its hash), kept in `SYNCDIR/btrdb.sqlite`; writes `REPO.db`
  (and `REPO.files`: desc + files) as a plain tar stamped with the index's timestamp, and keeps
  the decompressed index as `.REPO.btrdb`. Prints `REPO BYTES REQUESTS SECONDS patch|full|unchanged
  FULL_BYTES` (`--measure-full`: what the sync would have taken with the whole index).
- `btrdb.py compare A B`, `btrdb.py dump REPO.btrdb`, `btrdb.py train DICTDIR DB... --files FILES...`.
- Tests: `python3 btrdb/test_btrdb.py` (needs Python 3.14's `compression.zstd` and a btrpkg build).

Measured (2026-11-24, Arch extra, 15,303 packages): the index is 666 KB (1.43 MB decompressed,
490 KB of it the 32-byte desc hash per package, which doesn't compress); the desc blobs 8.4 MB in
total, a first sync 9.04 MB (extra.db is 9.06 MB). From 10:12 to 13:07 (6 updates):

| | bytes |
|---|---:|
| `pacman -Sy` (whole extra.db) | 9,056,189 |
| btrdb, whole index (p14s, measured) | 670,271 |
| zchunk (306,204 of it the header) | 315,010 |
| btrdb with an index patch (p14s: sig 148 + patch 370 + 6 desc blobs; 8 requests) | 4,886 |

The patch from 12:05 is 215 B; making one costs about 80 ms at zstd 19, about 9 ms at 12 for
1-2% more (level 12 is used); level 3's window is too small to reach the old index and gives
187 KB; applying one takes under 1 ms. zchunk can't patch its header: the client doesn't have
the new one.

Clients further behind (Arch's archived extra.db snapshots against the 13:07 index; bytes):

| behind | changed packages | btrdb: patch + blobs | btrdb: whole index + blobs | zchunk |
|---|---:|---:|---:|---:|
| 1 day | 283 | 150,422 | 805,540 | 570,458 |
| 2 days | 509 | 312,145 | 958,900 | 841,297 |
| 4 days | 822 | 521,412 | 1,156,602 | 1,154,489 |
| 7 days | 1,674 | 1,000,013 | 1,603,823 | 1,912,118 |
| 14 days | 3,471 | 2,127,225 | 2,664,830 | 3,566,515 |
| 31 days | 5,738 | 3,461,066 | 3,914,025 | 5,468,507 |

The desc blobs of the changed packages dominate after a day or two; past a week a patch saves
13-20% more, so publish keeps 7 days (about 170 indexes at one change an hour). The trial on p14s
(`zdb/zdb-trial.sh`, `/var/log/btrdb-trial.tsv` beside `/var/log/zdb-trial.tsv`, until 2026-11-26
12:00) compares both on hourly and daily syncs.

Not built: pacman support (the client writes the tar pacman reads), a local SQLite query store
for anything beyond the blob cache, `.files` in the trial.
