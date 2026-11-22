zstd level 3 vs 19 btrparts, same 8 packages (libreoffice-fresh, llvm-libs, mesa, nodejs, qt6-base,
noto-fonts, ghostscript, ffmpeg): 1.02 GB of image, 13,643 btrparts (128 KiB frames).
Stored: level 3 429.8 MB (12,868 zstd + 775 raw), level 19 384.4 MB (12,921 zstd + 722 raw), -10.6%.

Userspace decode, one core, libzstd via Python 3.14 compression.zstd (bench/frame-decode-bench.py),
best of 5:
  desktop Ryzen 9 9950X:   level 3 2062 MB/s   level 19 1545 MB/s   (-25%)
  p14s Ryzen 7 PRO 5850U:  level 3 1274 MB/s   level 19  961 MB/s   (-25%)

Kernel, p14s, cache files written with encoded writes (bench/level-read-bench.sh), cold read
(drop_caches) of all 8 with cat, 5 rounds alternating:
  on disk (compsize): level 3 458.8 MB (44%), level 19 413.7 MB (40%)
  wall time:          level 3 0.70-0.73 s,  level 19 0.70-0.72 s   (no difference)
  cat's sys time:     0.42-0.43 s both
  machine CPU time:   level 3 1.78-2.15 s (median 1.86), level 19 2.00-2.53 s (median 2.30)  (+24%)
btrfs decompresses in kernel worker threads on several cores, so the wall time doesn't change;
the extra decode work shows only as CPU time.
