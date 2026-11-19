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
btrpkg convert [--outer-level 19] in.pkg.tar.zst out.pkg.btr.zst           # raw chunks, solid zstd
btrpkg fetch [--no-encoded] [-l 3] [--sha256 HEX] IN|- /var/cache/pacman/pkg/x.pkg.btr   # root
btrpkg install /var/cache/pacman/pkg/x.pkg.btr /                          # reflinks, falls back to copy
btrpkg info x.pkg.btr
```

`fetch` checks integrity in one of two ways:
- `.pkg.btr` carries a sha256 of its contents, checked on the way in.
- For `.pkg.btr.zst`, `fetch` hashes the downloaded file itself (434 MB rather than 2.1 GB of
  inner stream) and compares it with `--sha256`, which plays the role of the repo database or
  signature. The zstd frame checksum catches corruption.

`fetch` also refuses trailing data. On any failure it removes its partial file.

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
- LUKS (the real system), network, and signatures. sha256 is checked after the encoded writes, but
  before anything reads the data.
- Upgrades: replacing files, reflinking unchanged extents from the previous version
  (delta downloads).
- Hostile input: the kernel decompresses frames that userspace supplied. A signature check must
  happen before install, and `fetch` must keep refusing to install unverified files.
