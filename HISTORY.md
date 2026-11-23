# btrpkg: history (lab notebook, newest first)

The current state is in `README.md`. The sections below were written as the work went; older ones describe earlier layouts (`index/`,
`.btridx`, `chunks/`) and options that have changed since. The first prototype's own description
is at the end ("The spike").

## zchunk compared (2026-11-23)

Question: could zchunk (github.com/zchunk/zchunk, Fedora's delta format for repository metadata)
be used? `bench/zchunk-compare.sh` (zchunk 1.5.5 in an archlinux container, defaults:
content-defined chunks, zstd, no dictionary): each package file's tar as `.tar.zck`, then
`zck_delta_size old new`; btrpkg from `convert` of old then new into an empty repository
(`bench/results/zchunk-vs-btrpkg-20261123.tsv`):

| | `.pkg.tar.zst` | zck full | zck upgrade | btrpkg full | btrpkg upgrade |
|---|---:|---:|---:|---:|---:|
| Hyprland set, 9 upgrades | 55.93 MB | 57.30 MB | 9.13 MB | 57.01 MB | 8.41 MB |
| linux-omarchy-headers 7.2.15-3 → 4 | 94.56 MB | 96.94 MB | 60.47 MB | 95.42 MB | 54.51 MB |

- Downloads: no gain over btrpkg here. zck finds slightly less to reuse (hyprland 6.82 against
  6.10 MB; only hyprtoolkit is smaller, 59 against 82 KB) and needs the old `.zck` (or, with
  `-u`, the old uncompressed tar) in the cache; btrpkg also reuses installed files.
- Install: zck chunks are slices of the tar stream cut by content, not 4 KiB-aligned pieces of
  one file of at most 128 KiB, and may use a zstd dictionary, so btrfs can't take them as encoded
  extents. The client would rebuild the tar, decompress and extract it as pacman does today,
  giving up the encoded writes, reflinks and shared extents that make btrpkg installs faster.
  Its header design is close to btrpkg's (one signed header holding every chunk's hash).
- Not tested: zchunk for the sync databases (`pacman -Sy` downloads every db whole), which is
  what Fedora uses it for. Omarchy's db is 64 KB; Arch's `extra.db`/`.files` are where it would
  matter, and pacman would need code for it.

## The package file when it is the smaller download (2026-11-23)

libalpm patch 12 (pacman `8f632f3f`, pacman-btrpkg 12): after `pull-ids --plan`, a target whose
planned btrpkg download is larger than its package file comes as the package file. Kernels were
the reason: their modules are already `.ko.zst`, so 128 KiB btrparts can't shrink them and lose the
package file's whole-stream window (linux-omarchy 7.2.15-4: 196.9 MB as btrpkg from scratch,
183.5 MB package file).

On p14s, dev build against pacman-btrpkg 11, `-Sw -dd` from the mirror into an empty btrfs cache
directory, 7.2.15-2 of both installed:

| package | btrpkg download | package file | pacman-btrpkg 11 | dev build |
|---|---:|---:|---|---|
| linux-omarchy 7.2.15-4 | 192,832,586 B | 183,505,130 B | btrpkg | package file |
| linux-omarchy-headers 7.2.15-4 | 73,129,120 B | 94,555,581 B | btrpkg | btrpkg |

Total download shown: 253.64 MiB before, 244.75 MiB now. Of the robust test's 10 packages,
omarchy-settings-dev (theme files, many small ones) also turns out bigger as btrpkg (3,206,701
against 2,673,145 B) and now comes as its package file.

`bench/pacman-robust.sh` counted every `.pkg.btr` in the cache (`-ge N`), so leftovers from
earlier runs could pass checks 1-5; it now counts each target as a cache file or a package file,
expects the ones pacman's debug log names as smaller package files, and removes both forms
between checks. Its "fetch-ids ran as alpm" check sampled `ps` every 50 ms and missed the now
shorter fetch-ids runs (2 of 3 runs failed); the helper is now a wrapper that logs its user and
arguments. 12/12 with pacman-btrpkg 11 (10 cache files), the dev build and pacman-btrpkg 12 from
CI (9 + 1, three runs); `pacman-gaps-test.sh gliff` 16/16, `plan-count.sh` 2/1/1/1.

## The index is NAME.pkg.btr next to the package file (2026-11-22)

Repositories keep each package's index beside its package file instead of in `index/`:

```
x86_64/
├── omarchy.db                                 btrpkg.index=gliff-0.4.3-1-x86_64.pkg.btr
├── gliff-0.4.3-1-x86_64.pkg.tar.zst (+ .sig)  the package file (fallback, stock pacman)
├── gliff-0.4.3-1-x86_64.pkg.btr     (+ .sig)  the index (was index/….btridx)
└── gliff-0.4.3-1-x86_64.pkg.btr.src           mirrors: sha256 of the package file it came from
btrparts/                                      unchanged
```

- The index is a `.pkg.btr` without its data: a cache file starts with exactly the same bytes
  (checked: 26,007 bytes of omarchy-local-ai on p14s) and has the same `.sig`. The client fetches
  NAME.pkg.btr and fills the data in under the same name. No format change: index bytes, IDs and
  signatures are the same.
- `mirror/migrate-sibling-index.py` moved the 11 live repositories (hard links, the database
  rewritten atomically under the mirror's `.lock`, then `index/` removed): 5,931 files, 1,958
  database entries, a few seconds in total. gc finds repositories by their `*.pkg.btr` files; its
  next pass kept the same 1,749 + 161 indexes.
- libalpm patch 10 takes only `btrpkg.index=NAME.pkg.btr`. Clients before pacman-btrpkg 9 see the
  new field as unusable and download the package files instead (that is the fallback path, so
  nothing breaks).
- Tested with pacman-btrpkg 9 / btrpkg r72 from `[btrpkg]`: x200, t460s, t490 download as btrpkg;
  macmini-m1's 5 packages in scratch roots identical to stock; p14s robust 12/12, option tests
  16/16, and omarchy-local-ai upgraded to 0.10.0 through btrpkg (97 files, 0 altered). The option
  test now takes a package NAME (its current version), since upstream releases moved 0.9.0 on.
- A btrpkg repository must not be a pacman CacheDir: its thin NAME.pkg.btr files have the cache
  files' names. (The ISO's btrpkg build uses its mirror as a Server only.)
- pacman-btrpkg 8 was built without patch 10: the PKGBUILD applied `btrpkg-000*.patch`, which
  doesn't match `0010`. Version 9 applies `btrpkg-[0-9][0-9][0-9][0-9]-*.patch`.

## Cleanup pass: bugs found, duplicated code removed (2026-11-22)

Four reviews (reuse, quality, efficiency, altitude) of the Rust code, the libalpm patches and the
scripts. Applied: the bugs and the safe duplication fixes; format and CLI unchanged.

- **Planned twice** (libalpm patch 9): `_alpm_sync_load` ran `btr_plan` again after
  `_alpm_sync_prepare`, so every upgrade ran `fetch-ids --index-only` and `pull-ids --plan` (which
  reads and hashes all reusable local data) twice per repository. `bench/plan-count.sh` (a
  logging BtrPkgHelper, one repository) on p14s: -6 ran `fetch-ids --index-only` 3 times and
  `pull-ids --plan` 2 times; -7 runs them 2 and 1 times (the second `--index-only` is the download step's, which finds
  the checked indexes already in DIR).
- **net::get** ignored curl's own error: DNS/TLS failures showed as "HTTP 0", and a body cut off
  mid-transfer (with status 200) would have been accepted.
- **gc** skipped directory entries it couldn't read on the marking side; their btrparts would
  have been swept. Now it stops before removing anything.
- **repo-mirror.sh** marked a pass done even when downloads failed, so they were retried only
  when upstream changed. **publish-release-repo.sh** treated a failed db download like "no db
  yet": it would have rebuilt `[btrpkg]` from one build's packages and deleted the others.
- Tests that couldn't fail: the no-temp-file checks used the wrong name, and the robust test
  counted `download-btrpkg/chunks` (it is `btrparts` since the rename).
- Duplication: `upgrade.rs` had its own db reader, desc parser and a `vercmp` process per
  package; it now uses `repo::read_db`/`descs`/`Desc` and `cache::vercmp`. Shared helpers in
  `store.rs` (`part_rel`, `index_rel`/`index_name`, `split_stem`, `syncfs_dir`,
  `PullOpts::default`) replace about 20 hand-written copies; `parse_name_id` replaces the copied
  NAME=IDHEX check in pull-ids and fetch-ids.
- p14s with btrpkg-git r69 and pacman-btrpkg 7 from `[btrpkg]`: robust 12/12, option tests 16/16.
  The option test now downloads its input (as btrpkg) when another test's cleanup removed it.
- Left for later (each its own task): one multi-server source for fetch-ids, pull-ids and
  pacbtr (server fallback is written three ways today); committing per package so one bad
  package doesn't send its whole repository to package files; splitting `store::pull_many` and
  `upgrade()`; one fork/exec helper in sync.c; fetching indexes over one connection.

## Range requests, fallback to package files, all clients on [btrpkg] (2026-11-22)

- **Raw btrparts without their trailing zeros.** A raw btrpart at a file's end is padded with
  zeros to 4 KiB. The index says where the file data ends (`store::data_len`), so the client asks
  for just that (`Range: bytes=0-N`), puts the zeros back and checks the SHA-256 (the btrpart's
  name) as before. A server that ignores Range answers 200 with everything, which is accepted.
  No format change.
  - linux-firmware-intel from the mirror's Caddy: 450 of its 1,378 btrparts came as 206, and
    0.91 MB fewer bytes were sent (0.6%).
  - From `python -m http.server`, which ignores Range: all 200, and the cache file was
    byte-identical.
  - Mirror-wide, the zeros in raw tails are 65 MB of 8.38 GB (0.77%).
