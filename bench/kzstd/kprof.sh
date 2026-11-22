#!/bin/bash
# One cold read of W/c with the function profiler on zstd_decompress_bio (sleep time excluded) and
# perf counting cycles and instructions per symbol (kernel only). Prints the profiler time and the
# cycles/instructions summed over the zstd decode functions.
set -euo pipefail
W=${W:-/var/tmp/zb/w} T=/sys/kernel/tracing
restore() { echo 0 > $T/function_profile_enabled; echo > $T/set_ftrace_filter; echo 1 > $T/options/sleep-time; }
trap restore EXIT
echo 0 > $T/function_profile_enabled; echo zstd_decompress_bio > $T/set_ftrace_filter; echo 0 > $T/options/sleep-time
echo 3 > /proc/sys/vm/drop_caches; sleep 1
echo 1 > $T/function_profile_enabled
perf record -q -a -e '{cycles:k,instructions:k}' -o /var/tmp/zb/k.data -- cat "$W"/c/*.pkg.btr > /dev/null 2>/dev/null
echo 0 > $T/function_profile_enabled
cat $T/trace_stat/function* | awk '$1=="zstd_decompress_bio"{t=$3; if($4=="ms")t*=1000; else if($4=="s")t*=1e6; s+=t; h+=$2} END{printf "profiler: %d calls %.3f s\n",h,s/1e6}'
perf report -i /var/tmp/zb/k.data --group --sort sym -F period,sym --stdio 2>/dev/null | grep -v '^#' | grep -v '^$' > /var/tmp/zb/k.txt
