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

Known gaps: the helper runs as root, outside pacman's download sandbox; `-U` of a `.pkg.btr` file
is not handled; `-Sc` doesn't know `.pkg.btr` files; the local database records the signify
check as `pgp` (the format has no other name for a signature); libalpm changes the sync package's
file name in place, which a long-running libalpm client would see after the transaction.

Cross-checks run against this repo's reader (see PLAN, iteration 2 milestone 2): `btrpkg dump`
and `btrdump` print identical text for all 384 real indexes (51 in the omarchy test repo, 18 in
btrtest, the 315 set), the 315 transports' chunks pass `btr_check_chunk`, and 9,000 indexes from
`bench/fuzz_entries.py` get the same verdict and output, with no ASan/UBSan reports.
