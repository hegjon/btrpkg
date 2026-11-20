#!/bin/bash
# Fresh install of a package set on a lab machine's own btrfs (LUKS2 + btrfs as installed), no
# formatting: throwaway subvolumes, the cache one inside @pkg, the root one inside @.
#   root@host: bash lab-fresh.sh DIR OUTDIR [ROUNDS]
# DIR: pkg/*.pkg.tar.zst, btr/*.btrpkg, set.txt (as for verify-host.sh); staged into /dev/shm first
# so reading the inputs costs no disk I/O.
#   pacman: cp .pkg.tar.zst into the cache (the download), bsdtar -xp into the root
#   btrpkg: btrpkg fetch (encoded writes) into the cache, btrpkg install (reflinks) into the root
# Records wall time incl. the final sync, bytes written to the root's partition, data space used
# (cache + root, and root alone after deleting the cache).
set -uo pipefail
D=$(realpath "$1"); OUT=$2; ROUNDS=${3:-3}; B=${B:-/usr/local/bin/btrpkg}; SCEN=${SCEN:-pacman btrpkg}
mkdir -p $OUT; exec > >(tee -a $OUT/log) 2>&1
CACHE=/var/cache/pacman/pkg/btrpkg-fresh; R=/btrpkg-fresh; S=/dev/shm/btrpkg-fresh
for s in $CACHE $R; do [[ -e $s ]] && { echo "$s exists, refusing"; exit 1; }; done
part=$(lsblk -sno NAME,TYPE $(findmnt -no SOURCE / | sed 's/\[.*//') | awk '$2=="part"{gsub(/[^a-zA-Z0-9]/,"",$1); print $1; exit}')
written() { awk '{print $7*512}' /sys/class/block/$part/stat; }
used() { btrfs filesystem sync / >/dev/null; btrfs filesystem usage -b / 2>/dev/null | grep -m1 '^Data,' | grep -o 'Used:[0-9]*' | cut -d: -f2; }
now() { date +%s.%N; }
cleanup() { btrfs subvolume delete $CACHE $R >/dev/null 2>&1; rm -rf $S; }
trap cleanup EXIT
mkdir -p $S/pkg $S/btr
while read -r f; do cp $D/pkg/$f $S/pkg/; cp $D/btr/${f%.pkg.tar.zst}.btrpkg $S/btr/; done <$D/set.txt
echo "host $(hostname) part $part, staged $(du -sb $S/pkg | cut -f1) + $(du -sb $S/btr | cut -f1) bytes"
run() {
  btrfs subvolume create $CACHE >/dev/null; btrfs subvolume create $R >/dev/null
  sync; echo 3 > /proc/sys/vm/drop_caches; sleep 2
  local u0 w0 a b w1 u1 u2 ok=0
  u0=$(used); w0=$(written); a=$(now)
  while read -r f; do
    n=${f%.pkg.tar.zst}
    if [[ $1 == pacman ]]; then
      cp $S/pkg/$f $CACHE/ && bsdtar -xpf $CACHE/$f -C $R --exclude .PKGINFO --exclude .BUILDINFO --exclude .MTREE --exclude .INSTALL --exclude .CHANGELOG || ok=1
    else
      $B fetch --unverified --no-fsync $S/btr/$n.btrpkg $CACHE/$n.pkg.btr 2>/dev/null && $B install $CACHE/$n.pkg.btr $R 2>/dev/null || ok=1
    fi
  done <$D/set.txt
  sync; b=$(now); w1=$(written); u1=$(used)
  btrfs subvolume delete $CACHE >/dev/null; btrfs subvolume sync / >/dev/null 2>&1; u2=$(used)
  printf '%s\t%s\t%s\t%.2f\t%s\t%s\t%s\n' $1 $2 $ok $(awk -v a=$a -v b=$b 'BEGIN{print b-a}') $((w1-w0)) $((u1-u0)) $((u2-u0)) | tee -a $OUT/results.tsv
  btrfs subvolume delete $R >/dev/null; btrfs subvolume sync / >/dev/null 2>&1
}
printf 'scenario\tround\tfailed\tseconds\twritten\tspace_cache_root\tspace_root\n' | tee $OUT/results.tsv
for r in $(seq 1 $ROUNDS); do
  order=$SCEN; (( r % 2 )) || order=$(echo $SCEN | tr ' ' '\n' | tac | xargs)
  for s in $order; do run $s $r; done
done
echo "load at end: $(cut -d' ' -f1-3 /proc/loadavg)"
