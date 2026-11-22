#!/bin/bash
# Kernel (btrfs) zstd decode speed for btrparts of one zstd level, on a lab machine (root, root fs
# btrfs, usually on LUKS). W/LEVEL holds the transports; they are written into cache files with
# encoded writes (`btrpkg fetch`), then ROUNDS times: drop caches, read every cache file with cat,
# with ftrace's function profiler on btrfs's zstd_decompress_bio (one call per compressed extent)
# and dm-crypt's crypt_convert (the decryption of what is read), sleep time excluded.
#
#   kernel-decode-bench.sh W LEVEL ROUNDS [BTRPKG]
#
# Prints per round: wall time, and per function the hits and the summed time (thread time, across
# all CPUs: btrfs and dm-crypt work in kernel worker threads, in parallel).
set -euo pipefail
W=$1 l=$2 R=$3 B=${4:-btrpkg}
T=/sys/kernel/tracing
rm -rf "$W/c"; mkdir -p "$W/c"
# no compression by the mount option (compress=zstd:3): only the btrparts' own zstd extents count
chattr +m "$W/c"
for f in "$W/$l"/*.pkg.btr; do "$B" fetch --unverified "$f" "$W/c/$(basename "$f")" >/dev/null 2>&1; done
rm -rf "${W:?}/$l"
sync
restore() { echo 0 > $T/function_profile_enabled; echo > $T/set_ftrace_filter; echo 1 > $T/options/sleep-time; }
trap restore EXIT
echo 0 > $T/function_profile_enabled
echo zstd_decompress_bio > $T/set_ftrace_filter
grep -qw crypt_convert $T/available_filter_functions && echo crypt_convert >> $T/set_ftrace_filter
echo 0 > $T/options/sleep-time
TIMEFORMAT=%R
for r in $(seq "$R"); do
  echo 3 > /proc/sys/vm/drop_caches; sleep 1
  echo 1 > $T/function_profile_enabled
  wall=$( { time cat "$W/c"/*.pkg.btr > /dev/null; } 2>&1 )
  echo 0 > $T/function_profile_enabled
  stats=$(cat $T/trace_stat/function* | awk '
    $1 == "zstd_decompress_bio" || $1 == "crypt_convert" {
      t = $3; if ($4 == "ms") t *= 1000; else if ($4 == "s") t *= 1000000; else if ($4 == "ns") t /= 1000
      h[$1] += $2; s[$1] += t }
    END { printf "zstd %d calls %.3f s", h["zstd_decompress_bio"], s["zstd_decompress_bio"] / 1e6
          if ("crypt_convert" in h) printf " | crypt %d calls %.3f s", h["crypt_convert"], s["crypt_convert"] / 1e6 }')
  echo "level $l round $r: wall $wall s | $stats"
done
rm -rf "${W:?}/c"
