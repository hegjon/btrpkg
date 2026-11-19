# btrpkg (spike)

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

```sh
cargo build --release
btrpkg convert [-l 19] [--small 4096] in.pkg.tar.zst out.btrpkg   # post-processor, any machine
btrpkg fetch [--no-encoded] out.btrpkg|- /var/cache/pacman/pkg/x.btrpkg   # root, btrfs
btrpkg install /var/cache/pacman/pkg/x.btrpkg /                     # reflinks, falls back to copy
btrpkg info x.btrpkg
```

## Format (version 0, spike-grade)

Transport file:

| part | contents |
|---|---|
| header (64 B) | `BTRPKG\0\0`, version, flags, meta_len, nchunks, image_len, image_base, meta_raw_len |
| manifest | one zstd frame (level 19): entries (kind, mode, uid, gid, mtime, size, data offset, small flag, path, link target, xattrs) followed by the small-file blob |
| chunk table | nchunks × 24 B: image_off, unencoded_len (4 KiB multiple, ≤128 KiB), encoded_len, compression (0 raw, 2 zstd) |
| chunk data | chunk payloads in table order. zstd frames use window ≤128 KiB, no checksum and no dictionary ID |
| digest | sha256 of all preceding bytes (stand-in for a signature) |

Fetched (cache) file: the header with `flags=1` and `image_base` set, then the manifest, table and
digest, then zero padding up to `image_base` (4 KiB-aligned), then the image, whose chunks are
btrfs compressed extents.

Converter rules:
- Files of `--small` bytes or less go in the blob.
- Identical file contents are stored once and reflinked more than once (dedup).
- A chunk stays raw when compressing it doesn't save at least one 4 KiB block.
- Root-level dotfiles (`.PKGINFO`, `.MTREE`, …) are kept in the manifest and not installed.

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
- Download is +15% to +32% against Arch's `.pkg.tar.zst`. The btrfs 128 KiB window is a hard
  limit; levels beyond 19 don't help.
- A higher small-file threshold shrinks the download but writes more at install, and still beats
  pacman at 64 KiB.

Not covered yet:
- pacman integration: DB, hooks, scriptlets, backup files, conflicts. Baseline is bsdtar, not
  `pacman -U`.
- LUKS (the real system), network, and signatures. sha256 is checked after the encoded writes, but
  before anything reads the data.
- Upgrades: replacing files, reflinking unchanged extents from the previous version
  (delta downloads).
- Hostile input: the kernel decompresses frames that userspace supplied. A signature check must
  happen before install, and `fetch` must keep refusing to install unverified files.
