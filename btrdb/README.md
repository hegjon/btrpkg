# btrdb: thin sync databases (prototype)

`pacman -Sy` downloads a changed sync database whole (Arch's `extra.db`: 9 MB, many times a day).
btrdb publishes the same database as one small index, `REPO.btrdb`, that names each package's
`desc` (and `files`) entry by the SHA-256 of a blob stored once in a content-addressed `db/`
directory. A client downloads the index on every change and only the blobs it doesn't have, then
rebuilds the `REPO.db` tar pacman reads. Compared in a trial with `zdb/` (zchunk), which needs no
index of its own.

```
btrdb/                          (~/Work/btrpkg-repo/btrdb, served at :1764/btrdb/)
├── version                     "0\n"
├── x86_64/
│   ├── core.btrdb, extra.btrdb the index, zstd-compressed whole; Last-Modified = upstream's db
│   └── *.btrdb.sig             signify signature over the stored file (btrpkg sign / verify)
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

- `btrdb.py publish ROOT ARCH REPO REPO.db [--files REPO.files] [--key SEC]`: blobs first, then
  signature and index; the previous 3 indexes stay as `REPO.btrdb.<timestamp>`, blobs none of the
  kept indexes names go after an hour. `mirror/mirror-all.sh` runs it every 10 minutes for core
  and extra from zdb's `.upstream/` copies.
- `btrdb.py sync URL SYNCDIR REPO [--files]`: conditional GET of the index and its `.sig`,
  `btrpkg verify`, the missing blobs (8 parallel HTTP/1.1 connections, each checked against its
  hash), blobs kept in `SYNCDIR/btrdb.sqlite`; writes `REPO.db` (and `REPO.files`: desc + files)
  as a plain tar stamped with the index's timestamp. Prints `REPO BYTES REQUESTS SECONDS STATE`.
- `btrdb.py compare A B`, `btrdb.py dump REPO.btrdb`, `btrdb.py train DICTDIR DB... --files FILES...`.
- Tests: `python3 btrdb/test_btrdb.py` (needs Python 3.14's `compression.zstd` and a btrpkg build).

Measured (2026-11-24, Arch extra, 15,303 packages): the index is 666 KB (most of it the 32-byte
desc hash per package, which doesn't compress); the desc blobs 8.4 MB in total, a first sync
9.04 MB (extra.db is 9.06 MB). For 5 hours of changes (19 updates) a sync downloads 676 KB
(index + 9.9 KB of blobs) where zchunk downloads 328 KB (most of it its header). The trial on p14s
(`zdb/zdb-trial.sh`, `/var/log/btrdb-trial.tsv` beside `/var/log/zdb-trial.tsv`, until 2026-11-26
12:00) compares both on hourly and daily syncs.

Not built: pacman support (the client writes the tar pacman reads), a local SQLite query store
for anything beyond the blob cache, `.files` in the trial.
