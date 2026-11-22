# libalpm patches

pacman branch `btrpkg`, based on `54d94116` (Arch's pacman 7.1.0.r9 build), as `git format-patch`
files. Apply with `git am` on that commit of https://gitlab.archlinux.org/pacman/pacman.

1. `libalpm: read btrpkg indexes and their signatures`: `lib/libalpm/btrpkg.c` parses and checks
   a version 1 index with the same checks and verdicts as `src/format.rs`, and verifies the
   signify signature (Ed25519 through OpenSSL). `test/btrpkg` (`meson test -C build btrpkg`):
   73 subtests on committed fixtures (a real index and signature, every bit flip and cut, crafted
   indexes from `bench/craft_indexes.py`).

2. `libalpm: install btrpkg packages from sync repositories`: with `BtrPkgHelper` set in
   pacman.conf (e.g. `BtrPkgHelper = /usr/bin/btrpkg`) and the cache directory on btrfs, sync
   targets whose database entry has `btrpkg.index`/`btrpkg.id` in `%DATA%` are downloaded as
   `NAME.pkg.btr` by `btrpkg pull-ids` (one download queue per repository, data reused from the
   installed versions), validated by header ID (instead of `%SHA256SUM%`) and signify signature
   (instead of OpenPGP, keys in `/etc/btrpkg/keys`, `/usr/share/btrpkg/keys`), read through
   `_alpm_open_archive` as a tar stream made from the index (`btrstream.c`), and extracted by
   pacman's usual code, except that large files are reflinked from the cache file (new file next
   to the target, owner, mode, xattrs, mtime, rename). Without `BtrPkgHelper` nothing changes.

3. `libalpm: btrpkg downloads in pacman's sandbox, with real sizes and progress`: the download
   is split in two processes. `btrpkg fetch-ids` (network) runs as `DownloadUser` under pacman's
   seccomp filter and its own Landlock ruleset (read anywhere, write only to
   `CACHEDIR/download-btrpkg/`, execute nothing); `btrpkg pull-ids` (root, no network) checks
   every index against the database's ID, reuses data from the installed versions and older
   cache files (read back and hashed), names the chunks still missing, and assembles the cache
   files when fetch-ids has them. Root reads the download directory only through
   `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`, regular files only, and never writes there.
   CacheServer entries are tried before Server entries; whatever one server lacks or delivers
   damaged is asked from the next. An interrupted download keeps the directory and the next run
   fetches only the rest; `-Sc`/`-Scc` remove the leftovers. In `_alpm_sync_prepare` the
   indexes are fetched and `pull-ids --plan` reports what is really missing, so the transaction
   summary and the download-space check show the real download size; fetch-ids' progress lines
   become pacman's download callbacks (progress bars).

