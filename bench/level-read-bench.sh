#!/bin/bash
# Cold reads through btrfs of cache files whose btrparts were written at different zstd levels
# (root, on btrfs). For each level dir under W: fetch every transport into W/cLEVEL with encoded
# writes, then ROUNDS times: drop caches, cat all cache files to /dev/null; prints real time, cat's
# sys time, and the CPU time of the whole machine (/proc/stat: btrfs decompresses compressed
# reads in kernel worker threads, on several cores, which cat's own times don't include).
#
#   level-read-bench.sh W ROUNDS LEVEL...
set -euo pipefail
W=$1 ROUNDS=$2; shift 2
for l in "$@"; do
  rm -rf "$W/c$l"; mkdir -p "$W/c$l"
  for f in "$W/$l"/*.pkg.btr; do btrpkg fetch --unverified "$f" "$W/c$l/$(basename "$f")" >/dev/null 2>&1; done
  echo "level $l: $(compsize -b "$W/c$l" | awk '$1=="TOTAL"{print "on disk " $3 " of " $4 " bytes (" $2 ")"}')"
done
sync
busy() { awk '/^cpu /{print $2+$3+$4+$7+$8}' /proc/stat; }
hz=$(getconf CLK_TCK)
TIMEFORMAT='%R %S'
for r in $(seq "$ROUNDS"); do
  for l in "$@"; do
    echo 3 > /proc/sys/vm/drop_caches; sleep 1
    b0=$(busy)
    t=$( { time cat "$W/c$l"/*.pkg.btr > /dev/null; } 2>&1 )
    b1=$(busy)
    echo "round $r level $l: real ${t% *} s, cat sys ${t#* } s, machine CPU $(awk -v d=$((b1 - b0)) -v hz="$hz" 'BEGIN{printf "%.2f", d/hz}') s"
  done
done
