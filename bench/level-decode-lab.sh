#!/bin/bash
# zstd level decode benchmark on one lab host: copy the sets, decode, clean up.
#   run-host.sh HOST SETS...      (SETS: full and/or small)
h=$1; shift
W=$TMPDIR/lvl; out=$W/out/$h.txt
mkdir -p $W/out
{
  echo "== $h: $(ssh root@$h 'echo "$(lscpu | sed -n "s/Model name: *//p" | tr "\n" " ")| $(nproc) threads | $(free -m | awk "/Mem/{print \$2}") MB | governor $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null)"')"
  ssh root@$h 'rm -rf /var/tmp/lvl; mkdir -p /var/tmp/lvl'
  scp -q $W/frame-decode-bench.py root@$h:/var/tmp/lvl/
  for s in "$@"; do
    rsync -a $W/$s root@$h:/var/tmp/lvl/
    for l in 3 12 15 19; do
      echo "$s level $l: $(ssh root@$h "cd /var/tmp/lvl/$s && python3 ../frame-decode-bench.py d$l 5 $l/*.pkg.btr")"
    done
    ssh root@$h "rm -rf /var/tmp/lvl/$s"
  done
  ssh root@$h 'rm -rf /var/tmp/lvl'
} > $out 2>&1
echo "$h done"
