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
btrpkg convert [--outer-level 19] [--frames N] [--min-frame B] in.pkg.tar.zst out.pkg.btr.zst  # raw chunks, solid zstd
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
- `index/x.btridx`: header, manifest, chunk table with a hash per chunk, and a sha256 of all of it
  (stand-in for a signature; `pull --sha256` checks the index file like a repo database entry).
  Chunks are written before the index, so a published index never refers to a missing chunk.

`pull` reads the index and builds a map of the chunks the cache already has from the `.btridx`
copies kept next to every pulled cache file. It reflinks those from the old cache file and reads
the rest from the store. Each chunk is checked against its hash before it is written as an encoded
extent. The result is the same cache file `fetch` makes, so `install` is unchanged. On any error
the partial file is removed.

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
