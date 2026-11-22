# btrpkg: status (2026-11-22)

Done since the iteration 3 plan (details in HISTORY.md):
- Packages from CI for x86_64 and aarch64, published as the `[btrpkg]` repository; all five lab
  clients (p14s, x200, t460s, t490, macmini-m1) upgrade from it.
- Mirrors for aarch64 beside x86_64 (Omarchy, ALARM core/extra/alarm, asahi-alarm); btrparts shared
  by architectures and repositories (`btrpkg.parts`); 35% of aarch64 data was already there from
  x86_64.
- Step 7, partly: `btrpkg gc` removes old versions and unreferenced btrparts on every mirror pass.
- Fallback to the package file when no server has an index or a btrpkg download fails.
- HTTP Range for raw btrparts (no trailing zeros sent).
- The index is `NAME.pkg.btr` next to the package file (was `index/NAME.btridx`).
- A cleanup pass: five bugs fixed (double planning, hidden curl errors, gc on unreadable entries,
  mirror retries, the release db), duplicated code merged.

Open, waiting for a decision from Jonny:
- Step 6, the unattended daily update on p14s.
- Step 7, the rest: a real signing key and keyring package, signed `[btrpkg]` packages, where an
  Omarchy btrpkg repository is published (R2/S3), database signing.
