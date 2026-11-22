# btrpkg

A package format, repository tool and pacman integration for Omarchy (and Arch) on btrfs. A
package's large files are stored as **btrparts**: pieces of up to 128 KiB of one file's data,
each already in btrfs's on-disk form (a zstd frame, or raw data). The client writes them into the
pacman cache with `BTRFS_IOC_ENCODED_WRITE`, so the kernel neither decompresses nor recompresses
them, and installs files by reflinking them from the cache file. A file's data is never read,
written or compressed again at install time, and the installed files share their extents with the
cache. Btrparts are content-addressed, so an upgrade downloads only those it can't take from the
installed version or an older cache file.

Repositories keep the plain `.pkg.tar.zst` beside the btrpkg data: stock pacman reads the same
database and ignores the btrpkg fields, and pacman-btrpkg falls back to the package file whenever
the btrpkg path can't be used.

**Status (2026-11-22): lab trial.** A mirror on the desktop serves Omarchy's edge repository and
Arch's core/extra for x86_64, and Omarchy, ALARM and asahi-alarm for aarch64, at
`http://192.168.2.2:1764/`. Five lab machines (p14s, x200, t460s, t490, macmini-m1) run
pacman-btrpkg from the `[btrpkg]` CI repository. Signing uses a test key, and the CI packages are
unsigned. How it got here, with all measurements: `HISTORY.md` (the lab notebook, newest first).