- **Fallback to the package file** (libalpm patch 8, pacman-btrpkg 6).
  - A package no server has an index for (its db is older than the servers' current + previous)
    comes as its `.pkg.tar.*`, checked by sha256sum and OpenPGP like any package. The index step
    runs `fetch-ids --index-only --skip-missing`, which leaves the package out instead of failing.
  - If a repository's btrpkg download fails altogether, all its packages come as package files.
    Each attempt is all or nothing, so no cache file is ever half written.
  - Planning already does this, so the transaction summary shows the real sizes. Only a broken
    download directory still fails the transaction.
  - p14s, `bench/pacman-robust.sh` 12/12 with three new cases: one package's index missing on the
    only server (it comes as its package file, the other 9 as btrpkg); no btrparts at all (all 10
    as package files); the first case again with DisableSandbox (pacstrap's way). The option
    tests: 16/16.
- **All clients on [btrpkg].** x200, t460s, t490 and macmini-m1 now have the `[btrpkg]` section
  and upgraded with pacman to the CI builds (btrpkg-git r67, pacman-btrpkg 6), as p14s did.
  - `packaging/use-btrpkg-repo.sh` refreshes only `btrpkg.db`. m1's other sync dbs stay at Nov
    12/14 (checked), so nothing else changes.
  - Downloads checked on all five. On m1, 5 packages from omarchy, core, extra and alarm in
    scratch roots came out identical to stock; bankstown now comes as btrpkg too.

## Packages from CI: the `[btrpkg]` repository (2026-11-22)

Every push to master builds and tests btrpkg-git and pacman-btrpkg for x86_64 and aarch64
(`.github/workflows/packages.yml`) and publishes them as a pacman repository on GitHub releases
(one release per architecture, tag = `$arch`, the way asahi-alarm serves its repository):

```
[btrpkg]
SigLevel = Optional
Server = https://github.com/hegjon/btrpkg/releases/download/$arch
```

- **x86_64:** `archlinux:latest` on ubuntu-24.04, about 2 min.
- **aarch64:** an Arch Linux ARM container (`packaging/alarm-image.sh`: the official rootfs, its
  signature checked against `mirror/keyrings/archlinuxarm.gpg`) on GitHub's arm64 runner, about
  3 min.
- Both build against the mirrors Omarchy machines use (`mirror.omarchy.org`,
  `arm-mirror.omarchy.org`), so the libraries match the machines (checked: openssl 4.1.0,
  libarchive 3.8.11, curl 8.23.0, gpgme 2.2.0, glibc 2.44 / 2.43 as on p14s and macmini-m1).
- **Tests on both architectures:** btrpkg 21/21; pacman's suite 339 ok, 6 expected failures,
  0 failed, btrpkg's 73 subtests included. CI has fakechroot, so this covers the 19 tests m1
  couldn't run. The runners have no btrfs and making one would need mkfs, so the two
  btrfs-only btrpkg tests skip there with a note. They run in local builds (container on btrfs,
  m1).
- **Publishing** (`packaging/publish-release-repo.sh`): only package files whose name isn't in the
  db yet are added. pacman-btrpkg with the same version is not replaced, since clients may
  have that file cached. The db, the files db and the new packages are uploaded. Package files
  that neither the new nor the previous db names are deleted, so current + previous stay. The
  second run showed it: btrpkg-git r63 added, r62 kept, pacman-btrpkg unchanged.
- **p14s** has `[btrpkg]` in its pacman.conf and upgrades with pacman like any package: r55 → r62
  → r63, `Packager: btrpkg CI <…/actions/runs/45001738351>`. With the CI packages the option
  tests pass 16/16 and downloads through the shared btrparts directories work.
- **Not signed yet**, hence `SigLevel = Optional`: the packages come over HTTPS from the repo's
  releases, but there is no signature. That waits for the signing-key decision. Then the key
  goes into an Actions secret, or signing moves to a machine that keeps the key.
- Locally: `ARCH=x86_64|aarch64 packaging/build.sh`. aarch64 on an x86_64 host runs through qemu
  and took 9m33s; the container needs `DisableSandbox`, since qemu-user has no Landlock or user
  switch.

## aarch64 beside x86_64 (2026-11-22)

The mirror now has `omarchy-mirror/aarch64` (Omarchy's edge aarch64 repo) and
`arch-mirror/{core,extra}/os/aarch64` (Arch Linux ARM's repos, which Omarchy's aarch64 machines
use). They share `omarchy-mirror/btrparts` and `arch-mirror/btrparts` with x86_64. The wanted
lists are the same as for x86_64, plus what 404s teach.

| | aarch64 indexes | aarch64 btrparts alone | already there from x86_64 | added |
|---|---|---|---|---|
| omarchy | 58 | 1.11 GB | 0.45 GB (40.5%) | 0.66 GB (1.50 → 2.16 GB) |
| core + extra | 200 + 618 | 2.43 GB | 0.80 GB (32.8%) | 1.63 GB (2.67 → 4.30 GB) |
| total | 876 | 3.54 GB | 1.25 GB (35.3%) | 2.29 GB |

The first conversion pass took 3m29s (8 jobs; most package files were already downloaded).

- **Client test on macmini-m1** (aarch64, Omarchy on Arch Linux ARM + Asahi, btrfs). btrpkg-git and
  pacman-btrpkg were built on the machine. Its sync dbs are older than the ARM mirror, so system
  packages couldn't be added without a full upgrade; `packaging/build-local-toolchain.sh` puts
  rustup and meson/ninja in one directory and builds with `makepkg --nodeps`. btrpkg's tests:
  21/21. pacman's suite: 320 ok, 6 expected failures, btrpkg's 73 subtests ok. The 19 hook,
  scriptlet and ldconfig tests failed: they need fakechroot to chroot as a user.
- `bench/scratch-root-compare.sh` installs the same 15 packages (hyprland, omarchy-dev, mise-bin,
  mesa, ffmpeg, noto-fonts-cjk, util-linux, …) into two scratch roots: stock pacman from the
  mirror's `.pkg.tar.*`, pacman-btrpkg from the shared btrparts. Result: 6,063 package-owned files
  and the local db identical, `-Qkk` 0 altered. One round: stock 14.9 s, btrpkg 5.4 s. That
  includes the downloads, and stock also has to decompress Arch Linux ARM's xz.
- **Encoded extents and reflinks on aarch64** (m1, btrfs mounted `compress=zstd:3`; 7 packages,
  610 MB unencoded; compsize built on the machine):

  | | stock pacman | pacman-btrpkg |
  |---|---|---|
  | root on disk | 542 MB (515 MB stored uncompressed; the kernel's heuristic gave up on it) | 425 MB (501 MB as encoded zstd extents) |
  | root + package cache | 887 MB | 428 MB (the `.pkg.btr` cache shares its extents with the root) |

- The first attempt failed as it should. m1 didn't have the mirror's signify key, so every index
  was refused ("signed with unknown key"). A client needs `/etc/btrpkg/keys/<key>.pub`, as the
  x86_64 clients have.
- **Found along the way:**
  - Arch Linux ARM's db has descs without `%BASE%`, with the fields in another order.
    `repo-mirror.sh` now pairs `%NAME%` and `%FILENAME%` per desc.
  - Omarchy publishes `obsidian-1.15.1-1-aarch64` without a `.sig`. It stays a plain package.
    `repo-update --keep-going` logs such packages and still publishes the db, so one unsigned
    package no longer fails the whole pass (and with it the cleanup).
  - Arch Linux ARM packages are signed by archlinuxarm-keyring's keys, so
    `mirror/keyrings/archlinuxarm.gpg` holds a copy of macmini-m1's.
  - btrpkg's `check()` runs the tests in `$srcdir`, because they need btrfs and `/tmp` is often
    tmpfs.
- **alarm and asahi-alarm (later the same day).** Arch Linux ARM's `alarm` and `asahi-alarm` are
  now mirrored too, under `arch-mirror/{alarm,asahi-alarm}/os/aarch64`. They share
  `arch-mirror/btrparts` with core and extra, so one `Server = …/arch-mirror/$repo/os/$arch`
  serves all four aarch64 Arch repos. asahi-alarm comes straight from its GitHub releases
  (`repo-mirror.sh` follows redirects now); its key is `mirror/keyrings/asahi-alarm.gpg`.
  - **Wanted lists:** the aarch64 repos have their own (`*-aarch64.wanted`, `alarm.wanted`,
    `asahi-alarm.wanted`). Each is the x86_64 list plus everything macmini-m1 and galaxybook have
    installed from that repo.
  - **Coverage:** 992 of the 993 packages those two machines have installed from omarchy, core,
    extra, alarm and asahi-alarm have btrpkg data. The one left is `ffmpeg`, which Omarchy's
    current aarch64 repo no longer has; extra's is covered.
  - **Growth:** omarchy-mirror/btrparts 2.16 → 2.76 GB, arch-mirror/btrparts 4.30 → 5.61 GB. The
    aurora kernel, mesa and m1n1 builds account for most of Omarchy's 0.60 GB.
  - **m1 client test:** 16 packages (linux-aurora + headers, mesa-aurora, m1n1-aurora,
    uboot-aurora, all 9 asahi-alarm packages, libpisp) in scratch roots: 18,810 package-owned
    files identical to stock, `-Qkk` 0 altered.
  - **Packages in both repos.** In that test, 5 came plain: bankstown, lzfse, speakersafetyd,
    triforce-lv2 and widevine are in both Omarchy's aarch64 repo and asahi-alarm. pacman takes
    Omarchy's, since that repo comes first. m1 still had asahi-alarm's builds from an older sync,
    so the generated list had them under asahi-alarm only. The 404s taught the mirror three of
    them; bankstown and lzfse had been served as plain files, so no 404 would ever teach those.
    All asahi-alarm names are now in the Omarchy aarch64 list as well, and all five have btrpkg
    data. Lesson: a list built from installed packages has to follow pacman's repo order, not
    the repo a machine happened to install from.
  - `bench/scratch-root-compare.sh` now has the second servers real clients have (pacoloco,
    GitHub). The mirror's db lists all of upstream's packages but it only stores the ones it
    converts; a 404 sends pacman to the next server and teaches the mirror.

## Cleanup: `btrpkg gc` (2026-11-22)

`btrpkg gc [--keep N] [--min-age SECS] [--dry-run] BTRPARTS` removes old package versions from
every repository that shares a btrparts directory, then removes the btrparts no kept index names.

- **What stays.** Per package name and repository: the version the database names, anything
  newer (published, db not swapped yet), and the N-1 newest older versions. The default N=2 means
  current + previous. A package gone from the database keeps its N-1 newest versions. A removed
  version loses its index, `.sig`, `.src`, and on mirrors its `.pkg.tar.*` and `.sig`.
- **Shared directory.** The repositories are found under the btrparts directory's parent (any
  directory with `index/` and one `REPO.db`). Every one of them is marked. A btrpart goes only when
  no kept index in any of them names it. A repository whose db points its entries somewhere else
  is marked but not pruned. Raw map entries (`.raw/`) leading to a removed btrpart go too.
- **Safety.**
  - It holds every repository's `.lock`; a running mirror pass makes it stop.
  - Nothing younger than an hour is removed.
  - An unreadable index stops the run before anything is deleted.
  - Indexes go before btrparts, so an interrupted run leaves extra btrparts, never an index
    without its btrparts.
- **Mirror.** `mirror-all.sh` runs it once per btrparts directory after all repositories, unless
  a repository failed in that pass. `GC_KEEP` and `NO_GC` override.
- **First run** (omarchy-mirror): 14 old versions of `omarchy-dev` and `omarchy-settings-dev`
  went (git builds r8121–r8143; r8160 and r8147 stay), with their package files and 649 btrparts:
  593 MiB, 3,589 → 2,999 MiB. Afterwards 0 of the 42,812 btrpart references were missing, and
  pacman-btrpkg on p14s downloaded the current versions. It takes 0.16 s there and 0.68 s for
  arch-mirror's 829 indexes and 89,689 btrparts (nothing old there yet).
- A client whose database is older than the previous version gets no index. pacman-btrpkg then
  fails the transaction instead of falling back to the `.pkg.tar.zst` from another server. That is
  the same outcome as a stale db against an Arch mirror (404), but the fallback would be better.

## HTTP/1.1 vs HTTP/2 vs HTTP/3 (2026-11-21)

`bench/proto-bench.sh`: the 10 upgrades of the lab runs, downloaded three ways per protocol:
btrpkg with reuse from the installed versions (a real upgrade), btrpkg without reuse (every
chunk, 332 MB in 3,846 requests), and the `.pkg.tar.zst` files with `curl --parallel
--parallel-max 5` (pacman's `ParallelDownloads = 5`, 335 MB in 10 requests). Server: caddy 2.11
on the desktop, plain http (h1, h2c) and https (h1, h2, h3 over QUIC; caddy's internal CA).
Client: libcurl 8.23 (nghttp2, ngtcp2, OpenSSL 4.1). Medians of 3 rounds, in seconds; the
access log confirmed each protocol. btrpkg: 8 connections over HTTP/1.1, 64 requests in flight
over HTTP/2 and 3.

| link | protocol | btrpkg, real upgrade | btrpkg, every chunk | .pkg.tar.zst |
|---|---|---:|---:|---:|
| p14s wired, 0.4 ms RTT | http/1.1 · h2c · https/1.1 · h2 · h3 | 1.03 · 1.03 · 1.07 · 1.06 · 1.09 | 3.08 · 3.09 · 3.12 · 3.14 · 3.21 | 2.87 · 2.87 · 2.88 · 2.88 · 2.93 |
| p14s + 30 ms (netem) | same | 9.11 · 3.26 · 9.75 · 3.92 · 3.31 | 21.57 · 5.97 · 21.92 · 6.62 · 17.96 | 3.35 · 3.95 · 3.44 · 3.96 · 4.68 |
| t460s Wi-Fi, 1.8 ms | same | 4.94 · 3.89 · 5.14 · 4.05 · 8.26 | 7.23 · 5.03 · 7.42 · 5.25 · 13.90 | 4.48 · 4.36 · 4.47 · 4.40 · 12.05 |
| x200 Wi-Fi, 3.0 ms | same | 17.40 · 16.46 · 17.08 · 15.93 · 21.07 | 22.42 · 21.15 · 22.64 · 21.98 · 31.10 | 19.63 · 20.31 · 20.04 · 19.88 · 28.34 |

- At LAN latency the protocol doesn't matter (p14s), or the link does (x200 at ~16 MB/s).
- With internet-like latency (30 ms) btrpkg needs a multiplexing protocol: over HTTP/1.1 its
  thousands of chunk requests take 2.8-3.6x longer than over HTTP/2. Whole packages don't care
  (few large requests; HTTP/1.1 is even a little faster). A btrpkg mirror must speak HTTP/2.
- HTTP/3 (quic-go server, ngtcp2 client) is the slowest wherever there is much to download:
  every chunk at 30 ms 18.0 s vs 6.6 s for HTTP/2 over TLS, and 2-3x slower on Wi-Fi even for
  whole packages; it also costs the most CPU (x200: 20 s CPU for the full download vs 13 s for
  h2). With more requests in flight it improves but stays behind (30 ms, every chunk: h3
  26.3/20.2/18.3/12.6 s at 8/32/64/128 in flight, h2 19.7/7.1/6.6/5.7 s), so it is the QUIC
  connection's throughput, not request concurrency. HTTP/2 stays btrpkg's default.
- The Wi-Fi links have only 2-3 ms RTT: they show bandwidth and loss, not internet latency;
  netem's delay is clean (no loss or reordering).
- HTTP/2 with 128 in flight beat 64 at 30 ms (5.7 vs 6.6 s): worth raising the default for
  far-away mirrors.

## Shared btrparts directory beside the architectures (2026-11-21)

A btrpart is one entry of an index's table: up to 128 KiB of one file's data, stored the way
btrfs stores an extent (a zstd frame it takes as an *encoded extent*, or raw). Btrparts were
called chunks before and were stored in each repository's `chunks/`. Btrparts are named by their
content, so x86_64 and aarch64 can share them (38% of the aarch64 btrpart bytes are already in
the x86_64 stores; see the next section). They now live in one directory beside the architecture
directories:

```
omarchy-mirror/
├── btrparts/xx/HASH           shared by x86_64/ and (later) aarch64/
│   └── .raw/xx/HASH           raw map, build server only (dot directory, caddy hides it)
└── x86_64/
    ├── omarchy.db             btrpkg.parts=../btrparts in every btrpkg entry
    └── index/NAME.btridx(.sig)
arch-mirror/
├── btrparts/                  shared by core/os/x86_64 and extra/os/x86_64
├── core/os/x86_64/core.db     btrpkg.parts=../../../btrparts
└── extra/os/x86_64/extra.db   btrpkg.parts=../../../btrparts
```

- Indexes stay per architecture and repository. The database names where the btrparts are, as a
  path relative to the repository's URL (`btrpkg.parts`). pacman `Server = …/$repo/os/$arch`
  lines keep working, and so do mirrors that copy the tree. The format and the signed index are
  unchanged. Btrparts are checked against the index that names them, so the path is no matter of
  trust. It has to be a plain relative path: `..` can't go above the host. Otherwise pacman
  ignores that package's btrpkg data and downloads the `.pkg.tar.zst`.
- `repo-update --parts REL`: default `../btrparts`. The ISO's offline repository uses
  `btrparts`, inside it, because it carries one repository. `publish`, `pull`, `pull-ids` and
  `fetch-ids` take `--parts` too; their default is `btrparts`. libalpm patch 7 groups downloads
  per repository and btrparts path and passes `--parts`. pacman's download directory has
  `btrparts/` inside it.
- `mirror/migrate-btrparts.py` moved the live stores without a new conversion:

  | | btrparts moved | already in the shared dir | db entries with btrpkg.parts |
  |---|---|---|---|
  | omarchy-mirror/x86_64 | 38,343 | 0 | 58/58 |
  | arch-mirror/core | 23,089 | 0 | 201/201 |
  | arch-mirror/extra | 66,600 | 14 (from core) | 627/627 |
  | omarchy, btrtest, btrtest2 (test repos) | 23,236 / 935 / 6 | 0 | 51 / 9 / 1 |

  Every repository checked out: 0 missing among the btrparts its indexes name (48,104 references
  for omarchy-mirror, 23,176 core, 66,949 extra). No packages were converted again
  (`converted: 0`). The mirror timer's next pass found nothing new to convert.
- The move left compat symlinks for the transition: `chunks -> ../btrparts` for pkgrel ≤4 clients
  (pacman-btrpkg -4 on p14s installed through it, 0 altered files) and `btrparts -> ../btrparts`
  for new clients with a db synced before the move. After every client was upgraded, both were
  removed (2026-11-21). The clients were p14s and x200, t460s and t490, which still had the
  iteration-1 dev builds in `/usr/local/bin` and `/opt/pacbtr`; those were replaced by btrpkg-git
  r55 and pacman-btrpkg -5. `bench/btrparts-client-check.sh` (throwaway dbpath and cache, `-Sw`)
  then downloaded through the database's path on all four: the test repo (`../btrparts`) on x200,
  t460s and t490, and the omarchy, core and extra mirrors on p14s (`../btrparts`,
  `../../../btrparts`). Every btrpart request went to the shared directories
  (`bench/results/lab-20261121-btrparts/client-check*.txt`).
- With an aarch64 directory beside x86_64, the measured overlap means it would add 2.04 GB
  instead of 3.29 GB of btrparts.

## Sharing btrparts between x86_64 and aarch64 (2026-11-21)

Could one store serve both architectures? Measured with the aarch64 builds of the packages in the
x86_64 mirrors: 849 of the 855 have one at the same version (Arch Linux ARM for core/extra,
Omarchy's aarch64 repository); 848 downloaded (one 404). They were converted through the same
pipeline (omarchy-iso's `index-offline-mirror.sh`, `btrpkg-offline-mirror.sh convert`, i.e.
`repo-update`), and every aarch64 chunk was looked up in the x86_64 stores' raw maps
(`bench/cross-arch-share.py`).

| aarch64 packages from | packages | chunks | already in the x86_64 stores |
|---|---|---|---|
| Arch Linux ARM core | 182 | 710 MB | 99 MB (14.0%) |
| Arch Linux ARM extra | 609 | 1,571 MB | 684 MB (43.5%) |
| Omarchy aarch64 | 57 | 1,005 MB | 466 MB (46.3%) |
| all | 848 | 3,285 MB | 1,249 MB (38.0%) |

- **It works without any change to the format.** Chunks are cut per file from the file's start
  and named by content, so identical files give identical chunks, and here every shared chunk was
  byte-identical too (the same zstd version and settings on both sides), not only the same
  unencoded data. A combined store would add 2.04 GB for aarch64 instead of 3.29 GB.
- **Not one package file is shared.** Arch Linux ARM rebuilds even `any` packages
  (`noto-fonts` comes as `.pkg.tar.xz` with `%ARCH% aarch64`), so a store of whole package files
  shares nothing; the contents still match (noto-fonts-cjk 100%, Omarchy's themes 100%,
  omarchy-dev 99.9%, libime 97.7%).
- **Firmware shares nothing**: `linux-firmware*` (474 MB, most of core's share) ships its files
  zstd-compressed on Arch (`PS_ASIC.pst.zst`, 56 MB for atheros) and uncompressed on Arch Linux ARM
  (`PS_ASIC.pst`, 148 MB): the same firmware, different files. Without it, 44.4% is shared.
- **Binaries don't share**, as expected: google-chrome 9.7%, webkit2gtk 2.1%, mise-bin 0.2%. What
  is shared is architecture-independent data inside architecture-specific packages (libreoffice
  16.7%, qt6-webengine 22.4%, hyprland 90.6%).
- `btrpkg convert` read only zstd, and Arch Linux ARM ships `.pkg.tar.xz`: it now reads xz and
  gzip too, by magic bytes (`liblzma`; btrpkg-git depends on `xz`). Spot checks: mesa, python and
  noto-fonts installed from the converted aarch64 packages are identical to bsdtar's extraction.

## Omarchy ISO with the btrpkg format (prepared, not built yet)

omarchy-iso branch `btrpkg` (local, in `~/Work/omarchy-iso-btrpkg`, on `quattro` `12a5daf`):
`bin/omarchy-iso-make --btrpkg DIR` with DIR holding one btrpkg-git and one pacman-btrpkg
package.

- **Offline mirror.** `builder/btrpkg-offline-mirror.sh convert` turns the build's `.pkg.tar.*`
  mirror into a btrpkg repository (`repo-update --no-gpg`: pacman -Syw verified each package's
  signature when it downloaded it). The ISO ships the whole repo db, plus indexes and chunks for
  the packages the root image lacks (`ship`), instead of their `.pkg.tar.*` files. The medium's
  checksum list covers them. No btrpkg signing key: the offline repo is `SigLevel = Never`, as
  today; pacman-btrpkg checks each index against the header ID in the db, and the ISO's
  signature covers both.
- **Root image.** `pacstrap-btrpkg` from that mirror, with the CacheDir on the image's own btrfs
  next to the root subvolume, so files are reflinks of the packages' zstd frames (encoded
  extents) and the image keeps that compression once the cache is deleted.
- **Live system and image.** pacman-btrpkg and btrpkg are installed in both; `pacman` (and in the
  live system `pacstrap`, which archinstall runs by name) are the btrpkg ones; the offline
  config gets `BtrPkgHelper`. The installer skips its trick of bind-mounting the mirror as the
  target's cache in this mode: packages are assembled in the target's cache and reflinked.

Checked without an ISO: `bench/iso-mirror-test.sh` runs the same steps on p14s (convert, an
image with the cache on its own filesystem, ship a subset, per-machine pacstrap from it,
archinstall's flags): package-owned files and local db identical to stock, encoded extents kept
after the cache is deleted, 119 MB on disk vs 184 MB, a package outside the shipped mirror
refused cleanly. omarchy-iso's own unit tests pass (245 checks under `LC_ALL=C`; `nightly-test.sh`
fails in a UTF-8 locale on unmodified `quattro` too). Building it needs `mkfs` (the root image is
a loop btrfs), so `bench/iso-ab-build.sh` builds the stock and `--btrpkg` ISOs for a person to
run.

## pacman -U, -Sp and the validation record (2026-11-21)

Three gaps closed in libalpm patch 6, checked on p14s by `bench/pacman-gaps-test.sh` (16 checks;
the packaged pacman-btrpkg before the patch failed 8 of them):

- **`pacman -U NAME.pkg.btr`.** The install path already worked; the signature didn't: pacman
  handed the signify `.sig` beside the file to GPGME ("signature format error"), so a cache file
  with its signature couldn't be installed at all. Now the `.sig` is checked as signify against
  `/etc/btrpkg/keys` and `/usr/share/btrpkg/keys`, as `LocalFileSigLevel` asks (Optional
  installs an unsigned file, Required refuses it, a signature by another key is refused). The
  test also found a worse gap: **a `.pkg.btr` with changed data installed silently**, because
  libalpm checked the index's structure and only pull-ids ever checked chunk data. `-U` now
  hashes every chunk against the index first ("chunk 0: data does not match the index"), at
  about the cost of stock pacman decompressing a `.pkg.tar.zst` to load it.
- **`-Sp`.** pacman's `--print` sets `NOLOCK`; such a transaction no longer selects or plans
  btrpkg downloads. Before, as root, `-Sp` fetched indexes from the mirror and printed a
  `.pkg.btr` URL that no server has (a btrpkg server holds indexes and chunks); as a normal user
  it printed the stock URL. Now both print the repository's `.pkg.tar.zst` URL and size and make
  no requests (counted in the mirror's access log). A real `-S` still plans: 2 requests and the
  real size in the summary.
- **The validation record.** `%VALIDATION%` said `pgp` for a signify check. A new token would
  be the clean fix in pacman alone, but the system's stock pacman reads the same local database
  and warns ("unknown validation type") about every such package. So `%VALIDATION%` keeps
  `sha256` (the header-ID check is a SHA-256 over the header, which holds the hashes of
  everything else), and the signature goes into extended data, which stock pacman reads silently
  and `pacman -Qii` shows: `btrpkg-validation=signify:btrpkg-test.pub`. A `btrpkg-validation`
  entry in a package's own `.PKGINFO` is dropped, so a package can't claim it.

## Faster repository conversion (2026-11-21)

Converting the root image's packages for the mirrors took about 40 minutes for 828 packages
(core 4 minutes of conversion, extra 17.5) at about one busy core. Profiling one package
(noto-fonts-cjk, 198 MB) showed `convert` was already parallel: 1.9 s at 2061% CPU. `publish`
took 24.1 s at 4% CPU, writing its 2,399 chunk files and raw-map entries one by one, each with an
fsync, behind repo-update's publish lock.

`publish` now decides each chunk's fate in parallel (reuse through the raw map, identical bytes, or
new), writes the new files in parallel without a per-file fsync (each distinct file once), runs one
`syncfs`, and only then writes the signed index. The index is still never visible before the chunks
it names are on disk. That package: 24.1 s → 0.25 s.

A/B on the same input (core's 201 packages, a fresh copy, `repo-update --jobs 4`):

| | before | after |
|---|---|---|
| seconds | 239.0 | 18.2 |
| CPU | 119% | 1658% |

Results: the same 23,089 chunks and byte-identical indexes. `--jobs 8` gives 15.4 s (16 is no
better), now the mirror's default. A package pulled from the new store installs identically to its
`.pkg.tar.zst` extracted by bsdtar. A mirror pass with nothing new to convert costs 1.7 s even on
extra's 15,291-entry database.

## pacstrap-btrpkg: new systems from btrpkg packages (2026-11-21)

`pacstrap-btrpkg` (in `pacman-btrpkg`) is Arch's pacstrap, run unchanged, with pacman-btrpkg as
its pacman and `BtrPkgHelper` added to the `-C` config. It takes the same options. The ones that
mattered first are omarchy-iso's: `-C CONF -c -G -M ROOT PKGS` (root image and live system) and
archinstall's `-C CONF -K ROOT PKGS --noconfirm --needed`. Details in `packaging/README.md`.

What it needed:
- **Mirrors for Arch's core and extra.** `mirror/repos.conf` lists them next to Omarchy's repo;
  the wanted lists are the dependency closure of omarchy-iso's x86_64 edge root image: 878
  packages (201 core, 627 extra, 50 Omarchy), all converted.
- **libalpm patch 4.** A package file already in a cache directory is used as it is. Without it,
  omarchy-iso's offline mirror would have been bypassed for a download.
- **libalpm patch 5.** pacstrap always disables the download sandbox, and the two-process
  download then only cost a double write (download directory, then cache file). Without a
  download user, one root `pull-ids` per server now fetches straight into the cache files. On the
  first small test set that turned "slower than stock" into level, with less written.

Measured on p14s (wired gigabit to the desktop's mirrors; stock pacstrap fetches the same
`.pkg.tar.zst` files from the same mirror). The root image's 878 packages went into throwaway
subvolumes of p14s's own btrfs (`compress=zstd:3`), 3 rounds each, alternating, with
`bench/pacstrap-compare.sh`. Medians:

| | stock pacstrap | pacstrap-btrpkg |
|---|---|---|
| **omarchy-iso image flags** `-C -c -G -M`, empty CacheDir on the same filesystem | | |
| seconds | 105.96 | 93.78 |
| written to the disk | 9.63 GB | 5.65 GB |
| downloaded | 3.51 GB | 4.16 GB |
| new root on disk (compsize) | 5.1 GB | 4.0 GB |
| cache on disk | 3.1 GB | 3.7 GB, all but 328 MB shared with the root |
| **archinstall flags** `-C -K --needed`, cache inside the new root | | |
| seconds | 108.46 | 93.37 |
| written to the disk | 9.70 GB | 5.32 GB |
| downloaded | 3.51 GB | 4.15 GB |
| new root including its cache, on disk | 8.3 GB | 3.8 GB |

- **Correctness.** In all 6 pairs, every package-owned file was identical between the stock and
  btrpkg roots: type, mode, owner, size, symlink target, xattrs, SHA-256, and mtime (backup files
  such as `/etc/passwd`, which scriptlets edit after install, without mtime). The local databases
  were identical apart from install dates.
- **What still differs** is 15–16 files made by hooks: machine-id (random), ldconfig's aux cache,
  the Java keystore (has a timestamp), and fontconfig's caches (they embed directory mtimes).
  `icon-theme.cache` also differs in size; probably because its layout follows the order files
  were created in the directory, but I haven't verified that it's equivalent.
- **Why it's faster and writes less.** Large files are reflinked from the cache files, so data is
  written once, and the new root's extents are the packages' zstd-19 frames rather than files
  recompressed at the mount's level 3.
- **The cost is the download.** A fresh install has nothing local to reuse, and per-chunk frames
  compress worse than whole-archive zstd: 18% more bytes (0.64 GB) here. It saves 12–15 s, so the
  break-even link is about 45–55 MB/s. On a slower link, stock pacstrap finishes first, but
  pacstrap-btrpkg still writes 40% less and uses 22–54% less disk.
- **omarchy-iso's actual root-image build** points CacheDir at its offline mirror, which already
  holds every `.pkg.tar.zst`. pacstrap-btrpkg then installs those files and downloads nothing
  (patch 4), so the image build changes only if the offline mirror ships `.pkg.btr` files. The
  image is also its own loop-mounted filesystem, where reflinks can't reach. Keeping the
  compressed frames there would need encoded read and write across filesystems (not done; today
  it falls back to `copy_file_range`, and the image recompresses).

## End to end: an Omarchy mirror and `omarchy update` on p14s (2026-11-21)

**Mirror** (`mirror/`, details in `mirror/README.md`):
- A user timer on the desktop runs every 10 minutes and keeps a btrpkg mirror of Omarchy's edge
  repository: upstream's database plus btrpkg data for the packages the lab uses.
- Every new file is checked against the database's SHA-256 and against Omarchy's OpenPGP
  signature before the mirror converts it and signs its index, so the mirror never signs anything
  Omarchy didn't sign.
- Packages it hasn't converted keep their plain entries; pacman fetches those from the next
  server.
- It learns what to convert from the 404s in caddy's access log.
- First run: 51 packages in 4 min 11 s (895 MB of chunks). An unchanged upstream costs 0.2 s.
- caddy and the mirror now run as systemd user services. The server root also used to serve the
  test key's secret half; it and the mirror's private files are hidden now.

**p14s:**
- `[omarchy]` lists the mirror first, then pacoloco and pkgs.omarchy.org.
- `pacman-btrpkg` is installed, and `/usr/local/bin/pacman` points at it.

1. **Comparison first.** p14s's real pending upgrade was 51 packages (6 core, 37 extra including
   chromium, libreoffice-fresh, mesa and docker, 8 Omarchy). It was installed 3 times with stock
   pacman and 3 times with pacman-btrpkg, with a snapper reset between runs (pacoloco warm,
   p14s wired):

   | | stock pacman | pacman-btrpkg |
   |---|---|---|
   | seconds (median of 3) | 25.08 | 22.76 |
   | written to disk | 2.58 GB | 2.08 GB |

   Installed files and database entries were identical in all 3 rounds, and `-Qkk` was clean
   after every reset. The saving is smaller than in the Omarchy-only tests because 43 of the
   51 packages are Arch's and install the normal way in both runs.

2. **Direct `pacman-btrpkg -Syu`** is refused by Omarchy's guard hook ("Woah partner"). Omarchy
   wants upgrades to go through `omarchy update` (snapshot, keyrings, migrations, restarts).
   Nothing was installed.

3. **`omarchy update -y`**, run as the desktop user `a`. sudo needs a password on the lab, so a
   `NOPASSWD` drop-in was added for the run and removed right after.
   - Omarchy's flow made its own snapshot and ran `/usr/local/bin/pacman -Syu`, which is
     pacman-btrpkg. Its migrations then installed fantasi, omarchy-themes-loops and flowtunes and
     removed hyprcursor, also through pacman-btrpkg.
   - Exit 0, 57 package operations, 26 s (the files had already been downloaded by the refused
     attempt).
   - **All 8 Omarchy upgrades went through btrpkg** (`.pkg.btr` cache files), including versions
     newer than the lab test repo had (omarchy-dev r8121, omarchy-local-ai 0.9.0). The 3 new
     packages came as `.pkg.tar.zst` from Omarchy's servers through the fallback, and the mirror
     converted them on its next run.
   - Afterwards: `pacman -Dk` clean, `-Qkk` 0 altered over the 33,535 files of the upgraded
     packages, nothing pending.

4. **Reboot** through kvm2: disk unlocked, ssh up 10 s later, no failed units, Hypoland's
   autologin session running, and a screenshot from inside the session shows both monitors
   drawing normally.

Still open:
- An unattended daily update for the multi-day trial (step 6). That needs either a permanent
  passwordless sudo for `a` (so `omarchy update -y` runs as designed) or a root timer that
  bypasses Omarchy's flow.
- Arch's own repositories aren't mirrored.
- The mirror never deletes old files or chunks (step 7).

## Packages: `btrpkg-git` and `pacman-btrpkg` (2026-11-21)

Step 5. `packaging/build.sh` builds both packages in a clean Arch container from the PKGBUILDs in
`packaging/` (details: `packaging/README.md`):

- **`btrpkg-git`** installs `btrpkg` and `pacbtr`, plus a weekly `btrpkg-cache-clean.timer`
  (keeps 3 versions per package).
- **`pacman-btrpkg`** is Arch's pacman package (the same commit, Arch's 4 patches, the same
  options) plus the 3 btrpkg patches. It installs beside the system pacman:
  - `/usr/bin/pacman-btrpkg` runs `/usr/lib/pacman-btrpkg/pacman` with its own libalpm (RUNPATH).
  - Its default config is `/etc/pacman-btrpkg.conf`: `BtrPkgHelper` plus `Include = /etc/pacman.conf`.

pacman is untouched and shares the database, lock, cache, hooks and keyring with pacman-btrpkg.
Removing `pacman-btrpkg` leaves a stock system.

Checked:
- In the clean build container, pacman's whole test suite passes: 339 tests including btrpkg's
  73 subtests, 0 failures. The 19 failures seen on the desktop come from the desktop's
  environment. btrpkg's 18 tests also pass.
- On p14s, the system pacman and pacman-btrpkg load different libalpm files. Both report the same
  49 pending updates from the real repositories, and `pacman -Qkk` is clean for both packages.
- The e2e run with the packaged binaries (3 rounds) was identical to stock in every round:
  4.70 s against stock's 7.34 s.
- All 9 robustness checks pass.
- The timer's service, run through systemd, kept the newest 3 versions and removed the 2 older
  ones together with their `.sig` files.

Two build findings:
- zstd-sys has to link the system's libzstd. Otherwise it compiles its own copy with makepkg's
  `-flto` CFLAGS into GCC LTO objects, which Rust's default linker (lld) can't read.
- The btrpkg patches carry binary test fixtures, so they go in with `git apply`; `patch` can't
  apply them.

## Iteration 3: pacman's sandbox, mirrors, cache cleaning, real sizes (2026-11-21)

Steps 1-4 of the post-iteration-2 list: the parts a person other than us would trip over first.
pacman patch 3 (`libalpm/0003-*.patch`), btrpkg `fetch-ids`, `pull-ids --fetch-via/--plan/--consume`,
`cache-clean`.

**Sandbox.** The helper no longer runs as root with network access. A download is now two
processes:

- `btrpkg fetch-ids` (network): runs as pacman's `DownloadUser` (`alpm`) under pacman's seccomp
  filter and no-new-privileges, and puts itself under Landlock: read anywhere, write only to
  `CACHEDIR/download-btrpkg/`, execute nothing. It fetches indexes and chunks into that directory,
  each checked against its hash on arrival.
- `btrpkg pull-ids` (root, no network): checks the indexes against the database's IDs, reuses
  data from the installed versions and older cache files (read back and hashed, as before), lists
  the chunks still missing (in a root-owned file outside the download directory), prints
  `fetch N BYTES` and waits; libalpm runs `fetch-ids --want LIST` and says `go`; pull-ids reads the
  chunks (only via `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, regular files only,
  `O_NONBLOCK`), checks them again, writes `NAME.pkg.btr` and removes the directory.

Root never writes into the download user's directory, and a compromised download process can't
touch anything outside it (tested: a symlinked `index/` pointing outside is refused under
`--landlock`, written without it).

**Mirrors.** `CacheServer` entries first, then `Server`, in order. Every server is asked only for
what the previous ones didn't deliver, missing or damaged. **Resume:** an interrupted download keeps
its directory, so the next run fetches only what's missing. **`-Sc`/`-Scc`** already handled
`.pkg.btr` files (libalpm loads them as packages through btrstream), and they now also remove an
interrupted download's leftovers (`nftw` with `FTW_PHYS`, never following a symlink).
`paccache` only knows `*.pkg.tar*`, so `btrpkg cache-clean [--keep N] [--dry-run] CACHEDIR`
does the same for `.pkg.btr` (newest N per package by pacman's vercmp, ported line by line from
libalpm and tested on pacman's own vercmp cases).

**Real download size.** `_alpm_sync_prepare` fetches the indexes (as the download user) and runs
`pull-ids --plan`, which does the local checks and writes nothing. pacman's summary and its
download-space check show what will really be fetched. fetch-ids' progress lines become pacman's
download callbacks, so the usual progress bars and Total line appear. On p14s, for the 10 lab
targets:

| | before | now |
|---|---|---|
| Total Download Size shown | 311.26 MiB (the `.pkg.tar.zst` sizes) | 68.93 MiB |
| bytes received on the interface | 75 MB | 75 MB |

Checks on p14s (`bench/pacman-robust.sh`, `bench/two-phase-test.sh`, all passing):

- `fetch-ids` was seen only as `alpm`, `pull-ids` only as root.
- A dead `CacheServer` before the real server: same 75 MB, no error.
- Every chunk over 100 KB damaged on the first server: fetched from the next one, and the
  install was correct.
- Interrupted (SIGINT) after 36 MB: 435 chunks were kept, and the second run fetched 50 MB.
  That's 11 MB more than one clean run, because chunks still in flight are lost.
- `DisableSandbox`: works, running as root.
- `-Sc` removes the leftovers and keeps other files.
- A locally changed installed file: pull-ids lists the extra chunks it needs and they are
  fetched.

Speed, the same e2e run as iteration 2 (p14s wired, 10 targets with 7 install scripts,
3 rounds; installed files and database entries identical to stock in every round, `-Qkk` clean
after every reset):

| | stock pacman | iteration 2 (helper as root) | sandboxed | sandboxed + `--plan` |
|---|---|---|---|---|
| p14s, median | 7.36 s | 4.18 s | 4.48 s | 4.71 s |

The sandbox costs about 0.3 s (three helper runs instead of one). The plan step costs about
0.25 s, because the local data is read and hashed twice: once for the plan and once for the real
assembly. Logs: `bench/results/lab-20261121-i3/`.

Known gaps:
- ~~The plan is run for `-Sp` and `-Sw` too~~: `-Sp` no longer touches btrpkg (see "pacman -U,
  -Sp and the validation record" below); `-Sw` keeps the plan, whose index fetch the download
  reuses.
- Resume is per chunk, not per byte.
- ~~`pacman -U NAME.pkg.btr` still isn't supported~~, ~~the validation is still recorded as
  `pgp`~~: both done, below.

## Iteration 2: pacman installs btrpkg packages (2026-11-21)

Two libalpm patches (`libalpm/`, pacman branch `btrpkg` on `54d94116`, Arch's 7.1.0.r9): an
index and signature reader in C that agrees with this repo's reader on every real, crafted and
fuzzed index tried, and the sync path: with `BtrPkgHelper = /usr/bin/btrpkg` in pacman.conf and
the cache on btrfs, packages with btrpkg data in the database come as `NAME.pkg.btr` (`btrpkg
pull-ids`, one download queue per repository, data reused from the installed versions), are
checked by header ID and signify signature, are read through libalpm's normal archive code as a
tar stream made from the index, and have their large files reflinked from the cache file. So
install scripts, backup files and `.pacnew`, NoExtract, hooks and the local database all work as
in stock pacman; `pacbtr` had to leave packages with install scripts or backup files to pacman.

### Stock pacman, patched pacman and pacbtr on the lab

`bench/lab-pacman-e2e.sh`: `pacman -S` of the same targets with stock pacman, with the patched
pacman (`/opt/pacbtr`), and with `pacbtr --no-snapshot`; 3 rounds in rotating order, reset with
`snapper undochange` between runs; same test repo and ssh tunnels as iteration 1. Medians; writes
on the root's partition below LUKS2. After every run a manifest of every installed file of the
targets (type, mode, owner, size, mtime, xattrs, sha256) and their local database entries were
compared with stock pacman's: identical in all 39 btrpkg runs. `pacman -Qkk` clean in all.

| host | targets | stock pacman | patched pacman | pacbtr | written stock / patched |
|---|---|---:|---:|---:|---:|
| p14s | 5 upgrades | 5.15 s | 2.07 s | 2.09 s | 685 / 113 MB |
| p14s | 10 (6 upgrades, 4 reinstalls; 7 with install scripts) | 7.35 s | 4.18 s | (can't) | 726 / 130 MB |
| t460s | 10 upgrades | 9.02 s | 5.83 s | 5.35 s | 729 / 213 MB |
| x200 | 10 upgrades | 35.00 s | 21.12 s | 20.38 s | 758 / 212 MB |

The patched pacman is as fast as `pacbtr` on p14s and 0.5-0.7 s slower on the older machines
(pacman's own work around the same download and placement), 1.6-2.5x faster than stock pacman
with a fifth to a third of its writes, and it handles the packages `pacbtr` couldn't.

Backup files, checked with `bench/make-e2e-pkgs.sh`'s package (built by hand as root): a 200 KB
backup file with a local change becomes `.pacnew` with both pacmans, without a local change it
is replaced by both; a 300 KB binary keeps its `security.capability` after the reflink; hardlink,
symlink and install-script output match.

### Client speed (same format)

`pull_many`: all packages of an upgrade share one download queue, and reused data is read back
and hashed in parallel. A/B against iteration 1's client in the same runs: `pull` t460s 4.86 →
3.69 s, x200 17.9 → 16.0 s (2 cores, no SHA or AES instructions, which also decrypt the tunnel).

### Index readers agree

`btrpkg dump` and libalpm's reader print identical text for 384 real indexes; 63 crafted indexes
(valid hashes, bad contents: unsafe paths, gaps, bad hardlinks, oversized headers,
`bench/craft_indexes.py`) and 9,000 fuzzed ones (`bench/fuzz_entries.py`) get the same verdict
from both. That found one ambiguity: the Rust reader ignored data after the first zstd frame of
the entries or blob section; both readers refuse it now (no published index had any). ASan/UBSan
found two bugs in the C reader's error paths, fixed before the first commit.

## Iteration 1: repo, client and upgrades on the lab (2026-11-21)

`PLAN.md` has the milestones. What exists now:
- `btrpkg repo-update DIR REPO`: checks each package's OpenPGP signature (`gpgv`, pacman's
  keyring) and its sha256 against the database, converts, publishes chunks, a signify signature
  and the index, then rewrites the database with `btrpkg.index=`, `btrpkg.id=`, `btrpkg.size=` in
  `%DATA%`. Plain pacman reads the result unchanged (`pacman -Si` byte-identical for all 51 test
  packages, signed installs work).
- `btrpkg pull DIR|URL`: libcurl multi, HTTP/2 (64 in flight) or HTTP/1.1 (8 connections). Each
  chunk comes from an older cache file of the same package, then from the installed file of the
  same path (read back, checked against the unencoded hash, whole pieces reflinked), then the
  server.
- `btrpkg upgrade` / `pacbtr`: targets are installed packages whose sync entry has btrpkg data and
  no `.INSTALL` or backup files; indexes must match the database's ID and carry a signature from
  `/etc/btrpkg/keys`; `pacman -U --print` of metadata-only packages checks dependencies and
  conflicts; file conflicts against the local database; pull; `syncfs`; snapper snapshot; place;
  remove the old versions' files; `pacman -U --dbonly` (hooks run there). On failure:
  `snapper undochange` and the snapshot's mtimes put back (undochange alone stamps restored files
  with the current time and `pacman -Qkk` reports them).

### Upgrades: pacman against btrpkg on four lab machines

`bench/lab-upgrade.sh`, 3 rounds, alternating order, reset between runs with a baseline snapper
snapshot (`snapper undochange`, then `bench/fix-mtimes.py`). Real pending upgrades from the
Omarchy edge test repo (51 packages, `~/Work/btrpkg-repo`, caddy on the desktop with h1 + h2c):
x200, t490 and t460s 10 packages (aether, mise-bin, omarchy-dev, omarchy-local-ai,
omarchy-themes-intros, omasnap, owe, owe-lockfeed, qtengine, quickshell; their installed versions
were 1-4 weeks old), p14s 5 of them. pacman: `pacman -S --needed` (download, signature checks,
extraction, hooks). btrpkg: `pacbtr --no-snapshot` (the snapshot alone took 0.37 s on t460s).
Times include the final `sync`; writes are counted on the root's partition (below LUKS2); space is
btrfs data used afterwards, package cache included. Every run ended with the same package versions
and `pacman -Qkk` clean. Binary `43ffda5`; results in `bench/results/lab-20261121/`.

| host | pacman | btrpkg | downloaded pacman / btrpkg | written pacman / btrpkg | space pacman / btrpkg |
|---|---:|---:|---:|---:|---:|
| p14s (Ryzen 7 5850U, NVMe) | 5.11 s | 2.38 s | 318 / 69 MB | 685 / 105 MB | 414 / 79 MB |
| t460s (i7-6600U, NVMe) | 9.19 s | 6.54 s | 335 / 162 MB | 724 / 202 MB | 530 / 172 MB |
| t490 (i5-8265U, SATA SSD) | 11.44 s | 7.54 s | 335 / 162 MB | 722 / 200 MB | 530 / 172 MB |
| x200 (Core 2 Duo P8400, SATA SSD) | 34.78 s | 22.41 s | 335 / 162 MB | 737 / 206 MB | 530 / 172 MB |

btrpkg: half the download (a fifth on p14s; the rest comes from the installed versions), 3.5-6.5x
fewer bytes written, 3-5x less space, 1.4-2.1x faster. `pull` is most of btrpkg's time (p14s 1.2 s,
t460s 4.9 s, x200 17.6 s of the totals); placing the files takes 0.3-0.7 s and `pacman --dbonly`
with hooks 0.2-0.6 s. On the older machines `pull` is CPU: reading back and hashing the unchanged
data of installed files (one thread, SHA-256 without CPU instructions) and per-package requests,
not the network (the tunnel moved 117 MB/s, line rate, for p14s).

### Fresh install of the 315-package set on the same machines

`bench/lab-fresh.sh`, 3 rounds, throwaway subvolumes (cache inside `@pkg`, root inside `@`), inputs
from tmpfs. pacman-style: copy the `.pkg.tar.zst` into the cache, `bsdtar -xp` (no signature
checks); btrpkg: `fetch --no-fsync` + `install`, one `sync` at the end.

| host | pacman-style | btrpkg | written | cache + root | root alone |
|---|---:|---:|---:|---:|---:|
| p14s | 11.58 s | 9.29 s | 1,910 / 990 MB | 1,640 / 749 MB | 1,154 / 719 MB |
| t460s | 17.68 s | 15.74 s | 1,918 / 1,000 MB | 1,634 / 749 MB | 1,148 / 719 MB |
| t490 | 17.78 s | 14.67 s | 1,897 / 991 MB | 1,620 / 749 MB | 1,134 / 719 MB |
| x200 | 56.14 s | 37.20 s | 1,905 / 1,004 MB | 1,622 / 749 MB | 1,136 / 719 MB |

### What the first lab round changed

The first round (binary `2e610de`, same machines, same night) had btrpkg *slower* than bsdtar for
fresh installs on t460s (26.8 s against 17.7 s) and writing 340-370 MB for the upgrades. Three
causes, measured on t460s: decoding and hashing every chunk at fetch time (7.3 s; now opt-in, see
above), an fsync per cache file (1.4 s; now one `syncfs` per upgrade), and data taken from installed
files being compressed and written into the cache again (now reflinked when it is a whole piece).
Plus fewer syscalls in `install`: new paths are created under their own name (a temporary name and
rename only when replacing a file), and the parent directory fd is reused.

Not covered: the network path is an ssh reverse tunnel, because the desktop's firewall only lets
the lab reach pacoloco; a real mirror has latency, and pkgs.omarchy.org serves only HTTP/1.1.
Packages left to pacman in this set: limine-mkinitcpio-hook, limine-snapper-sync (backup files),
omarchy-settings-dev, omawrite, signed (install scripts). The local database records
`%VALIDATION% none` for packages upgraded this way.

## Format (version 1)

The full description is at the top of `src/format.rs`. The same index describes a package in
all three forms; the other two only append to it, so the header (and later its signature) is
identical everywhere:

| form | layout |
|---|---|
| index (`.btridx` then, now the repository's thin `NAME.pkg.btr`) | header, chunk table, entries, small-file blob |
| transport (`.pkg.btr`) | index, then the chunk data in table order |
| cache file | index, zero padding up to `image_base = align(index length)`, then the image |

| part | contents |
|---|---|
| header (256 B) | `BTRPKG\0\0`, version 1, hash algorithm (1 = SHA-256), number of chunks, image length, stored and uncompressed length of entries and blob, SHA-256 of the table, entries and blob, 64 reserved zero bytes, SHA-256 of the header (the package's ID; what a signature will cover) |
| chunk table | per chunk 88 B: image offset, unencoded length (4 KiB multiple, ≤128 KiB), stored length, compression (0 raw, 2 zstd), SHA-256 of the stored bytes, SHA-256 of the unencoded bytes. In image order, no gaps |
| entries | one zstd frame (level 19): kind, mode, uid, gid, mtime, size, data offset, small flag, path, link target, xattrs |
| small-file blob | one zstd frame (level 19): the contents of all small files |
| chunk data | zstd frames with window ≤128 KiB, no checksum, no dictionary ID; or raw data |

Reading checks each section before the next is read, so the chunk table can be used (chunk
downloads started) before the rest arrives. Readers also check what `install` relies on: safe
relative paths, known entry kinds, file data inside the image or blob, chunks that tile the
image. Lengths in the header are capped and nothing is allocated from them before the data has
arrived. Tests: `cargo test` (every flipped bit and every truncation of an index is rejected,
corrupt transports and stores leave no files behind).

Checked on the 315-package set (2026-11-19, desktop, bench partition, no LUKS):
- Converting all 315 at `-l 19`: 20 s (4 in parallel). Transport files 645.4 MB against 642.3 MB
  in version 0 (+0.5%: the second hash per chunk, 64 B × 48k chunks).
- `bench.sh` with `fetch` + `install`: 0 differences over 99,562 files against bsdtar
  (`results/v1-verify.txt`); installed extents `encoded,shared`.
- Same container run, alternating version 0 and 1 binaries, 3 rounds each
  (`results/v1-vs-v0.tsv`): 2.84–2.99 s against 2.88–2.97 s, the same device writes (0.98 GB) and
  root size; root + cache 3 MB larger (the bigger indexes). The earlier 2.52 s for version 0 was
  measured on another day; today's machine is slower for both.
- `pull-test.sh` against a version-1 store: the same chunks as before are uploaded and downloaded
  for the Hyprland upgrades (hyprland: 144 chunks, 5.50 MB; index 177.6 KB, was 155.4 KB),
  0 differences after installing, a corrupt chunk and a tampered index are refused with no file
  left behind.

Version 0 (the spike's format until 2026-11-19) had a 64-byte header, one manifest frame with the
blob inside, 24-byte table entries, the table after the manifest and a single trailing SHA-256.
The measurements below were made with it; the chunk data is the same in both versions.

Converter rules:
- Files of `--small` bytes or less go in the blob.
- Identical file contents are stored once and reflinked more than once (dedup).
- A chunk stays raw when compressing it doesn't save at least one 4 KiB block.
- Root-level dotfiles (`.PKGINFO`, `.MTREE`, …) are kept in the entries and not installed.

## Results: 2026-11-19, desktop (Ryzen 9, kernel 7.2.15), `/dev/nvme0n1p3`

Set: the 315 packages in this machine's pacman cache that match an installed version
(`bench/installed-set.txt`): 99,562 files, 9,069 dirs, 2,693 symlinks, 2.26 GB installed.
`bench/bench.sh` runs in the privileged `exp-btrfs` container.

Setup:
- A fresh `mkfs.btrfs` for every run, with no LUKS.
- `@` and `@pkg` subvolumes mounted as on Omarchy, with `compress=zstd:3`.
- Inputs are read from tmpfs. Time is wall-clock and includes the final `sync`.
- "Written" is sectors written to the partition, from `/sys/class/block/nvme0n1p3/stat`.
- Three rounds each. The spread was within 0.1 s, so the table shows one round.

| | pacman-style (cp + `bsdtar -xp`) | btrpkg, small ≤4 KiB | btrpkg, small ≤64 KiB | btrpkg `--no-encoded` (≤4 KiB) |
|---|---:|---:|---:|---:|
| download (package bytes) | 485 MB | 642 MB (+32%) | 559 MB (+15%) | 642 MB |
| install time | 4.78 s | **2.51 s** (−47%) | 3.39 s | 4.10 s |
| written to the device | 1.89 GB | **0.97 GB** (−49%) | 1.18 GB | 1.41 GB |
| data used, root + cache | 1.62 GB | **0.75 GB** (−54%) | 0.93 GB | 1.20 GB |
| data used, root only (cache deleted) | 1.14 GB | **0.72 GB** (−37%) | 0.82 GB | 1.19 GB |

Correctness (`bench/compare.py`, both trees installed side by side): 0 differences in type, mode,
owner, nlink, xattrs, mtime, symlink target or content (sha256) over all 99,562 files. Both
s4k and s64k runs. `filefrag` on installed files shows `encoded,shared` extents. Fetch never fell
back from encoded writes and install never fell back from reflinks.

Package size against `.pkg.tar.zst` for the same 315 packages (`-l 19`; `-l 22` gains under 0.1%,
because the 128 KiB window is the limit, not the level):

| small-file threshold | size | vs `.pkg.tar.zst` | without ansible | share of bytes reflinked |
|---|---:|---:|---:|---:|
| 4 KiB, no dedup | 709 MB | +46% | +24% | |
| 4 KiB + dedup | 642 MB | +32% | +22% | 97% |
| 16 KiB + dedup | 597 MB | +23% | +19% | |
| 64 KiB + dedup | 559 MB | +15% | +16% | 71% |

Worst cases:
- ansible (+278% before dedup): thousands of similar small Python files, where a solid archive
  wins.
- sof-firmware (+559% before dedup): duplicate files.
- gcc +23%, glibc +43%: long-range matches that a 128 KiB window can't reach.

## `.pkg.btr.zst` results (same set, 3 rounds, idle machine)

| | download | install time | written | root + cache | root only |
|---|---:|---:|---:|---:|---:|
| pacman-style (cp + `bsdtar -xp`) | 485 MB | 4.8 s | 1.89 GB | 1.63 GB | 1.14 GB |
| `.pkg.btr` (level 19 on the server) | 642 MB | **2.9 s** | 0.97 GB | 0.75 GB | 0.72 GB |
| `.pkg.btr.zst`, `fetch -l 1` | **434 MB** | 4.4 s | 1.09 GB | 0.86 GB | 0.84 GB |
| `.pkg.btr.zst`, `fetch -l 3` | **434 MB** (−10%) | 4.5 s | 1.06 GB (−44%) | 0.83 GB (−49%) | 0.80 GB (−30%) |

Notes:
- Size per package is about the same as `.pkg.tar.zst`: the median for packages over 1 MB is
  ×1.0006. Tiny packages are a few hundred bytes bigger because of fixed overhead. The total is
  10% smaller because of dedup and the long window in a few big packages (gcc −26%,
  hermes-desktop −26%).
- Converting all 315 packages to `.pkg.btr.zst` took 87 s wall on 32 threads (1,017 CPU-s).
- Verify: 0 differences over all 99,562 files.

Where `fetch` spends its time (all 315 packages, no install):
- `.pkg.btr`: 1.4 s.
- `.pkg.btr.zst`: 3.8 s with serial stages, then 3.4 s with a reader thread decompressing in
  parallel with framing and writing, then 2.8 s with the file hash replacing the inner hash.

The critical path is now the single-threaded outer zstd decode (about 1.3 s for 2.1 GB). Parallel
decode would need a multi-frame outer stream. Real pacman also downloads several packages at once,
so several `fetch`es would overlap.

## Splitting the outer stream into frames for parallel decode

`convert --frames N` writes the outer zstd as N independent frames of about size/N each (at least
`--min-frame`, default 1 MiB). Each frame has its content size in the header and a window sized to
the frame, capped at 2^27. `fetch` finds the frame boundaries from the block headers without
decoding and decodes all frames in parallel. A single-frame file is still stream-decoded, so its
decoding overlaps with framing and writing.

Spike limitation: the whole download is held in memory, and parallel decoding starts only once the
download is complete.

Same 315 packages, outer level `-15` (quicker benchmarks), `fetch -l 3`, 3 rounds:

| outer stream | download | conversion (32 threads) | `fetch` only, all | install end to end |
|---|---:|---:|---:|---:|
| pacman-style baseline | 485 MB | | | 4.78 s |
| `.pkg.btr` (no outer zstd) | 642 MB | | 1.4 s | 2.98 s |
| 1 frame | 481 MB | 16 s | 2.8–3.0 s | 4.46 s |
| 4 frames (size/4) | 505 MB (+5%) | 20 s | 2.50 s | 4.02 s (−10%) |
| 8 frames (size/8) | 521 MB (+8%) | 30 s | 2.40 s | 3.92 s (−12%) |
| 16 frames | 530 MB (+10%) | 33 s | 2.36 s | |
| 8 frames, each ≥32 MiB (12 packages split) | 503 MB (+5%) | 28 s | 2.72 s | |
| for reference: 1 frame at `-19` | 434 MB | 87 s | 2.8 s | 4.5 s |

Written bytes and space used are identical for all variants (same inner frames at level 3).
Verify: 0 differences over 99,562 files with 8 frames.

Findings:
- A frame can't use matches from earlier frames, and the long-range matches are in the big
  packages, the same ones that gain from parallel decode. Splitting only packages of 256 MiB or
  more still costs +5%.
- Splitting saves 0.4–0.55 s across the whole set, because decoding is only part of `fetch`. The
  rest is framing at level 3 (about 9 CPU-s), writes and install.
- `-19` instead of `-15` saves more download (−10%) than any frame split costs, so the outer level
  matters more than the frame count.
- Zstd dictionaries ("learning files") can't be used for the inner frames: btrfs decompresses
  extents without a dictionary. They could only apply to the outer stream, where they would mainly
  help the many tiny packages, which are a small share of the bytes.

## Under LUKS2, outer level `-19`, 1/2/4 frames

`bench.sh` with `LUKS=1`: LUKS2 `aes-xts-plain64` with 4096-byte sectors between the partition and
btrfs, set up fresh for each scenario. Same 315 packages, `fetch -l 3`, 3 rounds (spread ≤0.15 s),
idle machine. Results: `bench/results/luks-l19.tsv`.

| | download | install end to end | device writes | space (root + cache) | space after cache removed |
|---|---:|---:|---:|---:|---:|
| pacman baseline | 485 MB | 4.71 s | 1881 MB | 1623 MB | 1138 MB |
| 1 frame `-19` | 434 MB (−10%) | 4.57 s (−3%) | 1059 MB (−44%) | 831 MB (−49%) | 804 MB (−29%) |
| 2 frames `-19` | 451 MB (−7%) | 4.30 s (−9%) | 1059 MB (−44%) | 831 MB | 804 MB |
| 4 frames `-19` | 463 MB (−4%) | 4.00 s (−15%) | 1060 MB (−44%) | 831 MB | 804 MB |

Verify with 4 frames: 0 differences over 99,562 files.

- With LUKS, timings are within 0.1 s of the runs without it (pacman 4.78 s, 4 frames 4.02 s):
  AES-NI keeps up on this machine, and the writes btrpkg saves are encrypted writes saved too.
- At `-19`, 2 frames cost +4% download over 1 frame and 4 frames cost +7%. Even 4 frames stay
  smaller than the `.pkg.tar.zst` download.
- Conversion at `-19` for the whole set (32 threads): 2 frames 57 s, 4 frames 92 s.

6 and 8 frames, same setup, with pacman and 4 frames rerun alongside (`luks-l19-f68.tsv`):

| | download | install end to end |
|---|---:|---:|
| pacman baseline | 485 MB | 4.76 s |
| 4 frames `-19` | 463 MB (−4%) | 4.03 s (−15%) |
| 6 frames `-19` | 469 MB (−3%) | 3.93 s (−17%) |
| 8 frames `-19` | 480 MB (−1%) | 3.90 s (−18%) |

Device writes and space are the same as above. Verify with 8 frames: 0 differences. Conversion
takes 86 s for 6 frames and 87 s for 8.

- From 4 to 6 frames: +5 MB for −0.10 s. That only pays off on links faster than about 400 Mbit/s.
- From 6 to 8 frames: +11 MB for −0.03 s. That only pays off above about 3 Gbit/s, so 8 frames is
  not worth it.
- The benches read packages from RAM, so download time is not included. On a slow link, fewer
  frames finish first overall.

## Level-19 chunks inside an outer zstd `-10`

`convert -l 19 --outer-level 10 … .pkg.btr.zst`: the chunks are btrfs frames at level 19 (the
inner file is byte-for-byte the `.pkg.btr`), wrapped in one outer zstd frame at level 10. `fetch`
only decodes the outer stream and writes the frames as they are. LUKS2 4K, 3 rounds
(`luks-i19o10.tsv`), with the other variants rerun alongside:

| | download | install end to end | device writes | space (root + cache) | space after cache removed |
|---|---:|---:|---:|---:|---:|
| pacman baseline | 485 MB | 4.69 s | 1887 MB | 1624 MB | 1138 MB |
| `.pkg.btr` (level-19 chunks, no outer zstd) | 642 MB (+32%) | 2.86 s (−39%) | 973 MB (−48%) | 745 MB (−54%) | 719 MB (−37%) |
| level-19 chunks + outer `-10` | 616 MB (+27%) | 2.94 s (−37%) | 974 MB (−48%) | 745 MB (−54%) | 719 MB (−37%) |
| raw chunks + outer `-19`, 4 frames | 463 MB (−4%) | 4.01 s (−15%) | 1059 MB (−44%) | 831 MB (−49%) | 804 MB (−29%) |

Verify with level-19 chunks + outer `-10`: 0 differences. Converting the set took 25 s wall
(657 CPU-s, nearly all of it the level-19 chunks).

- The outer `-10` only takes 4% off `.pkg.btr` (compressed chunks leave little to find) and costs
  0.08 s of decoding. That pays off on links slower than about 2.6 Gbit/s.
- Against raw chunks with 4 frames, it installs 1.07 s faster and uses 85 MB less disk, but the
  download is 153 MB (33%) bigger. On download time alone, it only wins on links faster than about
  1.1 Gbit/s.

## Upgrades: reusing data from the installed version (Hyprland packages)

`bench/delta.py` works out what an upgrade would download if `fetch` reflinked whatever the
installed version already has. Frames are deterministic (same converter, level 19), so an
unchanged chunk has byte-identical frame bytes in both versions.
- **File reuse** (OSTree-like): a file whose frames all exist in the old version isn't downloaded.
- **Chunk reuse** (casync-like, on the fixed 128 KiB boundaries btrfs extents need): any frame
  the old version has isn't downloaded.
- Both always download the header, manifest (including small files), chunk table and digest.
- Reference: `zstd -19 --long=27 --patch-from` over the uncompressed tars. That's a binary diff:
  the client would have to rebuild and recompress, which btrpkg is meant to avoid.

Old versions came from pacoloco (hyprutils, hyprland via pkgs.omarchy.org) and the Arch archive.
Results: `bench/results/delta-hypr.tsv`.

| upgrade | `.pkg.tar.zst` | `.pkg.btr` | file reuse | chunk reuse | binary diff | unchanged |
|---|---:|---:|---:|---:|---:|---:|
| hyprland 0.57.1-2 → 0.57.2-2 (Omarchy) | 53.70 MB | 53.31 MB | 5.69 MB | **5.64 MB** | 1.50 MB | 132/142 files, 74% of bytes |
| hyprtoolkit 0.6.0-2 → 0.6.0-3 (rebuild) | 529 KB | 574 KB | 571 KB | **73 KB** | 3 KB | 83% of bytes |
| hyprland-guiutils 0.2.2-5 → 0.2.2-6 (rebuild) | 184 KB | 321 KB | 321 KB | 321 KB | 7 KB | 0% |
| hyprutils 0.14.3 → 0.14.4 | 240 KB | 255 KB | 241 KB | 241 KB | 91 KB | 15% |
| aquamarine 0.15.1 → 0.16.0 | 476 KB | 507 KB | 501 KB | 501 KB | 136 KB | 3% |
| hyprcursor, hyprgraphics, hyprpicker, xdg-desktop-portal-hyprland | 796 KB | 921 KB | 919 KB | 919 KB | 360 KB | 0–1% |
| **all 9 upgrades** | **55.93 MB** | 55.89 MB | 8.24 MB | **7.69 MB (−86%)** | 2.09 MB | |
| the 8 without hyprland | 2.23 MB | 2.58 MB | 2.55 MB | 2.06 MB (−8%) | 0.60 MB | |

- hyprland's saving is its wallpapers: `wall0-2.png` (48 MB) didn't change. The `Hyprland`
  binary (17 MB) changed, and chunk reuse found almost nothing inside it (49 KB). A new build
  changes addresses everywhere, so practically every 128 KiB chunk differs.
- Rebuilds differ: hyprtoolkit's rebuild left most chunks identical, hyprland-guiutils' changed
  all of them.
- For a new upstream version of a library, reuse finds almost nothing, and `.pkg.btr`'s +15–30%
  over `.pkg.tar.zst` remains.
- A binary diff would be 4x smaller again, but needs the client to recompress (the deltarpm /
  pacman-delta trade-off).

## Content-addressed chunk store: `publish` and `pull` (prototype)

```sh
btrpkg publish x.pkg.btr STORE                       # upload only chunks the store lacks
btrpkg pull [--sha256 HEX] STORE x CACHE/x.pkg.btr   # download only chunks the cache lacks
btrpkg install CACHE/x.pkg.btr /                      # unchanged
```

The store is a tree of static files (`src/store.rs`):
- `chunks/ab/<sha256>`: a chunk's bytes exactly as written to disk (a zstd frame or raw data),
  named by the sha256 of those bytes.
- `raw/ab/<sha256 of the uncompressed data>` → `compression length chunk-hash`. When the same
  data is published again, the stored chunk is reused even if this build compressed it to
  different bytes (another zstd version or level). Chunks stay stable across compressor upgrades.
- `index/x.btridx`: the package's index (format version 1: header with section hashes, chunk
  table with both hashes per chunk, entries, small-file blob). `pull --sha256` checks the index
  file like a repo database entry; signatures come in milestone 2.
  Chunks are written before the index, so a published index never refers to a missing chunk.
  When `publish` reuses a chunk through `raw/`, it first checks that chunk's bytes and that it
  decompresses to the same data, then writes the index with the stored chunk's entry.

`pull` reads the index, then the header and chunk table of every other cache file in the
directory (each cache file starts with its own index), and maps their chunks by the hash of the
unencoded data. It reflinks those from the old cache file and reads the rest from the store. Each
chunk is checked against its hash before it is written as an encoded extent. The result is the
same cache file `fetch` makes, so `install` is unchanged. On any error the partial file is
removed. (Version 0 kept a `.btridx` copy next to each cache file and matched on the stored
bytes' hash.)

Hyprland upgrades (old versions published and installed first, then the new versions;
`bench/pull-test.sh`, fresh btrfs on the bench partition, no LUKS). Results:
`bench/results/publish-hypr.tsv`, `pull-hypr-new.tsv`, `pull-hypr.log`.

| new version | `.pkg.tar.zst` | chunks downloaded | download (index + chunks) | upload (index + chunks) |
|---|---:|---:|---:|---:|
| hyprland-0.57.2-2 | 53.70 MB | 144/647 | 5.66 MB | 5.66 MB |
| hyprutils-0.14.4-1 | 0.24 MB | 7/16 | 0.24 MB | 0.24 MB |
| aquamarine-0.16.0-1 | 0.48 MB | 15/19 | 0.50 MB | 0.50 MB |
| hyprtoolkit-0.6.0-3 | 0.53 MB | 4/17 | 0.07 MB | 0.07 MB |
| hyprgraphics-0.6.0-1 | 0.09 MB | 3/3 | 0.09 MB | 0.09 MB |
| hyprcursor-0.1.14-1 | 0.23 MB | 8/9 | 0.30 MB | 0.30 MB |
| xdg-desktop-portal-hyprland-1.5.0-1 | 0.38 MB | 10/10 | 0.43 MB | 0.43 MB |
| hyprland-guiutils-0.2.2-6 | 0.18 MB | 11/11 | 0.32 MB | 0.32 MB |
| hyprpicker-0.4.8-1 | 0.10 MB | 3/3 | 0.10 MB | 0.10 MB |
| **all 9** | **55.93 MB** | 205/735 | **7.72 MB** | **7.72 MB** |

- Download and upload are the same here because the store and the cache both held exactly the
  previous versions. The index alone is 0.22 MB for the 9 packages (hyprland 155 KB).
- Space on the test filesystem: old versions installed and cached 57.1 MB, new versions pulled
  65.5 MB, after installing over the old ones and deleting the old cache files 58.2 MB.
- Verify: the 9 new versions installed from pulled cache files, after the old cache files were
  deleted, against `bsdtar`: 0 differences over 785 files. Extents `encoded,shared`.
- A corrupted chunk in the store and a tampered index were both refused, leaving no file behind.
- hyprland 0.57.2-2 converted again at `-l 18` (337 of 647 chunks compress to different bytes)
  and published: 0 chunks uploaded, all mapped to the stored ones through `raw/`.

Prototype limits:
- The store is read as a local directory. HTTP, parallel and pipelined downloads aren't done.
- No signature, and the publisher's `raw/` map is trusted.
- Chunks already in the local cache are trusted without rehashing. `BTRFS_IOC_ENCODED_READ` could
  rehash them without decompressing.
- No garbage collection of the store.
- Many small files: fine for Omarchy's own repo, but not the layout of a normal pacman mirror.
- `install` doesn't remove files that a new version dropped.

## Repository layout, S3 and signatures (design; signatures not implemented)

*Historical design (2026-11-20). Since then: signatures are implemented (signify over the index
header), indexes are `NAME.pkg.btr` next to the package files, and btrparts live in a shared
`btrparts/` directory; see "Repository layout" at the top.*

### What a published package is

A package is now its **index** (`x.btridx`): a small file with the package's metadata, its tiny
files and a list of chunk hashes. The file data lives in chunk objects shared by all packages and
versions. For the 315-package set (`--small 4096`, format version 1):

| part of the index | total (315 indexes) | |
|---|---:|---|
| small-file blob, zstd | 18.2 MB (77%) | the contents of all files ≤ 4 KiB (60.3 MB uncompressed) |
| chunk table | 4.2 MB (18%) | 48,118 entries × 88 B (offset, lengths, compression, SHA-256 of stored and of uncompressed bytes) |
| entries, zstd | 1.2 MB (5%) | paths, modes, owners, mtimes, xattrs, symlinks (15.6 MB uncompressed) |
| headers | 0.08 MB | 256 B each, with the section hashes (what the signature covers) |

Median index 11.4 KB, 90th percentile 124 KB, largest ansible at 4.2 MB (53,668 files). Total
23.7 MB; it was 22.1 MB in format version 0 (56 B table entries).

### Layout

```
<repo root>/                                  (bucket, or a directory served over HTTP)
  x86_64/omarchy.db[.sig]                     repo database: the only object that changes
  x86_64/index/hyprland-0.57.2-2-x86_64.btridx        one per package version, immutable
  x86_64/index/hyprland-0.57.2-2-x86_64.btridx.sig    detached signature, immutable
  btrparts/ab/ab12…ef                         btrpart bytes as stored on disk, named by their sha256;
                                              shared by all packages, versions, repos and architectures
                                              (the db's btrpkg.parts=../btrparts; was chunks/)
(build server only, not published)
  btrparts/.raw/ab/…                          uncompressed-data hash → btrpart; keeps btrparts stable
                                              when the compressor changes
```

The database entry for a package names its index and the index's sha256 (`pull --sha256`), the
way `%FILENAME%` and `%SHA256SUM%` name a `.pkg.tar.zst` today. Put them in the entry's `%DATA%`
block as `key=value` lines (`btrpkg.index=…`, `btrpkg.index_sha256=…`), next to the normal fields,
so one `omarchy.db` serves pacman and btrpkg. Tested with pacman 7.1.0: `%DATA%` lines are read
silently, while a new `%KEY%` section prints "unknown key" on every database read. A `%DATA%` line
without `=` makes pacman drop the whole package entry. The publish order is: chunks,
then index and signature, then the database. A client never sees a database entry whose objects
are missing.

### On S3 (or R2)

- **Immutable objects.** Every object except the database is immutable and named by content or
  version, so it can be served with `Cache-Control: public, max-age=31536000, immutable` behind
  any CDN. Only the database needs a short TTL.
- **Upload.** Chunks are written with `PutObject` + `If-None-Match: *`. A 412 means another
  publish already stored the same chunk, so concurrent publishers are safe and need no listing.
  The 2-hex-digit prefix isn't needed for S3 performance any more, but keeps the tree usable as a
  plain mirror directory (256 subdirectories).
- **Downloads are many small objects.** At the 4 KiB small-file threshold, 79% of chunk objects
  are under 16 KB (median 4.5 KB): files just above 4 KiB each become their own object. A
  client needs HTTP/2 and many requests in flight. Raising the threshold moves those files into
  the index:

| `--small` | chunk objects | chunks | indexes | total download, all 315 | median chunk |
|---|---:|---:|---:|---:|---:|
| `.pkg.tar.zst` today | 315 files | | | 485 MB | |
| 4 KiB | 47,518 | 612.7 MB | 22.1 MB | 634.8 MB | 4.5 KB |
| 16 KiB | 22,939 | 535.4 MB | 53.8 MB | 589.2 MB | 12.4 KB |
| 64 KiB | 12,326 | 447.1 MB | 105.1 MB | 552.2 MB | 31.8 KB |

  (Index sizes measured with format version 0; version 1 adds 32 B per chunk, 23.7 MB at 4 KiB.)

  Files inside the index are written as plain data at install (the kernel compresses them at
  `zstd:3`). At 64 KiB the earlier bench measured 1.18 GB written against 0.97 GB at 4 KiB, still
  far below pacman's 1.89 GB.
- **Packs (if request count matters).** An alternative is to group the chunks each package
  version introduces into one pack object, with the index pointing at (pack, offset). The client
  then fetches runs of chunks it lacks with byte-range GETs. S3 serves one range per request, so
  each run is one GET; hyprland's changed binary is 132 consecutive chunks. The cost is
  git-style repacking to drop dead chunks.
- **Cost at list prices** (S3 Standard us-east-1: GET $0.0004 and PUT $0.005 per 1,000, egress
  $0.09/GB after 100 GB/month; R2: $0.36 and $4.50 per million, no egress fee).
  - Fresh install of the 315-package set at 4 KiB: about 47,800 GETs. That's $0.019 on S3 plus
    $0.057 egress, or $0.017 on R2. The same set as `.pkg.tar.zst` is 315 GETs + $0.044 egress.
  - Publishing the whole set once: about 47,800 PUTs, $0.24 on S3.
  - The hyprland upgrade: 145 PUTs.
  - Egress, not requests, dominates on S3, and the upgrade savings cut exactly that.
- **Garbage collection** is mark and sweep: chunks not referenced by any retained index are
  deleted. A lifecycle rule alone can't do it.

### Download speed with many small objects (measured 2026-11-19 from the desktop, estimates from it)

Measured with `curl --parallel` on small real objects (`.sig` files, 119 B):

| server | protocol | RTT | 1 at a time | 8 parallel | 32 parallel | 100 parallel |
|---|---|---:|---:|---:|---:|---:|
| pkgs.omarchy.org (Cloudflare, cache HIT) | HTTP/1.1 only | 18 ms | 31 req/s | 210 req/s | 550 req/s | 790 req/s |
| the same, first fetch (cache MISS) | HTTP/1.1 | | 3.7 req/s (270 ms each) | | | |
| geo.mirror.pkgbuild.com (Arch) | HTTP/2 | 74 ms | 15 req/s | 105 req/s | 300 req/s | 500 req/s |

Bulk download from pkgs.omarchy.org: 25 MB/s (hyprland, 53.7 MB, 2.1 s). pkgs.omarchy.org doesn't
offer HTTP/2 (`--http2` still gets 1.1).

Estimate for a fresh install of the 315-package set at 25 MB/s and 790 req/s, taking the
slower of bytes and requests:

| | objects | bytes | estimate |
|---|---:|---:|---:|
| `.pkg.tar.zst` | 315 | 485 MB | about 19 s (bytes) |
| btrpkg, `--small` 4 KiB | 47,833 | 635 MB | about 60 s (requests) |
| btrpkg, `--small` 16 KiB | 23,254 | 589 MB | about 29 s (requests) |
| btrpkg, `--small` 64 KiB | 12,641 | 552 MB | about 22 s (bytes) |
| btrpkg with packs, range GETs | about 1 per run of chunks | ~635 MB | about 25 s (bytes) |

- Upgrades are where chunks win: hyprland 0.57.1 → 0.57.2 is 145 objects and 5.7 MB, about
  0.25 s, against 2.1 s for the 53.7 MB package.
- Cache misses are the bigger risk: a chunk that few clients ask for is likely not in the CDN
  edge, and each miss costs about 270 ms (origin fetch). Fewer, bigger objects (packs or a higher
  `--small`) are cached better.
- These are estimates from small objects; not yet measured with a real chunk store on R2/S3.

HTTP/1.1, HTTP/2 and HTTP/3 against the same server (Arch's mirror, which offers all three, same
250 `.sig` files) at the same parallelism were equal: 0.45 to 0.48 s with 100 in flight, 2.3 to
2.4 s with 8. What changes is how much parallelism each one gets cheaply:
- HTTP/1.1 runs one request per connection at a time, so in-flight requests = connections. The
  790 req/s above needed 100 TLS connections; pacman's `ParallelDownloads` default is 5, which at
  32 ms per request gives about 155 req/s, or about 5 minutes for 47,833 objects.
- HTTP/2 multiplexes up to the server's stream limit (typically 100 to 256) on one connection, so
  the request floor drops to 47,833 × 32 ms / 100 = about 15 s and the byte floor (about 25 s)
  decides. HPACK also shrinks the response headers, 851 B per object on pkgs.omarchy.org today
  (40 MB over the set, 6% of the bytes).
- HTTP/3 has the same multiplexing. It adds a faster handshake (one round trip less, irrelevant over
  a 25 s download) and no TCP head-of-line blocking: a lost packet stalls one stream, not all of
  them. That helps on lossy wifi or mobile links, not on a clean wired one.

### Signatures

Hash: SHA-256 over the chunk bytes as stored (compressed), as the prototype does. Measured on
the desktop (Ryzen 9 9950X, SHA-NI): SHA-256 2.6 GB/s per core, BLAKE3 9.4 GB/s per core (36 GB/s
on all cores). Hashing the 635 MB of chunks takes 0.25 s on one core against about 25 s of
download, so BLAKE3's speed buys nothing here. SHA-256 is what pacman (`%SHA256SUM%`), OpenPGP,
fs-verity and S3's upload checksums already use, and `sha256sum` can check a chunk's name.
Not xxhash or CRC: chunk names are content addresses shared across packages, so they must be
collision resistant. Put an algorithm id in the index header so it can change later.

What this machine does today: `SigLevel = Required DatabaseOptional`. Every package has a
detached OpenPGP signature (Arch packagers, or "Omarchy <pkgs@omarchy.org>" for the Omarchy
repo). The repo databases are unsigned (`omarchy.db.sig` is a 404).

**Decision: plain Ed25519, as OpenBSD does, in the Alpine apk v3 structure.**
- **OpenBSD** (`signify`, `pkg_add`): Ed25519, public keys as files in `/etc/signify/*-pkg.pub`,
  and the signature is checked before anything is decompressed.
- **Alpine apk v3:** one metadata block holds every file's hash. A digest of that block is
  signed, the data blocks are covered by the hashes, and keys are files (`/etc/apk/keys`) looked
  up by key ID. Alpine still signs with RSA keys (apk v2 `.SIGN.RSA.*`; v3 lists "newer signature
  algorithms"), so btrpkg takes the structure from Alpine and the algorithm from OpenBSD.

For btrpkg:
1. **Sign the index header.** The header holds the SHA-256 of the chunk table, entries and blob,
   and the table holds every chunk's hashes. So one Ed25519 signature over the header covers
   manifest, small files, layout and all data, with a namespace string (`btrpkg-index-v1`) so it
   can't be reused elsewhere. Chunks have no signatures of their own.
2. **Keys are files.** Public keys go in `/usr/share/btrpkg/keys/` (from a keyring package) and
   `/etc/btrpkg/keys/` (local). The signature names its key. There's no GPG, trustdb, agent or
   keyserver on the btrpkg path. What GPG did that has to be replaced: revocation (a
   revoked-keys file in the key directory) and rotation (a new keyring package signed by the
   old key).
3. **Speed** (desktop): Ed25519 verify 44 µs (22,584/s, `openssl speed`), against `gpgv` 6.8 ms
   per package, of which about 4.8 ms is hashing the package.
4. **Order on the client.** Download the index and its signature, and verify before parsing
   anything else. Check every chunk before it reaches `BTRFS_IOC_ENCODED_WRITE`. The kernel
   stores frames without checking them, so this check is the only protection against a bad
   frame.
5. **Sign what is published.** `publish` can rewrite the chunk table when it reuses stored
   chunks (a compressor change), which changes the header. So the signature is made over the
   index as published, not at convert time.
6. **Re-verifying the cache.** Verify the cache file's own index header against its signature,
   then hash the chunks in the image. `BTRFS_IOC_ENCODED_READ` returns the stored bytes, so no
   decompression is needed. On other filesystems the table's uncompressed hash (format
   version 1) covers plain data.
7. **Who signs.** Iteration 1 covers Omarchy's own packages only (`PLAN.md`), signed with the
   Omarchy repo key. Arch's packages would need converting, and Arch's OpenPGP packager
   signatures cover the `.pkg.tar.zst` bytes, which a conversion changes. The build server would
   check those signatures once with `gpgv` and pacman's keyring, then sign the index with the repo
   key. That's a new trust decision, so it stays out of scope for now. Conversion is
   deterministic, so anyone can download the original, convert it and compare the uncompressed
   chunk hashes.
8. **The `raw/` map stays private** on the build server. `publish` checks that a mapped chunk
   decompresses to the same data before reusing it (format version 1).
9. **Repo database.** Signing the index doesn't stop a mirror from serving an older, validly
   signed index or database (freeze or rollback). That's the same gap pacman has with
   `DatabaseOptional`. Signing the database (or TUF-style timestamped metadata) closes it for
   both.
10. **On S3,** signatures are ordinary immutable objects. The signing key stays on the build host:
    whoever can write to the bucket can delete objects or serve old ones, but can't forge a
    package.

Not implemented yet (milestone 2 in `PLAN.md`): `btrpkg keygen`, `publish --key`, verifying in
`fetch`/`pull`/`install`, revoked keys, and re-verifying the cache.

## Other filesystems than btrfs (from the code, not tested yet)

Installing works on any filesystem, but only btrfs gets the benefits. The format itself has
nothing btrfs-specific in it. `fetch`, `pull` and `install` each fall back when a btrfs feature
is missing:

| step | on btrfs | without encoded writes (not btrfs, or not root) | without reflinks |
|---|---|---|---|
| `fetch` / `pull` writing chunks | zstd frames stored as they are | decompresses each chunk and writes plain data (falls back after the first failed `BTRFS_IOC_ENCODED_WRITE`) | — |
| `pull` reusing chunks from older cache files | reflink | — | downloads those chunks again instead |
| `install` | reflink from the cache file | — | `copy_file_range` from the cache file |

What that means per filesystem:
- **XFS (reflink on by default), bcachefs:** installed files share blocks with the cache file, so
  the cache costs no extra space. But no data is compressed: root + cache take the uncompressed
  size (2.26 GB for the 315-package set, against 0.75 GB on btrfs). Decompressing at fetch is
  cheap; nothing is compressed at install.
- **ext4, tmpfs, others without reflinks:** the cache file is the uncompressed image and install
  copies it, so root + cache take twice the uncompressed size. That's worse than pacman, which
  keeps the compressed `.pkg.tar.zst` in the cache. Deleting the cache file after install gets
  back to plain extracted files. `pull` also loses its upgrade saving, because chunks it can't
  reflink are downloaded again.
- **Download size** doesn't depend on the filesystem.

Known gaps:
- **`pull` could copy, not download, chunks it can't reflink.** It already has them locally.
- **btrfs without root:** `fetch`/`pull` set `FS_NOCOMP_FL` on the cache file before trying
  encoded writes. When that fails, the fallback writes plain data that the kernel then stores
  uncompressed. The flag should only be set once an encoded write has succeeded.
- **Never run:** `bench/nonbtrfs-test.sh` (tmpfs, ext4 and XFS against `bsdtar`) is written but
  hasn't been run yet.

## A standard index format? SQLite and others (measured 2026-11-20)

`bench/sqlite_index.py bench/out-v1 OUT` rewrites the 315 version-1 indexes (118,286 entries,
48,118 chunks) as SQLite files:

| index | on disk | download (zstd -19) | read all 315 (Python) |
|---|---:|---:|---:|
| version 1 | 23.7 MB | 23.7 MB | 129 ms |
| SQLite, small files as one row each | 101.7 MB | 24.3 MB (+3%) | 137 ms |
| SQLite, small-file blob as one zstd column | 40.3 MB | 23.6 MB (−1%) | 127 ms |

SQLite doesn't hurt speed or download size: about 0.4 ms per package against about 9 ms of install
per package. It doesn't suit the transport, for other reasons:
- A database has to be downloaded whole and decompressed before the first query. Version 1 checks
  the header and chunk table first and starts chunk downloads while the entries and blob arrive.
- The index is the first part of `.pkg.btr` and of the cache file, so one header (and later one
  signature) holds everywhere. A database would be a separate file.
- The bytes depend on the SQLite version, so a republished index can get a new hash and name.
- libalpm doesn't link SQLite. The reader in `src/format.rs` (header, table, entries, checks) is
  about 150 lines, small enough to port to C for the libalpm patch.

Where the 23.7 MB goes: small-file blob 18.2 MB, chunk table 4.2 MB, entries 1.2 MB, headers 0.08 MB.

SQLite does fit on the client: as the local index of which chunk hashes are in the cache (`pull`
opens every cache file on every pull now), or as a local package database later.

Other existing formats, none a drop-in:
- **zstd:chunked / eStargz** (containers/storage, OCI layers): a `.tar.zst` with each file's data
  in its own zstd frames and a table of contents in a trailing skippable frame. This is the closest
  match, and it points to a design: a `.pkg.btr` whose 128 KiB frames sit inside a valid tar
  stream would also be a `.pkg.tar.zst` that pacman installs unchanged. Tested only that libarchive
  accepts it: a pax tar cut into separate 128 KiB zstd frames, plus a trailing skippable frame,
  passes `zstd -t` and `bsdtar -x` (byte-identical). Not built: aligning frame boundaries with file
  data, `.PKGINFO` first, and its size (the +15–32% of per-extent frames would then hit pacman
  users too).
- **casync `.caidx`**: a chunk list without file metadata, with content-defined chunks that don't
  fit btrfs extents.
- **composefs/EROFS** (OSTree, bootc): a metadata image plus objects named by digest, close to the
  store layout, but its objects are whole uncompressed files, not encoded extents.
- **Generic encodings** (CBOR, protobuf, FlatBuffers, mtree) would fit only the entries section,
  which is 1.2 MB of the 23.7 MB. The fixed header, table and per-section hashes stay either way.

`.MTREE` (makepkg's `bsdtar --format=mtree`, gzip, keywords `type uid gid mode time size sha256 link`,
no xattrs) already travels inside every `.pkg.btr` as an ordinary file: 4.40 MB over the 315 set
(0.7% of the 645.4 MB transport; 4.06 MB of it in chunks, being over the 4 KiB small-file limit).
It holds what the entries hold, except data offsets and xattrs, plus a hex SHA-256 per file. Hyprland
0.57.2: `.MTREE` 117 KB raw, 36.4 KB gzip, 31 KB zstd -19; its entries 80 KB raw, 10.9 KB zstd. Keep
it: libalpm needs it for the local database (`--dbonly` builds the file list from it,
`build_filelist_from_mtree` in `be_package.c`, and `pacman -Qkk` checks against it in `check.c`).
Making it from the entries on the client instead would need a per-file SHA-256 in the entries
(32 B per file) and would save about 1 MB (0.2%), so not worth a format change.

The whole index as one zstd-compressed CBOR document (`uv run --with cbor2 python
bench/cbor_index.py bench/out-v1`; canonical CBOR of `{v, chunks, entries, blob}`, then zstd -19):

| index (315 set) | size | read all 315 (Python) |
|---|---:|---:|
| version 1 | 23.71 MB | 125 ms |
| CBOR, arrays, zstd -19 | 22.82 MB (−3.8%) | 92 ms (cbor2) |
| CBOR, entries as text-keyed maps | 22.88 MB (−3.5%) | |
| CBOR, arrays, no image offsets | 22.71 MB (−4.2%) | |

The gain is the chunk table getting compressed, not CBOR: version 1 stores the table raw, and zstd
-19 of the tables alone saves 0.86 MB (3.6%). The hashes (64 of the 88 B per chunk) don't compress.
So size doesn't decide it. What CBOR would bring: fields can be added without a format version,
deterministic encoding is specified (RFC 8949 §4.2), there are decoders for Rust (serde) and C
(tinycbor, libcbor, QCBOR), and COSE_Sign1 (RFC 9052, EdDSA = -8) is a standard signature container.
What it costs: the table can't be used before the whole document has arrived and been decompressed
(estimated earlier at about 2 RTT, ~36 ms, on a cold connection; an average index is 75 KB),
and the decompressed size must be capped before decompressing (review finding). A small fixed
binary prefix (magic, version, compressed and raw length, SHA-256 of the compressed document,
signed) keeps the index at the front of `.pkg.btr` and the cache file, and means the CBOR parser
only ever sees authenticated bytes.

## pacman integration (design; step 1 tested by hand)

Step 1, no pacman change: btrpkg places the files, then hands pacman a metadata-only package.
`pacman -U --dbonly x-meta.pkg.tar.zst` with only `.PKGINFO`, `.BUILDINFO`, `.MTREE` (and
`.INSTALL`) writes a full local DB entry: libalpm builds the file list from `.MTREE`
(`be_package.c`, `build_filelist_from_mtree`). Tested with pacman 7.1.0 on `tree` in an Arch
container: 2,552 B metadata tar, `pacman -Qkk tree` "7 total files, 0 altered", `pacman -R` removes
the files. `--dbonly` skips the file-conflict check (`sync.c` `_alpm_sync_check`), the removal of the
old version's files (`remove.c`) and all extraction, including `.pacnew`/backup handling, but still
runs scriptlets and hooks. So btrpkg would have to do conflicts, removals and backups itself, and
`pre_upgrade` would run after the files are already in place. Good for measuring, not for real use.

Step 2, a libalpm patch, where each part plugs in (pacman `a6f7467d`):
- sync DB: `btrpkg.index=` / `btrpkg.index_sha256=` in `%DATA%` (works today).
- download (`sync.c` `find_dl_candidates`/`download_files`): when the entry has an index and the
  cache supports encoded writes, fetch the index and the missing chunks into `.pkg.btr` with the
  existing libcurl multi code; otherwise `%FILENAME%` as now.
- validation (`sync.c` `check_validity`): the index's sha256 from the (signed) DB, or its `.sig`.
- loading (`be_package.c` `_alpm_pkg_load_internal`): `.PKGINFO` and `.MTREE` come from the index
  manifest instead of libarchive.
- extraction (`add.c`): keep `commit_single_pkg` and `extract_single_file` (backup, `.pacnew`,
  NoExtract, conflicts); only `perform_extraction`, which calls `archive_read_extract2`, gets a
  second path: `FICLONERANGE` from the cache file for large files, the small-file blob for the rest,
  then mode, owner, mtime and xattrs.
- cache (`_alpm_filecache_find`, `-Sc`, paccache): know `.pkg.btr` + `.btridx`.
The client side has to be C (libalpm is C); the Rust converter stays on the build server.

## Rollback on a failed install (decision, not implemented)

Chosen (Jonny, 2026-11-19): **snapshot first, install live, roll back only on failure**. This is what
`omarchy-update` already does through `omarchy-snapshot` (snapper config `root`, Limine boots the
snapshots):

1. Take a snapper snapshot of `@` before the transaction.
2. Install into the live root as now.
3. If the install fails, the snapshot is the way back (`snapper rollback` or boot the snapshot,
   then reboot). The snapshot also covers `/var/lib/pacman`, so the database and the files roll back
   together. That closes pacman's half-written `desc` and upgrade-gap problems for the failure case.

Why not install into a snapshot and switch to it at boot (transactional-update / OSTree style):
btrfs can't merge, so anything written to the live `@` between the snapshot and the switch is lost.
On Omarchy only `/home`, `/var/log` and `/var/cache/pacman/pkg` are separate subvolumes; `/etc`
and `/var/lib` (docker, tailscale, bluetooth, NetworkManager, ollama, ...) are in `@`.

Notes:
- The snapshot is metadata only. With btrpkg the old files' extents are shared with `@pkg` anyway,
  so keeping the snapshot costs little extra space.
- `@home`, `@log` and `@pkg` are not rolled back, which is what we want.
- Not checked yet: whether a booted older snapshot has a matching kernel and initramfs on the ESP
  (limine-snapper-sync).

## Verdict: VALIDATED for install cost, with a download-size cost

What worked:
- **Install.** Encoded writes into `@pkg` plus cross-subvolume, cross-mount reflinks into `@`
  install 315 packages in about half the time with half the device writes.
- **Disk space.** The cache costs almost nothing: root + cache is less than pacman's root alone.
  The root itself is 37% smaller because extents are compressed at level 19, not the kernel's
  zstd:3.
- **Dedup.** Identical files inside a package share extents on disk, which pacman can't do.
- **Fallback.** The same file works without encoded writes (`--no-encoded`) and without reflinks
  (copy).

Costs:
- `.pkg.btr` downloads are +15% to +32% against Arch's `.pkg.tar.zst`. `.pkg.btr.zst` removes
  that cost (−10%) in exchange for client-side compression during `fetch`. The btrfs 128 KiB window is a hard
  limit; levels beyond 19 don't help.
- A higher small-file threshold shrinks the download but writes more at install, and still beats
  pacman at 64 KiB.

Not covered yet:
- pacman integration: DB, hooks, scriptlets, backup files, conflicts. Baseline is bsdtar, not
  `pacman -U`.
- Network and signatures. sha256 is checked after the encoded writes, but before anything reads the
  data. (LUKS2 with 4K sectors is covered above.)
- Upgrades: replacing files, reflinking unchanged extents from the previous version
  (delta downloads).
- Hostile input: the kernel decompresses frames that userspace supplied. A signature check must
  happen before install, and `fetch` must keep refusing to install unverified files.

## The spike (2026-11-19)

A trial package format for Omarchy: a post-processor turns a pacman `.pkg.tar.zst` into a
`.btrpkg` whose file data is already in btrfs's on-disk form, made of zstd frames of up to 128 KiB,
one set of extents per file. On a btrfs system:

1. **fetch** streams the package into the package cache. Header, manifest and chunk table are
   written as plain data. Every zstd frame goes to disk unchanged through
   `BTRFS_IOC_ENCODED_WRITE`: the kernel neither decompresses nor recompresses it. The cache file
   reads back as an uncompressed image of all the package's large files, each at a 4 KiB-aligned
   offset.
2. **install** creates each large file by `FICLONERANGE` from the cache file, rounding the length up
   to 4 KiB and truncating afterwards. Small files come from a solid-compressed blob in the
   manifest and are written normally. A file's data is never read, written or compressed at install
   time, and the installed files share their extents with the cache.

Two variants of the same file:

- **`.pkg.btr`**: chunks are already btrfs zstd frames (level 19 on the build server). `fetch`
  does no compression work, but the download is 15–32% bigger than `.pkg.tar.zst`.
- **`.pkg.btr.zst`**: a `.pkg.btr` with *raw* chunks, wrapped in one solid zstd stream
  (`-19 --long=27`, with checksum). The download is smaller than `.pkg.tar.zst`. `fetch`
  decompresses it while streaming, compresses each 128 KiB chunk into a btrfs frame on all cores
  (`-l`, default 3), and writes the frames with encoded writes. Wrapping already-framed chunks in
  zstd ("double compression") saves only about 4%, so the inner chunks stay raw.

```sh
cargo build --release
btrpkg convert [-l 19] [--small 4096] in.pkg.tar.zst out.pkg.btr          # post-processor
btrpkg convert [--outer-level 19] [--frames N] [--min-frame B] in.pkg.tar.zst out.pkg.btr.zst  # raw chunks, solid zstd
btrpkg keygen NAME                                                        # NAME.sec + NAME.pub (signify format)
btrpkg sign --key NAME.sec x.pkg.btr                                      # x.pkg.btr.sig over the header
btrpkg publish [--key NAME.sec] x.pkg.btr STORE                           # chunks, signature, index
btrpkg fetch TRUST [--no-encoded] [-l 3] [--sha256 HEX] IN|- /var/cache/pacman/pkg/x.pkg.btr   # root
btrpkg pull TRUST STORE NAME /var/cache/pacman/pkg/x.pkg.btr
btrpkg install /var/cache/pacman/pkg/x.pkg.btr /                          # reflinks, falls back to copy
btrpkg info x.pkg.btr
# TRUST: --id HEX (header ID from the repo db) | --sig FILE [--keys DIR] (default IN.sig or the
#        store's NAME.btridx.sig, keys in /etc/btrpkg/keys and /usr/share/btrpkg/keys) | --unverified
```

`fetch` and `pull` check the header first, against an expected ID or a signature, and refuse to
go on without one (`--unverified` is for benchmarks). Then they check the index (chunk table,
entries, small-file blob, each against its hash in the header), then every chunk before writing
it: the hash of its stored bytes (which the signed index covers) and, for a zstd chunk, the frame
header btrfs relies on (one frame, no dictionary ID or checksum flag, content size = unencoded
length). `--verify-data` also decodes every frame the way btrfs will (window at most 128 KiB) and
checks the unencoded hash; `publish` always does. The client doesn't need it for safety: data is
only ever reused (from an older cache file or an installed file) after reading it back and
checking its unencoded hash, and on a CPU without SHA instructions the full check cost 7 s of 27 s
for the 315-package set (t460s).
`install` resolves every path from a root directory fd with `openat2(RESOLVE_BENEATH |
RESOLVE_NO_SYMLINKS)`, so nothing is written, changed or linked through a symlink, whether the
package made it or it was on disk (pacman refuses to extract through a symlink too); hardlinks may
only point at earlier files of the same package. For `.pkg.btr.zst` the outer zstd checksum is checked too. `--sha256` optionally
compares the downloaded file with the repo database's value. `fetch` refuses truncated input and
trailing data, and on any failure removes its partial file.


