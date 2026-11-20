# btrpkg: iteration 1 plan

Goal: one real upgrade loop, end to end, on lab machines. Omarchy's own packages are published
in both formats, a btrfs client upgrades through btrpkg with a snapshot first, pacman's database
stays correct, and the result is measured against plain pacman.

State before this iteration: a spike (1,211 lines of Rust, no tests) with `convert`, `fetch`,
`install`, `info`, `publish`, `pull` on local directories. Format version 0, no signatures, no
HTTP, no pacman integration. See README for the measurements.

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

1. **Format v1, with tests.** Done 2026-11-19 (see README, "Format (version 1)").
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

5. **Upgrade through pacman (step 1).**
   `btrpkg upgrade PKG...`: snapshot, verify, pull, check file conflicts against the local
   database, place files (temporary name, then rename), remove files the old version had and the
   new one doesn't, then `pacman -U --dbonly` with the metadata package (hooks run there).
   Only packages without `.INSTALL` and without backup files; the rest go to pacman unchanged
   (`--dbonly` would run `pre_upgrade` after the files are in place, and skips `.pacnew`).
   Done when: after an upgrade `pacman -Qkk` is clean, a later `pacman -Syu` sees nothing odd,
   and a failure injected mid-upgrade is undone by the snapshot.

6. **Measure on the lab.**
   The same upgrades through pacman and btrpkg on several lab machines: bytes downloaded, time,
   bytes written, disk used, plus a fresh install of a package set. Results in the README and a
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
  LAN. pkgs.omarchy.org's HTTP/1.1-only limit is measured separately (README).
- Packages: Omarchy's own only (Jonny, 11-19).
- Client language: Rust for iteration 1 (default; not answered yet). C only for the libalpm patch.

## Open questions

- Which lab machines; they are busy with the night run until 09:00 on 11-20.