Contents: [Repository layout](#repository-layout) · [File forms](#file-forms) ·
[Trust](#trust) · [Client: pacman-btrpkg](#client-pacman-btrpkg) ·
[Upgrades: what gets downloaded](#upgrades-what-gets-downloaded) ·
[Repository side](#repository-side) · [Commands](#commands) · [Packages and CI](#packages-and-ci) ·
[Format](#format-version-1) · [Tests](#tests) · [Open decisions and known gaps](#open-decisions-and-known-gaps)

Other documents: `HISTORY.md` (lab notebook and measurements), `libalpm/README.md` (the pacman patches), `mirror/README.md` (the mirror),
`packaging/README.md` (the packages), `PLAN.md` (plans and status), `src/format.rs` (the format,
in full).

## Repository layout

One package (gliff 0.4.3-1, x86_64) on the mirror:

```
omarchy-mirror/
├── x86_64/
│   ├── omarchy.db                                 pacman database, btrpkg fields in %DATA%
│   ├── gliff-0.4.3-1-x86_64.pkg.tar.zst (+ .sig)  the package file: stock pacman, fallback
│   ├── gliff-0.4.3-1-x86_64.pkg.tar.zst.sha256sum "HASH  FILE": the package file this index came from
│   └── gliff-0.4.3-1-x86_64.pkg.btr     (+ .sig)  the index (thin .pkg.btr), signify signature
├── aarch64/                                       the same layout
└── btrparts/                                      shared by x86_64 and aarch64
    └── 98/989335ddfc1e…                           a btrpart, named by the SHA-256 of its stored bytes
```

The database entry keeps every stock field and adds:

```
%DATA%
btrpkg.index=gliff-0.4.3-1-x86_64.pkg.btr
btrpkg.id=ef575023…48d2        SHA-256 of the index header (the package's ID)
btrpkg.size=6169373            index + all btrparts: a full download (the .pkg.tar.zst is 6,591,746)
btrpkg.parts=../btrparts       where the btrparts are, relative to the repository URL
```

- Btrpart names stay stable when the compressed bytes change (another zstd version, or the old
  level-19 btrparts):
  `convert` looks up each btrpart's unencoded hash in a map built from the indexes of the
  repositories sharing the btrparts directory and reuses the btrpart already stored for that data,
  after checking it.
- `btrpkg.parts` lets architectures (and Arch's `$repo/os/$arch` repositories, with
  `../../../btrparts`) share one btrparts directory. Without the field it is `btrparts` inside the
  repository (the ISO's offline repository).
- A btrpkg repository must not be a pacman `CacheDir`: its thin `NAME.pkg.btr` files have the
  names of cache files.

## File forms

A package exists in two forms, both starting with the same index; they are told apart by their
length (the header records the index length and the image length):

| form | where | layout | length |
|---|---|---|---|
| thin (index) | repository `NAME.pkg.btr`, its btrparts in `btrparts/xx/HASH` | header, btrpart table, entries, small-file blob | index length |
| cache | pacman cache `NAME.pkg.btr` | index, zeros up to 4 KiB, then the image (each large file 4 KiB-aligned, as encoded extents) | `align(index) + image length` |

`btrpkg convert` writes a package file straight into a repository (index and btrparts); `pull` and
`pull-ids` make cache files from a repository. Reading an index parses only its first bytes, so a
cache file works as an index too. Whatever needs a cache file checks the length: `open_cache()`/`cache_head()` in Rust (exact), libalpm's `read_index` (at
least that long: "cache file shorter than its index says"). Details: "Format (version 1)" below
and `src/format.rs`.

## Trust

- The database's `btrpkg.id` is the SHA-256 of the index header; the header holds the SHA-256 of
  the btrpart table, entries and blob, and the table holds each btrpart's stored-bytes hash and
  unencoded hash. Each section is checked before the next is read, each btrpart before it is
  written.
- The index header is signed with signify (Ed25519; `NAME.pkg.btr.sig`). Keys: `/etc/btrpkg/keys`,
  `/usr/share/btrpkg/keys`. pacman-btrpkg checks it as `SigLevel` asks and records
  `btrpkg-validation=signify:KEYFILE`; `%VALIDATION%` says `sha256`.
- The mirror converts and signs a package only after its upstream OpenPGP signature (gpgv) and the
  database's SHA-256 checked out, so a client that trusts the mirror's key gets nothing upstream
  didn't sign.
- Local data (installed files, older cache files) is reused only after reading it back and checking
  its unencoded hash. Installation resolves every path beneath the root with `openat2`
  (`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`).

## Client: pacman-btrpkg

`pacman-btrpkg` is Arch's pacman 7.1.0 (`54d94116`) plus 10 libalpm patches (`libalpm/README.md`),
installed beside the system pacman (`/usr/bin/pacman-btrpkg`, `/etc/pacman-btrpkg.conf`, which
includes `/etc/pacman.conf` and sets `BtrPkgHelper = /usr/bin/btrpkg`). With a helper set and the
cache on btrfs, a sync target with btrpkg fields comes as `NAME.pkg.btr`:

1. `_alpm_sync_prepare`: `btrpkg fetch-ids --index-only` fetches the indexes, `pull-ids --plan`
   reports what local data can't cover; the summary shows the real download size.
2. Download: `fetch-ids` (network, as `DownloadUser`, seccomp + Landlock, writes only to
   `CACHEDIR/download-btrpkg/`) fetches the missing btrparts from the servers in order
   (CacheServer first); `pull-ids` (root, no network) checks everything and assembles the cache
   files with encoded writes and reflinks. With the sandbox off (pacstrap) one root `pull-ids` does
   both.
3. Install: pacman's own extraction over a tar stream made from the index; large files are
   reflinked from the cache file. Scripts, backups/`.pacnew`, hooks and the database work as usual.

Network: every request of a btrpkg process goes through one long-lived curl multi handle, so a run
opens one connection per server (HTTP/2 or 3; up to 8 kept-alive ones over HTTP/1.1) and keeps it
for all its downloads: the indexes and signatures, all requested at once, then the btrparts.
Checking and writing btrparts runs on a thread of its own, so the transfer loop keeps reading and
acknowledging (QUIC is in userspace). At 50 ms round-trip time, planning an upgrade of 100 packages
takes 0.36 s over HTTP/2 and 0.28 s over HTTP/3 (`bench/net-latency-bench.sh`). The plan and the
download are two runs (pacman asks in between), so a transaction opens two connections.

Fallbacks to the package file (checked by sha256sum and OpenPGP as usual): no server has the index
(a database older than what the servers keep), or the btrpkg download of a repository fails. A
`.pkg.tar.*` already in the cache wins over btrpkg (offline mirrors).

Also: `pacman -U NAME.pkg.btr` (a cache file, signify `.sig`), `pacstrap-btrpkg` (new systems), `makebtrpkg`
(makepkg that also converts the packages), `btrpkg cache-clean` (paccache for `.pkg.btr`), and `pacbtr` / `btrpkg upgrade`, a standalone upgrader
(metadata packages + `pacman -U --dbonly`).

## Upgrades: what gets downloaded

Example, measured with both versions converted into a scratch repository: hyprland 0.57.1-2 →
0.57.2-2 has 647 btrparts, 503 of them (48.0 MB) identical to 0.57.1's. The download is the index
(177,621 B) and 144 btrparts (5.93 MB): about 6.1 MB instead of the 53.7 MB `.pkg.tar.zst`.

Every btrpart in the new index's table carries the SHA-256 of its uncompressed data. `pull-ids`
(root, no network) gives each one a source, in this order:

1. An older cache file of the same package (`hyprland-0.57.1-2-x86_64.pkg.btr`): the cache files
   in CACHEDIR with the same package name and the exact cache-file length, looked up by
   (uncompressed hash, length) in their btrpart tables.
2. The installed file at the same path: the index's entries say which file and offset each btrpart
   belongs to; the file must exist under the root and be long enough.
3. Otherwise: download `btrparts/xx/HASH` (named by its stored-bytes hash).

Candidates from 1 and 2 are read back and hashed; only a match counts, so a locally changed or
damaged file just means a download. Matches from cache files are reflinked into the new cache
file; from installed files whole pieces are reflinked and a file's last piece is compressed again.
Only the same package's cache files and installed files are searched, not other packages.

The same pass serves both steps of a transaction: `pull-ids --plan` counts the missing bytes (the
download size pacman shows), and `pull-ids --fetch-via FILE` lists the missing btrparts for the
unprivileged `fetch-ids --want FILE`, which downloads exactly those and checks each against its
stored hash (`pull-ids` checks again before writing). A btrpart already in the download directory
with the right length isn't fetched again, so an interrupted download resumes.

The check is cheap: pulling hyprland 0.57.2 next to the 0.57.1 cache file, hashing the 503 reused
btrparts and writing the 144 new ones, took 0.04 s (old cache file in memory). btrfs has no way to
ask for a content hash instead: its data checksums are crc32c by default and cover 4 KiB of the
on-disk (compressed) bytes, readable only by a raw tree search as root; `BTRFS_IOC_ENCODED_READ`
skips decompression but still reads the data, and fits only extents btrpkg wrote itself; fs-verity
would need another hash scheme and read-only files.

### Reading installed files

btrfs isn't asked for anything here: an installed file is opened by its path, read and hashed like
any file; btrfs only comes in for the reflink.

1. Path and offset come from the new index: `chunk_files` maps each btrpart's image offset to
   (path, offset in file, data length), e.g. btrpart 37 → `ROOT/usr/bin/Hyprland`, offset
   4,718,592, 131,072 bytes (large files are consecutive 128 KiB btrparts). ROOT is what libalpm
   passes (`--installed /`).
2. `open(O_RDONLY | O_NOFOLLOW | O_NONBLOCK)` (no symlink at the last component, no hang on a
   FIFO), then `fstat`: a regular file, at least offset + data length long.
3. `pread` at the offset, zero-padded to the btrpart's length (only a file's last piece is shorter),
   SHA-256 compared with the btrpart's uncompressed hash from the signed index. Candidates are
   checked in parallel; `--plan` stops here.
4. A whole piece is reflinked (`FICLONERANGE`) from the same descriptor that was hashed into the
   cache file at the btrpart's image offset, so a file replaced after the check can't slip in;
   nothing is written or compressed. A file's last piece can't be reflinked (reflinks need 4
   KiB-aligned lengths except at end of file, and it would end mid cache file), so the bytes
   already read are compressed at level 12 and written as an encoded extent. Without reflink
   support the data is written instead; still no download.

If stock pacman installed the old version on a `compress=zstd:3` mount, the cache file shares those
zstd:3 extents; the data is the same, only the on-disk compression differs, until a later version
downloads that data. Reading kernel-compressed files makes btrfs decompress them, which the 0.04 s
above (files in memory) doesn't include.

Installed files are only looked for at the new path, so data that moved is downloaded again when
the old cache file is gone (the cache-file lookup is by content and finds it).
`bench/moved-files.py` over the 24 packages with two versions on the mirror: 244.0 MB of the
1,107.5 MB download is at the same path in the old version, another 117.3 MB moved (almost all
kernel modules and headers, which move to a new `/usr/lib/modules/VERSION/`).

## Repository side

- `btrpkg repo-update [--db-in UPSTREAM.db] DIR REPO`: converts and publishes every new package
  file in DIR (checked against the database's SHA-256 and, unless `--no-gpg`, its OpenPGP
  signature), signs the indexes (`--key`), and writes REPO.db last with the btrpkg fields.
  Unchanged packages are recognised by `PKGFILE.sha256sum` (written after a conversion, sha256sum's
  format, so `sha256sum -c` checks the package file too).
- `makebtrpkg` (in pacman-btrpkg, details in `packaging/README.md`): makepkg that also puts the
  packages into a btrpkg repository directory, and with `BTRREPO` into its database.
- `mirror/` (details in `mirror/README.md`): `mirror-all.sh` runs `repo-mirror.sh` for each line of
  `repos.conf` every 10 minutes (systemd user timer): upstream database through pacoloco, the
  wanted packages (lists plus 404s learned from caddy's log), `repo-update`, then `btrpkg gc` per
  btrparts directory.
- `btrpkg gc BTRPARTS`: in every repository sharing the directory, keeps each package's current
  version, anything newer and one older (`--keep 2`), removes the other indexes and package files,
  then the btrparts no kept index names. Nothing younger than an hour goes; mirror locks are held.

## Commands

`btrpkg` with no arguments prints the full usage. In short:

| | commands |
|---|---|
| package files | `convert`, `info`, `dump`, `keygen`, `sign` |
| client, by hand | `pull` (repository → cache), `install` (cache → root), `cache-clean`, `upgrade` (= `pacbtr`) |
| client, for pacman | `fetch-ids` (unprivileged network half), `pull-ids` (root half; `--plan`, `--fetch-via`) |
| repository | `convert` (package file → repository), `repo-update`, `gc` |

Trust options (`--id`, `--sig`, `--unverified`) and network options (`--http h1|h2|h3`, `--h2c`,
`--cacert`) are in the usage text.

## Packages and CI

- `btrpkg-git` (`/usr/bin/btrpkg`, `/usr/bin/pacbtr`) and `pacman-btrpkg`, built by
  `.github/workflows/packages.yml` for x86_64 and aarch64 on every push and published as the
  `[btrpkg]` repository: `Server = https://github.com/hegjon/btrpkg/releases/download/$arch`,
  `SigLevel = Optional` (unsigned for now).
- A machine joins with `packaging/use-btrpkg-repo.sh` (adds the section, refreshes only
  `btrpkg.db`). Local builds: `packaging/build.sh` (container), `packaging/build-local-toolchain.sh`.
- The pacman patches live on the `btrpkg` branch of `~/Work/pacman` and are exported to
  `libalpm/00*.patch`; the PKGBUILD applies all of them.

## Format (version 1)

The full description is at the top of `src/format.rs`; a worked example follows the tables. An
index is header, btrpart table, entries and small-file blob, in that order.

| part | contents |
|---|---|
| header (256 B) | `BTRPKG\0\0`, version 1, hash algorithm (1 = SHA-256), number of btrparts, image length, stored and uncompressed length of entries and blob, SHA-256 of the table, entries and blob, 64 reserved zero bytes, SHA-256 of the header (the package's ID, `btrpkg.id`; what the signature covers) |
| btrpart table | per btrpart 88 B: image offset, unencoded length (4 KiB multiple, ≤128 KiB), stored length, compression (0 raw, 2 zstd), SHA-256 of the stored bytes, SHA-256 of the unencoded bytes. In image order, no gaps |
| entries | one zstd frame (level 19): kind, mode, uid, gid, mtime, size, data offset, small flag, path, link target, xattrs |
| small-file blob | one zstd frame (level 19): the contents of all small files |
| btrpart (repository `btrparts/xx/HASH`) | a zstd frame with window ≤128 KiB, no checksum, no dictionary ID; or raw data |

Reading checks each section before the next is read, so the btrpart table can be used (downloads
started) before the rest arrives. Readers also check what `install` relies on: safe
relative paths, known entry kinds, file data inside the image or blob, btrparts that tile the
image. Lengths in the header are capped and nothing is allocated from them before the data has
arrived. Tests: `cargo test` (flipped bits and truncations of an index or a btrpart are rejected
and leave no files behind).

Converter rules:
- Files of `--small` bytes or less (default 4096) go in the blob.
- Identical file contents are stored once and reflinked more than once (dedup).
- Btrparts are zstd level 12, fixed (also for those `pull` makes from installed files): the
  highest level that decodes as fast as level 3 in btrfs on every lab machine; 13 is 5–10% slower
  and 15–19 about 20% slower, for 1–5% less size (`bench/results/zstd-level-read-20261122/`).
  Entries and blob are level 19 (metadata, read once).
- A btrpart stays raw when compressing it doesn't save at least one 4 KiB block.
- Root-level dotfiles (`.PKGINFO`, `.MTREE`, …) are kept in the entries and not installed.

### Example: gliff 0.4.3-1 (x86_64)

`btrpkg info` and `btrpkg dump` of the mirror's `gliff-0.4.3-1-x86_64.pkg.btr` (converted when
btrparts were still level 19; at 12 its btrparts would take 6,637,445 B instead of 6,153,725):

```
header        256 B   131 btrparts, image 16,588,800 B, entries 406 B (1,773), blob 3,458 B (6,100),
                      hashes of table/entries/blob; header hash ef575023…48d2 = btrpkg.id, what .sig signs
btrpart table 11,528 B  131 × 88 B, e.g.  offset 12288  unencoded 131072  stored 20014  zstd  c9550014…  3927f6b1…
entries       406 B   25 entries: 13 files, 12 directories
blob          3,458 B the 7 small files (6,100 B)
index         15,648 B = 256 + 11,528 + 406 + 3,458
```

Entries (image offsets for large files, blob offsets for small ones):

```
file 644    11,885  image @ 0           .BUILDINFO
file 644       280  blob  @ 0           .INSTALL
file 644       894  blob  @ 280         .MTREE
file 644     1,003  blob  @ 1174        .PKGINFO
dir  755                                usr, usr/bin, … (12 directories)
file 755 5,081,704  image @ 12288       usr/bin/gliff
file 755 5,799,448  image @ 5,095,424   usr/bin/gliff-probe
file 755 5,663,832  image @ 10,895,360  usr/bin/gliff-server
file 644       251  blob  @ 2177        usr/share/applications/gliff.desktop
file 644     6,623  image @ 16,560,128  usr/share/doc/gliff/README.md
file 644    18,291  image @ 16,568,320  usr/share/doc/gliff/hardware-quirks.md
file 644     1,157  blob  @ 2428        usr/share/icons/hicolor/scalable/apps/gliff.svg
file 644     1,072  blob  @ 3585        usr/share/licenses/gliff/LICENSE
file 644     1,443  blob  @ 4657        usr/share/licenses/gliff/NOTICE
```

- Large files start on 4 KiB boundaries of the image: gliff ends at 5,093,992, so gliff-probe
  starts at 5,095,424. Each is cut into btrparts of up to 128 KiB (all 131 of gliff's are zstd:
  16.6 MB of data stored as 6.15 MB).
- Root-level dotfiles (`.PKGINFO`, `.MTREE`, `.BUILDINFO`) are kept for pacman, not installed.

This package in the repository and in the cache:

```
thin (repository)  [index]                                                      15,648 B
btrparts           131 files, 6,153,725 B: a full download is 6,169,373 B = btrpkg.size
cache (pacman)     [index][zeros up to 16,384][image, 16,588,800 B]           16,605,184 B
```

In the cache file each btrpart sits at 16,384 + its image offset as a compressed extent
(`BTRFS_IOC_ENCODED_WRITE`): the file reads as 16.6 MB of plain data and takes about 6.2 MB on disk.
Installing `usr/bin/gliff` reflinks 5,081,704 bytes from offset 16,384 + 12,288, sharing those
extents; its data is never decompressed, copied or written again.

## Tests

- `cargo test`: 22 tests (format: bit flips and truncations rejected; convert/pull/install; reuse
  from cache files, installed files and across compressor changes; repo-update; gc; HTTP Range).
  Two need btrfs and skip elsewhere.
- libalpm: `meson test -C build pacman:btrpkg` (73 subtests on real and crafted indexes), pacman's
  own suite.
- On p14s: `bench/pacman-robust.sh` (sandbox roles, resume, dead and corrupt servers,
  DisableSandbox, fallbacks, `-Sc`; 12 checks), `bench/pacman-gaps-test.sh NAME` (`-U`, signatures,
  validation records; 16), `bench/plan-count.sh` (each btrpkg step runs once).
- Clients: `bench/btrparts-client-check.sh` (throwaway dbpath download), `bench/scratch-root-compare.sh`
  (stock vs btrpkg into scratch roots, compared file by file).

## Open decisions and known gaps

- **Signing and publishing** (needs Jonny): a real signify key and keyring package, signed `[btrpkg]`
  packages, where an Omarchy btrpkg repository would be published (R2/S3), database signing.
- **p14s daily trial**: how it should update unattended (`omarchy update` needs sudo).
- **ISO**: the `btrpkg` branch of omarchy-iso is ready but unbuilt (the build formats loop images,
  which the agent doesn't run; or a fork's CI).
- **Left for later**: one multi-server source for fetch-ids, pull-ids and `pacbtr` (server fallback
  is written three ways); committing per package so one bad package doesn't send its repository to
  package files; splitting `store::pull_many` and `upgrade()`; one fork/exec helper in libalpm;
  libalpm's cache-file length check (exact, and before the "already in the cache" shortcut).
- Code identifiers still say "chunk" for btrpart.
