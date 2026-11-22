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

Level sweep (same packages; convert on the desktop with 32 threads; decode one core, best of 5):
  level  stored MB  convert s  decode desktop MB/s  decode p14s MB/s
   3      429.8       3.6        2070                1245
   9      407.9       4.3        2074                1243
  12      406.4       5.0        2071                1228
  13      401.2       5.1        1759                   -
  14      388.4       5.8        1573                   -
  15      387.7       6.2        1578                 976
  16      385.3       7.0        1538                   -
  17      385.2       7.7        1538                 970
  19      384.4      10.9        1539                 964
Decode speed drops at 13-14 (zstd's optimal parsers) and stays flat from 14 to 19.

Kernel, p14s, machine CPU time for a cold read of all 8 (5 rounds, median; wall time 0.71-0.75 s
for all):  level 3 2.03 s, 12 1.96 s, 15 2.16 s, 19 2.23 s.  On disk: 458.8 / 435.8 / 417.1 / 413.7 MB.

Decode speed on every lab machine (2026-11-22, bench/level-decode-lab.sh; MB/s, one core, best of 5;
"small" = mesa, qt6-base, ghostscript, 156 MB, so turbox (1 GB RAM) could run it; "full" = the 8 packages):
  host        CPU                                  small: L3   L12   L15   L19   full: L3   L12   L15   L19   L15/L12
  desktop     AMD Ryzen 9 9950X 16-Core Processor       2198  2187  1720  1678      2059  2066  1582  1540   0.79
  p14s        AMD Ryzen 7 PRO 5850U with Radeon Gr      1292  1333  1059  1038      1266  1250   985   970   0.79
  macmini-m1  Icestorm-M1 BIOS CPU @ 0.0GHz Firest      1217  1215   975   952      1184  1182   942   913   0.80
  galaxybook  Oryon BIOS Snapdragon(R) X Elite - X      1176  1146   913   889      1143  1108   877   849   0.80
  t490        Intel(R) Core(TM) i5-8265U CPU @ 1.6      1011  1000   797   749       972   925   731   711   0.80
  t460s       Intel(R) Core(TM) i7-6600U CPU @ 2.6       920   910   728   708       887   879   690   670   0.80
  t550        Intel(R) Core(TM) i5-5300U CPU @ 2.3       787   772   628   613       760   747   600   583   0.81
  xps         Intel(R) Core(TM) i5-5300U CPU @ 2.3       785   771   627   611       758   746   601   582   0.81
  samsung-i5  Intel(R) Core(TM) i5-3337U CPU @ 1.8       570   569   469   459       551   549   450   438   0.82
  x200        Intel(R) Core(TM)2 Duo CPU     P8400       294   286   244   238       285   279   236   229   0.85
  hp-amd      AMD A6-7310 APU with AMD Radeon R4 G       289   285   240   234       281   277   231   224   0.84
  chromebook  Intel(R) Celeron(R) CPU  N2840  @ 2.       267   267   223   214       259   259   215   210   0.84
  turbox      Intel(R) Core(TM)2 CPU         T5500       214   207   174   171         -     -     -     -   0.84
Every machine: L12 decodes as fast as L3; L15 is 15-21% slower than L12; L19 another 2-6% slower than L15.

Kernel (btrfs) decode, every lab machine (bench/kernel-decode-lab.sh, bench/kernel-decode-bench.sh):
cache files written with encoded writes (chattr +m: only the btrparts' own zstd extents), cold reads
(drop_caches), ftrace function profiler on zstd_decompress_bio (thread time, sleep excluded) and
crypt_convert (dm-crypt decryption, every root is LUKS2 aes-xts-plain64). Full 8-package set, median of 3.
MB/s = unencoded zstd bytes / summed zstd_decompress_bio time (one core's worth).
  host        AES-NI  LUKS sector  L3   L12  L15  L19 MB/s  L15/L12  decrypt s L12/L15  decrypt/decompress at L12
  p14s        yes      4096       827  883  710  676      0.80     0.16/0.16        0.15
  t490        yes      4096       513  503  405  401      0.81     0.17/0.16        0.09
  t460s       yes      4096       456  450  361  347      0.80     0.18/0.17        0.09
  t550        yes       512       396  381  307  299      0.81     0.46/0.43        0.18
  xps         yes       512       380  371  301  292      0.81     0.48/0.46        0.18
  galaxybook  yes      4096       356  360  283  275      0.79     0.20/0.21        0.08
  x200        no       4096       282  276  233  225      0.84     5.63/5.19        1.59
  samsung-i5  yes      4096       253  252  212  206      0.84     0.41/0.39        0.11
  chromebook  no        512       234  232  195  190      0.84     9.40/8.99        2.24
  hp-amd      yes      4096       192  190  166  160      0.87     0.51/0.46        0.10
  turbox      no       4096       168  171  143  139      0.84     8.22/7.86        1.44
  macmini-m1: its kernel has no function tracer (CONFIG_FUNCTION_TRACER not set); machine CPU time of
  the cold read instead (level-read-bench.sh, 3 rounds): L3 1.95-2.04 s, L12 1.84-1.90, L15 1.93-2.40, L19 2.07-2.24.
- The kernel's zstd decodes at 46-71% of userspace libzstd's speed on the 2013-2021 x86 laptops, 83-99% on the
  oldest/smallest CPUs (x200, turbox, chromebook), and 32% on the Snapdragon X Elite (galaxybook).
- L15 is 13-21% slower than L12 in the kernel too; L12 is as fast as L3.
- LUKS: with AES-NI decryption costs 8-18% of the decompression time (at L12) (4 KiB sectors 2.5x cheaper than 512 B:
  t460s 0.18 s vs t550/xps 0.46 s). Without AES-NI (x200, turbox, chromebook) decryption costs more than
  decompression (1.4-2.2x). L15's smaller data saves 0.36-0.44 s of decryption there but costs 0.66-1.13 s more
  decompression: L12 still needs less CPU in total on every machine.

Kernel decode, levels 11-13 (same method, same machines minus macmini-m1, level 12 measured again):
  stored: L11 406.8 MB, L12 406.4 MB, L13 401.2 MB (L15 387.7, L3 429.8)
  host        L11  L12  L13 MB/s   L13/L12   (L12 in the first run)
  p14s         805  863  787        0.91     (883)
  t490         509  504  475        0.94     (503)
  t460s        448  451  406        0.90     (450)
  t550         381  380  341        0.90     (381)
  xps          372  372  339        0.91     (371)
  galaxybook   360  358  325        0.91     (360)
  x200         276  276  254        0.92     (276)
  samsung-i5   253  253  233        0.92     (252)
  chromebook   223  233  214        0.92     (232)
  hp-amd       190  191  180        0.94     (190)
  turbox       169  166  158        0.95     (171)
L11 decodes as fast as L12 (within 1%, p14s and chromebook 4-7% off in this run) and is not smaller;
L13 is 5-10% slower than L12 and 1.3% smaller. L12 reproduced within 2% of the first run.

BMI2 probe per context (Phoronix, 2026-08-26): on p14s, level 12, zstd_init_dstream (btrfs calls it once per
compressed extent; it runs the CPUID probe) took 1.5 ms for 12,910 calls (0.12 us each) against ~1.2 s of
decompression: 0.13%. zstd_decompress_stream ran ~25 times per extent (318k calls: btrfs hands zstd one
4 KiB output page at a time).
