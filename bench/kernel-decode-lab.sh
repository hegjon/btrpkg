#!/bin/bash
# Run bench/kernel-decode-bench.sh on one lab host for LEVELS (default "3 12 15 19"; 3 rounds each).
#   kernel-decode-lab.sh HOST [BTRPKG_BINARY_TO_COPY]
h=$1 bin=${2:-}
W=$TMPDIR/kz; out=$W/out/$h.txt; mkdir -p $W/out
{
  echo "== $h: $(ssh root@$h 'r=$(findmnt -no SOURCE / | sed "s/\[.*//"); echo "$(lscpu | sed -n "s/Model name: *//p" | tr "\n" " ")| $(nproc) threads | aes-ni $(grep -qw aes /proc/cpuinfo && echo yes || echo no) | $(cryptsetup status ${r#/dev/mapper/} 2>/dev/null | awk "/cipher:|sector size:/{printf \"%s %s \", \$(NF-1), \$NF}") | $(uname -r)"')"
  ssh root@$h 'rm -rf /var/tmp/kz; mkdir -p /var/tmp/kz'
  scp -q ~/Work/btrpkg/bench/kernel-decode-bench.sh root@$h:/var/tmp/kz/
  B=btrpkg
  if [[ -n $bin ]]; then scp -q "$bin" root@$h:/var/tmp/kz/btrpkg; B=/var/tmp/kz/btrpkg; fi
  for l in ${LEVELS:-3 12 15 19}; do
    rsync -a $W/$l root@$h:/var/tmp/kz/
    ssh root@$h "bash /var/tmp/kz/kernel-decode-bench.sh /var/tmp/kz $l 3 $B"
  done
  ssh root@$h 'rm -rf /var/tmp/kz'
} > $out 2>&1
echo "$h done"
