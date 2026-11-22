#!/bin/bash
# Kernel zstd decode per thread vs. the number of btrfs-endio workers (mount option thread_pool),
# and userspace zbench run alone, on one thread per core, and on every SMT thread.
set -uo pipefail
cd /var/tmp/zb
W=/var/tmp/zb/w T=/sys/kernel/tracing
restore() { echo 0 > $T/function_profile_enabled; echo > $T/set_ftrace_filter; echo 1 > $T/options/sleep-time; mount -o remount,thread_pool=8 /; }
trap restore EXIT
echo zstd_decompress_bio > $T/set_ftrace_filter; echo 0 > $T/options/sleep-time
TIMEFORMAT=%R
for tp in 1 8 1 8; do
  mount -o remount,thread_pool=$tp /
  echo 3 > /proc/sys/vm/drop_caches; sleep 1
  echo 0 > $T/function_profile_enabled; echo 1 > $T/function_profile_enabled
  wall=$( { time cat $W/c/*.pkg.btr > /dev/null; } 2>&1 )
  echo 0 > $T/function_profile_enabled
  cat $T/trace_stat/function* | awk -v tp=$tp -v w=$wall '$1=="zstd_decompress_bio"{t=$3; if($4=="ms")t*=1000; else if($4=="s")t*=1e6; s+=t} END{printf "kernel thread_pool=%d: wall %s s, zstd %.3f s -> %.0f MB/s per thread\n",tp,w,s/1e6,973.4/(s/1e6)}'
done
for n in 1 8 16; do
  cpus=$(if [ $n = 8 ]; then seq 0 2 14; else seq 0 $((n-1)); fi)
  for c in $cpus; do taskset -c $c ./zb-k-full frames.bin frames.sizes 3 s > u.$c & done; wait
  echo "userspace $n parallel: $(cat u.* | awk '{s+=$5; n++} END{printf "%.0f MB/s per thread (mean of %d)", s/n, n}')"; rm -f u.*
done