- Building the btrpkg ISO (the build formats loop images; or a fork's CI).

Open, no decision needed (each its own task): one multi-server source for fetch-ids, pull-ids and
pacbtr; per-package commit in pull_many; splitting pull_many and upgrade(); one fork/exec helper in
libalpm; indexes over one connection; step 8, adopting existing installs.


# btrpkg: iteration 3 plan (2026-11-21)

Goal: make the patched pacman safe and unsurprising for someone else to run. Format version 1 stays.

1. ~~Sandbox the helper~~: two processes, fetch-ids as DownloadUser (seccomp + Landlock), pull-ids
   as root without network; root never writes into the download directory.
2. ~~Mirror failover and CacheServer~~, resume after an interrupt.
3. ~~`-Sc`/`-Scc` and old `.pkg.btr` files~~: `-Sc` works and removes leftovers; `btrpkg cache-clean`.
4. ~~Real download size and progress~~: `pull-ids --plan` in `_alpm_sync_prepare`, fetch-ids
   progress lines as download callbacks.
5. ~~A `btrpkg` package and a `pacman-btrpkg` build installed beside the system pacman~~:
   `packaging/` (btrpkg-git, pacman-btrpkg, `build.sh` in a clean container), installed on p14s,
   cache-clean timer enabled there.
6. A trial on p14s: real `-Syu` runs over several days. Started: the Omarchy mirror
   (`mirror/`), p14s's `[omarchy]` repo pointed at it, `/usr/local/bin/pacman` -> pacman-btrpkg,
   and the first `omarchy update` through btrpkg plus a reboot are done. Open: an unattended
   daily update (needs a choice: passwordless sudo for `a`, or a root timer bypassing Omarchy's
   flow).
7. Repo pipeline (Omarchy publish step, R2/S3, chunk deletion, real key, keyring package): needs a
   decision from Jonny. (2026-11-22: deletion done, `btrpkg gc`; the rest still open.)
8. Adopt existing installs.
9. ~~pacstrap with btrpkg~~: `pacstrap-btrpkg` (omarchy-iso's and archinstall's flags), Arch
   core/extra mirrors for the root image's closure. Next for the ISO: an offline mirror of `.pkg.btr`
   files and encoded read/write when the cache and the target are different filesystems (the
   root image is a loop-mounted btrfs).

Results of 1-4: HISTORY.md, "Iteration 3"; of 5: HISTORY.md, "Packages", and `packaging/README.md`.


# btrpkg: iteration 2 plan (2026-11-21)

Goal: pacman itself installs and upgrades btrpkg packages, so everything pacman does (install
scripts, backup files and `.pacnew`, conflicts, removals, NoExtract, the database) works as before,
and the client gets the speed the lab measurements point at. Format version 1 stays (Jonny,
2026-11-21): no format change in this iteration.

Iteration 1 ended with `pacbtr` (metadata package + `pacman -U --dbonly`), which has to leave
packages with an `.INSTALL` or backup files to pacman and records `%VALIDATION% none`.

## Milestones

1. **Client speed, same format.** Read back and hash installed files in parallel; one download
   queue across all packages of an upgrade instead of one package at a time. A/B against `43ffda5`
   in one run on t460s and x200 (where `pull` is 75-80% of the time).
   Done when: `pull` time drops measurably on both, results identical.
   **Done 2026-11-21** (`742894d`, `store::pull_many`): A/B against `43ffda5` in the same runs, 3
   rounds, 10 upgrades each, same bytes downloaded and written, `-Qkk` clean. `pull` t460s
   4.86 → 3.69 s (−24%), x200 17.9 → 16.0 s (−11%); whole upgrade t460s 6.54 → 5.48 s, x200
   22.55 → 21.89 s. The x200 (2 cores, no SHA or AES instructions) also decrypts the ssh tunnel
   (`bench/results/lab-20261121-i2/upgrade-*.log`).

2. **libalpm: read side.** A branch `btrpkg` of pacman at `54d94116` (Arch's 7.1.0.r9 build):
   `lib/libalpm/btrpkg.c` parses and checks a version-1 index (header, table, entries, blob; the
   same checks as `src/format.rs`) and verifies its signify signature (Ed25519 through OpenSSL's
   EVP, which libalpm already links; keys from `/etc/btrpkg/keys`, `/usr/share/btrpkg/keys`).
   Done when: a C test program accepts the Rust-made indexes of the 315 set and rejects every
   flipped bit and truncation of a sample index, as `cargo test` does.
   **Done 2026-11-21** (pacman `223f54a0`, `libalpm/0001-*.patch`): `btrpkg dump` and the C
   `btrdump` print identical text for 384 real indexes; 63 crafted indexes with valid hashes and
   bad contents (`bench/craft_indexes.py`) and 9,000 fuzzed ones (`bench/fuzz_entries.py`) get
   the same verdict from both; ASan/UBSan clean after two fixes it found (a use-after-free in an
   error message, a NULL dereference on a bad xattr count). It also found that the Rust reader
   ignored data after the first zstd frame of a section: both refuse that now.
   `meson test btrpkg`: 73 subtests. pacman's suite: same failures as unpatched.

3. **libalpm: download.** For a sync package with `btrpkg.index`/`btrpkg.id` in `%DATA%` and a
   btrfs cache directory, libalpm gets `NAME.pkg.btr` into the cache instead of `%FILENAME%`, by
   running `btrpkg pull` (one HTTP and reuse implementation; pacman already runs external
   downloaders through XferCommand). Validation: the index ID from the database and its signature.
   Done when: `pacman -Sw` of a btrpkg package leaves a checked `.pkg.btr` in the cache; packages
   without btrpkg data download as before.
   **Done 2026-11-21** (pacman `a02c75b8`): `BtrPkgHelper` in pacman.conf; `btrpkg pull-ids`
   downloads all of a repository's btrpkg targets in one queue; validation by header ID and
   signify signature (`%VALIDATION%` now `sha256 pgp` instead of iteration 1's `none`).

4. **libalpm: extraction.** `commit_single_pkg` takes its entries from the index instead of
   libarchive and keeps `extract_single_file` (backup files, `.pacnew`, NoExtract, conflicts);
   only the data path changes: FICLONERANGE from the cache file for large files, the blob for small
   ones, then owner, mode, xattrs, mtime. `.PKGINFO`, `.MTREE`, `.INSTALL` come from the index.
   Done when: `pacman -S`/`-Syu` of btrpkg packages, including ones with install scripts and backup
   files, gives the same installed tree and database as stock pacman (`-Qkk` clean, `.pacnew`
   where stock pacman makes one), in an Arch container and on p14s.
   **Done on p14s 2026-11-21** (pacman `a02c75b8`; the container part was dropped: the Arch image
   has OpenSSL 3.6, the lab and desktop 4.1, and p14s is the real target). Not as planned: no
   change to `commit_single_pkg`'s loop; `_alpm_open_archive` reads a cache file as a tar stream
   made from the index, and only `perform_extraction` changes (reflink for large files). Same
   installed files (type, mode, owner, size, mtime, xattrs, sha256) and database entries as stock
   pacman for upgrades and reinstalls of 10 packages (7 with install scripts) and a test package
   with backup files (`.pacnew` with a local change, none without), a `security.capability`
   file, a hardlink and a symlink.

5. **Package and measure.** A pacman package built from the branch for the lab; the iteration-1
   upgrade runs again with three variants: stock pacman, patched pacman, `pacbtr`.
   **Done 2026-11-21**, with the build copied to `/opt/pacbtr` instead of a package (a package
   would replace the lab machines' system pacman). Tables in HISTORY.md ("Iteration 2"): patched
   pacman 1.6-2.5x faster than stock, as fast as `pacbtr` on p14s, 0.5-0.7 s slower on t460s and
   x200; identical installed files and database entries in all 39 runs.

## After iteration 2

- ~~Run the helper inside pacman's download sandbox (DownloadUser)~~: done in iteration 3
  (fetch-ids as DownloadUser + Landlock, pull-ids as root without network).
- ~~`-Sc` for `.pkg.btr` files~~ (worked already; leftovers now removed too), `btrpkg cache-clean`
  for paccache's job. ~~`pacman -U NAME.pkg.btr`, signify recorded as `pgp`~~: patch 6 (`-U` with
  signify and a data check; `btrpkg-validation` extended data, `%VALIDATION%` sha256).
- ~~Mirror failover, CacheServer, real download size, progress~~: done in iteration 3.
- Don't change the sync package's file name in place (long-running libalpm clients).
- Measure without the ssh tunnels: the test repo is now served on port 1764 (inside ufw's open KDE
  Connect range), reachable from the lab directly. x200 and t460s are on Wi-Fi (14 and 76 MB/s),
  p14s wired (118 MB/s): rerun the iteration-2 tables that way.

## Out

ISO and `adopt` (Jonny: wait), fs-verity, packs, an `undo` frontend, other filesystems, Arch's
packages, a format change.

## Open questions

- Downloading in C (libcurl multi in `dload.c`) instead of the `btrpkg pull` helper: decide after
  milestone 3, with numbers.


# btrpkg: iteration 1 plan

Goal: one real upgrade loop, end to end, on lab machines. Omarchy's own packages are published
in both formats, a btrfs client upgrades through btrpkg with a snapshot first, pacman's database
stays correct, and the result is measured against plain pacman.

State before this iteration: a spike (1,211 lines of Rust, no tests) with `convert`, `fetch`,
`install`, `info`, `publish`, `pull` on local directories. Format version 0, no signatures, no
HTTP, no pacman integration. See HISTORY.md for the measurements.

## Decisions already made (README)

- Rollback: snapper snapshot of `@` before the transaction, install live, roll back on failure.
- Repo: `.pkg.tar.zst` stays; index + chunks next to it; one `omarchy.db` with
  `btrpkg.index=` / `btrpkg.index_sha256=` in `%DATA%`.
- Hash: SHA-256. Signatures: plain Ed25519, keys as files (`/usr/share/btrpkg/keys/`,
  `/etc/btrpkg/keys/`), no GPG for the btrpkg path.
- ISO integration: waits.

## Scope

In:
- Omarchy's own packages only (pkgs.omarchy.org, edge). That avoids re-signing Arch's packages,
  the open trust decision.
- Client in Rust, as a separate `btrpkg` command; pacman via step 1 (`--dbonly` metadata package).

Out (later iterations): the libalpm patch, ISO and `adopt`, fs-verity, packs, an `undo` command or
frontend, filesystems other than btrfs, Arch's packages.

## Milestones

1. **Format v1, with tests.** Done 2026-11-19 (see README/HISTORY.md, "Format (version 1)").
   Header with format version, hash and signature algorithm IDs, section lengths and a SHA-256
   per section. Order: header, chunk table, entries, small-file blob. Per chunk: the stored-bytes
   hash plus the uncompressed-data hash. Unit tests for round trips and for a flipped byte in
   every section.
   Done when: the 315-package set converts and installs with 0 differences (as in v0), and every
   corruption test is rejected before anything is written.

2. **Signing and keys.** Done 2026-11-20 (`src/sign.rs`): signify's file formats, so `signify -V`
   verifies a btrpkg signature (checked in an Arch container, signify 33). The signed message is
   the 256-byte header; its magic and version are the namespace, so no separate namespace string.
   `btrpkg keygen`, `publish --key`, Ed25519 over the header with the namespace
   `btrpkg-index-v1`; the signature names its key file. A revoked-keys file in the key directory.
   Sign in `publish`, after any raw-map remapping: remapping rewrites the table and so the header,
   so a signature made at convert time would not match the published index. Model: OpenBSD
   signify (Ed25519, keys as files) in Alpine apk v3's structure (one signed metadata block that
   holds every hash).
   Done when: a valid index passes, and a tampered index, an unknown key and a revoked key fail.

3. **Repo tool.** Done 2026-11-20 (`src/repo.rs`, `btrpkg repo-update`). Test repo
   `~/Work/btrpkg-repo/omarchy/x86_64`: the 51 Omarchy packages installed on p14s, x200, t490 and
   t460s (current edge, 851 MB), all gpgv-checked and converted in 4 min (866 MB of chunks, 7.6 MB
   of indexes). In an Arch container `pacman -Si` of all 51 is byte-identical against the original
   database with no warnings, and signed installs (`SigLevel = Required`) work, `-Qkk` clean.
   `btrpkg repo-update DIR`: for each `.pkg.tar.zst`, check its OpenPGP signature (`gpgv`, pacman's
   keyring), convert, publish chunks, then index and signature, then write the database with the
   `%DATA%` fields. Publish order: chunks, indexes, database last. Keep the `.pkg.tar.zst` files.
   Done when: plain pacman installs from the resulting repo exactly as before.

4. **Client over HTTP.** Done 2026-11-20 (`src/net.rs`, `btrpkg pull DIR|URL`): libcurl multi,
   64 requests in flight over HTTP/2 (h2c with `--h2c`), 8 connections over HTTP/1.1, 3 attempts per
   object. Chunks come from an older cache file of the same package, then from installed files
   (`--installed ROOT`, checked against the unencoded hash, compressed here), then the server.
   The 9 Hyprland-set upgrades through caddy (h2c) download 7.74 MB (0.24 MB indexes) in both
   cases, against 55.9 MB of `.pkg.tar.zst`; hyprland itself 144 chunks, 5.50 MB. The installed
   result matches bsdtar of the new packages (785 files, 0 differences).
   `pull` from an HTTP(S) store: HTTP/2 with many requests in flight, retries, chunk reuse from the
   cache (exists), and reuse from installed files through the uncompressed hash, so a package
   installed from `.pkg.tar.zst` doesn't need a full download.
   Done when: the hyprland 0.57.1 → 0.57.2 upgrade downloads about 5.7 MB from a cache made by
   btrpkg and from a `.pkg.tar.zst` install alike.

5. **Upgrade through pacman (step 1).** Done 2026-11-20 (`src/upgrade.rs`, `btrpkg upgrade`, or
   `pacbtr` as a hardlink). Dependencies and conflicts: `pacman -U --print` of the metadata packages
   (`--print` can't be combined with `--dbonly`). Rollback: `snapper undochange N..0`, then the
   snapshot's modification times put back on every touched path (undochange restores contents,
   owners and modes but stamps the current time, which `pacman -Qkk` reports). On p14s through the
   test repo: a failure injected after 3 of 5 packages leaves `snapper status 3..0` empty and
   `-Qkk` clean; the real upgrade of the same 5 (mise-bin, omarchy-dev, omarchy-local-ai,
   omarchy-themes-intros, owe) leaves `-Qkk` clean, `pacman -Dk` clean and nothing pending.
   Packages left to pacman: no btrpkg data, an `.INSTALL`, or backup files (old or new version).
   Known: the database records `%VALIDATION% none` (an unsigned local metadata package).
   `btrpkg upgrade PKG...`: snapshot, verify, pull, check file conflicts against the local
   database, place files (temporary name, then rename), remove files the old version had and the
   new one doesn't, then `pacman -U --dbonly` with the metadata package (hooks run there).
   Only packages without `.INSTALL` and without backup files; the rest go to pacman unchanged
   (`--dbonly` would run `pre_upgrade` after the files are in place, and skips `.pacnew`).
   Done when: after an upgrade `pacman -Qkk` is clean, a later `pacman -Syu` sees nothing odd,
   and a failure injected mid-upgrade is undone by the snapshot.

6. **Measure on the lab.** Done 2026-11-21: p14s, t460s, t490, x200; tables in HISTORY.md
   ("Iteration 1"). Upgrades 1.4-2.1x faster with a fifth to half of the download and 3.5-6.5x fewer writes;
   fresh installs of the 315 set 1.1-1.5x faster with half the writes.
   The same upgrades through pacman and btrpkg on several lab machines: bytes downloaded, time,
   bytes written, disk used, plus a fresh install of a package set. Results in HISTORY.md and a
   History bullet in the infrastructure notes.

## Review findings to fix before milestone 2 (2026-11-20)

Fixed 2026-11-20 with tests (`cargo test`: 14), except "`pull` opens every cache file", which
milestone 4 replaces. Checked as root on p14s (LUKS2 + btrfs, throwaway subvolumes,
`bench/verify-host.sh`): the 315 set fetches with encoded writes and installs with 0 differences
against bsdtar.

From a review of `eca5b8b`; the first three are shown by working proofs of concept.
- **Critical: `install` follows symlinks the package created.** A symlink `a` → `/etc`, then a
  file `a/passwd`, writes outside the root; so do a directory entry over a symlink (chmod follows
  it) and hardlinks to existing files. Resolve every path from a root dirfd without following
  symlinks (`openat2` `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS`, `fchmodat`/`fchownat`). Hardlink
  targets must be earlier files of the same package. The bug has been there since version 0.
- **High: the uncompressed hash is never checked**, but `pull` reuses chunks by it, so one
  self-consistent package can plant bad data in a later one. Decompress every zstd chunk before
  `ENCODED_WRITE` (window ≤ 2^17, exactly `unencoded_len` bytes, hash = `raw`), and check reused
  ranges or only reuse from verified cache files.
- **High: `fetch` writes chunks before the index is authenticated.** `--sha256` is checked
  after the writes and is optional. Check an expected header ID (later: the signature) right after
  reading the index.
- **Medium:**
  - Decompression bomb: `MAX_RAW` 16 GiB per section; a 33 KB index reached 1.5 GB RSS.
  - `align` overflows on huge sizes.
  - `install` leaves `.btrpkg-new` files behind on errors.
  - The outer-zstd reader allocates from frame content sizes and allows a 2 GiB window.
- **Low:**
  - No fsync before renames.
  - `unhex` panics on non-ASCII text.
  - `publish` trusts existing store chunks.
  - `with_extension("part")` can make two cache files share one temp name.
  - `pull` opens every cache file in the directory on every pull.
- **Tests to add:** symlink escape, a wrong uncompressed hash, reuse from a bad cache file, a bad
  raw-map entry, cleanup after a failed install.

## Risks

- The `--dbonly` route: scriptlet order and the skipped checks. Mitigated by the package filter
  in milestone 5; the real fix is the libalpm patch (iteration 2).
- Encoded writes take frames from userspace and need root; nothing is written before the
  signature and hash checks.
- pkgs.omarchy.org serves only HTTP/1.1: many small requests are slow until HTTP/2 or HTTP/3 is on.
- Frame bytes depend on the zstd version: the server's `raw/` map and the uncompressed hash cover it.

## Answered

- Test repo: a local HTTP server on the desktop (Jonny, 11-19). The lab machines reach it over the
  LAN. pkgs.omarchy.org's HTTP/1.1-only limit is measured separately (HISTORY.md).
- Packages: Omarchy's own only (Jonny, 11-19).
- Client language: Rust for iteration 1 (default; not answered yet). C only for the libalpm patch.

## Open questions

- Which lab machines; they are busy with the night run until 09:00 on 11-20.
