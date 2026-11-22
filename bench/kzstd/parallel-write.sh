#!/bin/bash
# Encoded writes of 8 packages' btrparts into cache files: one after another vs all at once.
cd /var/tmp/pw; TIMEFORMAT=%R
one() { for f in 12/*.pkg.btr; do btrpkg fetch --unverified "$f" c/$(basename "$f") >/dev/null 2>&1; done; }
par() { for f in 12/*.pkg.btr; do btrpkg fetch --unverified "$f" c/$(basename "$f") >/dev/null 2>&1 & done; wait; }
for r in 1 2 3; do for m in one par; do
  rm -rf c; mkdir c; chattr +m c; sync; echo 3 > /proc/sys/vm/drop_caches; cat 12/* > /dev/null
  w=$( { time $m; } 2>&1 ); s=$( { time sync; } 2>&1 )
  echo "$m round $r: writes $w s, then sync $s s"
done; done
du -sh c; compsize c 2>/dev/null | tail -2; rm -rf c