4. `libalpm: btrpkg: use a package file that is already in the cache`: a `.pkg.tar.*` file in a
   cache directory wins over a btrpkg download (offline mirrors such as the Omarchy ISO's).

5. `libalpm: btrpkg: one root process when downloads aren't sandboxed`: without a download user
   (DisableSandbox, and pacstrap always disables the sandbox) one root `pull-ids` per server
   fetches straight into the cache files, without the download directory's second write.

6. `libalpm: btrpkg: pacman -U of .pkg.btr, -Sp without btrpkg, signify not recorded as pgp`:
   - `pacman -U NAME.pkg.btr` (and `-Qp`): the `.sig` beside it is signify, checked against the
     btrpkg keys as `LocalFileSigLevel` asks, never handed to GPGME. Every chunk's data is
     checked against the index first: a cache file pull-ids didn't assemble has had no other
     check, and before this a damaged `.pkg.btr` installed silently.
   - A transaction that only prints (`NOLOCK`, which `pacman -Sp` sets) doesn't select or plan
     btrpkg downloads: it shows the repository's file names and sizes, as stock pacman does, with
     no network I/O, as root or not. Before, as root, it fetched indexes and printed a `.pkg.btr`
     URL that no server has.
   - The signify check is recorded as extended data, `btrpkg-validation=signify:KEYFILE`
     (`pacman -Qii` shows it), and `%VALIDATION%` says `sha256` (the header-ID check), not
     `pgp`. A new `%VALIDATION%` token would make the stock pacman that shares the local
     database warn about every such package. A `btrpkg-validation` entry in a package's own
     `.PKGINFO` is dropped, so a package can't claim a check it didn't get.

7. `libalpm: btrpkg: btrparts where the database says (btrpkg.parts)`: the helpers get `--parts`
   from the database's `btrpkg.parts` (default `btrparts`), so several repositories can share one
   btrparts directory (`../btrparts`, `../../../btrparts`). A value that isn't a plain relative
   path makes the package's btrpkg data unusable. Downloads are grouped per repository and
   btrparts path.

8. `libalpm: btrpkg: fall back to the package file`: a package no server has an index for
   (`fetch-ids --index-only --skip-missing` leaves it out) is downloaded as its `.pkg.tar.*`,
   checked by sha256sum and OpenPGP like any package; so is every package of a repository whose
   btrpkg download fails. Planning does the first part, so the summary shows the real sizes. Only
   a broken download directory still fails the transaction.

9. `libalpm: btrpkg: plan each package once`: `_alpm_sync_load` planned the targets again after
   `_alpm_sync_prepare` (a second `fetch-ids --index-only` and `pull-ids --plan` per repository,
   hashing all reusable local data again).

10. `libalpm: btrpkg: the index is NAME.pkg.btr next to the package file`: `btrpkg.index` must be
    `NAME.pkg.btr` (no `/`), the repository's thin `.pkg.btr` beside `NAME.pkg.tar.zst`; the
    cache file takes the same name and the same signature. The older `index/NAME.btridx` form is
    unusable data, so such a database gets package files.

How it was checked:
- `test/btrpkg/btrtar` (libalpm's view of an archive): for all 315 packages of the set, the tar
  stream from the btrpkg cache file equals the original `.pkg.tar.zst`, entry for entry, data
  included (shadow's two `security.capability` files: the original tar carries the xattr twice,
  as LIBARCHIVE.xattr and SCHILY.xattr; the same value).
- `bench/lab-pacman-e2e.sh` on p14s (live system, reset by snapper between runs): stock pacman
  and patched pacman install the same targets; the manifests of every installed file (type,
  mode, owner, size, mtime, xattrs, sha256) and the local database entries must be identical.
  Upgrades and reinstalls with install scripts, and `bench/make-e2e-pkgs.sh`'s package: backup
  files with and without a local change (`.pacnew` in both), a large file with a
  `security.capability` xattr, a hardlink, a symlink.
- pacman's own test suite: the same 19 failures as unpatched `54d94116` on this machine
  (environment), plus the new `btrpkg` test.

Known gaps: libalpm changes the sync package's file name in place, which a long-running libalpm
client would see after the transaction; `-U` takes cache files (what `-Sw` and `btrpkg pull`
leave); `-Sp` URLs are the repository's `.pkg.tar.*` ones, which a
btrpkg-only server (the ISO's offline mirror) doesn't have; "already in the cache" checks only
that `NAME.pkg.btr` exists, so a thin index in a CacheDir is caught later, by the length check
("cache file shorter than its index says"), instead of being downloaded again.

On p14s, after each release: `bench/pacman-gaps-test.sh NAME` (patch 6, 16 checks),
`bench/pacman-robust.sh` (patches 3, 5, 8: sandbox roles, resume, failover, fallbacks, `-Sc`; 12
checks), `bench/plan-count.sh` (patch 9). Check first that the installed libalpm has the new
patch: pacman-btrpkg 8 was built without patch 10 (the PKGBUILD's patch glob missed `0010`).

Cross-checks run against this repo's reader (see PLAN, iteration 2 milestone 2): `btrpkg dump`
and `btrdump` print identical text for all 384 real indexes (51 in the omarchy test repo, 18 in
btrtest, the 315 set), the 315 transports' chunks pass `btr_check_chunk`, and 9,000 indexes from
`bench/fuzz_entries.py` get the same verdict and output, with no ASan/UBSan reports.
