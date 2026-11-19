#!/bin/bash
# btrpkg vs pacman-style extraction on the desktop's bench partition.
# Run as root in the privileged exp-btrfs container:
#   docker run --rm --privileged -v /dev:/dev -v ~/Work/btrpkg:/w \
#     -v /var/cache/pacman/pkg:/pkgs:ro --tmpfs /exp:size=8g exp-btrfs bash /w/bench/bench.sh
# Layout mirrors Omarchy: subvolumes @ (root) and @pkg mounted at /var/cache/pacman/pkg,
# so reflinks cross subvolumes and mounts. compress=zstd:3 as on the installed system.
# Scenarios (fresh filesystem each):
#   pacman  - copy .pkg.tar.zst into the cache (the download), bsdtar -xp into the root
#   btrpkg  - btrpkg fetch (encoded writes into the cache), btrpkg install (reflinks)
#   noenc   - btrpkg fetch --no-encoded (decompress, kernel recompresses), btrpkg install
set -uo pipefail
DEV=/dev/nvme0n1p3; PART=nvme0n1p3; T=/mnt/t; ROOT=/mnt/root
SET=${SET:-/w/bench/installed-set.txt}; CONV=${CONV:-/w/bench/out-l19}
OUT=/w/bench/results; mkdir -p $OUT $T $ROOT
B=/w/target/release/btrpkg
ROUNDS=${ROUNDS:-2}; SCENARIOS=${SCENARIOS:-"pacman btrpkg noenc"}

[[ -b $DEV ]] || { echo "$DEV missing"; exit 1; }
if findmnt -S $DEV >/dev/null; then echo "$DEV is mounted, refusing"; exit 1; fi
size=$(blockdev --getsize64 $DEV); (( size > 100*1024**3 && size < 120*1024**3 )) || { echo "unexpected size $size"; exit 1; }

echo "staging inputs into tmpfs"
mkdir -p /exp/pkg /exp/btr /exp/btrzst
while read -r f; do
  cp /pkgs/$f /exp/pkg/; cp $CONV/${f%.pkg.tar.zst}.btrpkg /exp/btr/
  [[ -d ${CONVZST:-/nonexistent} ]] && cp $CONVZST/${f%.pkg.tar.zst}.pkg.btr.zst /exp/btrzst/
  for d in ${ZSTDIRS:-}; do mkdir -p /exp/$d; cp /w/bench/out-$d/${f%.pkg.tar.zst}.pkg.btr.zst /exp/$d/; done
done <$SET
du -sb /exp/pkg /exp/btr /exp/btrzst

written() { awk '{print $7*512}' /sys/class/block/$PART/stat; }
used() { btrfs filesystem sync $T >/dev/null; btrfs filesystem usage -b $T 2>/dev/null | grep -m1 '^Data,' | grep -o 'Used:[0-9]*' | cut -d: -f2; }
now() { date +%s.%N; }

setup() {
  umount -R $ROOT 2>/dev/null; umount $T 2>/dev/null
  wipefs -aq $DEV; mkfs.btrfs -q -f $DEV || exit 1
  mount -o compress=zstd:3 $DEV $T || exit 1
  btrfs -q subvolume create $T/@ $T/@pkg
  mount -o compress=zstd:3,subvol=@ $DEV $ROOT
  mkdir -p $ROOT/var/cache/pacman/pkg
  mount -o compress=zstd:3,subvol=@pkg $DEV $ROOT/var/cache/pacman/pkg
  sync; echo 3 >/proc/sys/vm/drop_caches
}
teardown() { umount -R $ROOT; umount $T; }

run_pacman() {
  local c=$ROOT/var/cache/pacman/pkg
  while read -r f; do
    cp /exp/pkg/$f $c/ || return 1
    bsdtar -xp --numeric-owner -f $c/$f -C $ROOT \
      --exclude .PKGINFO --exclude .BUILDINFO --exclude .MTREE --exclude .INSTALL --exclude .CHANGELOG || return 1
  done <$SET
}
run_btrpkg() {  # $1 = extra fetch flags, $2 = input dir, $3 = input extension
  local c=$ROOT/var/cache/pacman/pkg
  while read -r f; do
    local in=/exp/${2:-btr}/${f%.pkg.tar.zst}${3:-.btrpkg}
    f=${f%.pkg.tar.zst}.pkg.btr
    $B fetch $1 $in $c/$f 2>>/tmp/fetch.log || return 1
    $B install $c/$f $ROOT 2>>/tmp/install.log || return 1
  done <$SET
}

echo -e "scenario\tround\tseconds\tdev_written\tdata_used\tdata_used_after_cache_rm" | tee $OUT/${OUTNAME:-matrix}.tsv
for round in $(seq $ROUNDS); do
  for s in $SCENARIOS; do
    setup; : >/tmp/fetch.log; : >/tmp/install.log
    w0=$(written); t0=$(now)
    case $s in
      pacman) run_pacman ;;
      btrpkg) run_btrpkg "" ;;
      noenc)  run_btrpkg --no-encoded ;;
      btrzst-l*) run_btrpkg "-l ${s#btrzst-l}" btrzst .pkg.btr.zst ;;
      f[0-9]*) run_btrpkg "-l ${FETCHLEVEL:-3}" $s .pkg.btr.zst ;;
    esac || { echo "$s failed"; tail /tmp/*.log; exit 1; }
    sync; t1=$(now); w1=$(written)
    u=$(used)
    rm -f $ROOT/var/cache/pacman/pkg/*; sync; btrfs subvolume sync $T >/dev/null 2>&1; sleep 2
    u2=$(used)
    printf "%s\t%s\t%.2f\t%s\t%s\t%s\n" $s $round $(awk "BEGIN{print $t1 - $t0}") $((w1-w0)) $u $u2 | tee -a $OUT/${OUTNAME:-matrix}.tsv
    [[ $s != pacman && $s != noenc ]] && grep -c "unavailable" /tmp/fetch.log /tmp/install.log
    teardown
  done
done

[[ -n ${SKIPVERIFY:-} ]] && exit 0
# Correctness: both installs side by side on one filesystem, compared file by file.
echo "== verify"
setup
mkdir -p $ROOT/a $ROOT/b
c=$ROOT/var/cache/pacman/pkg
while read -r f; do
  bsdtar -xp --numeric-owner -f /exp/pkg/$f -C $ROOT/a \
    --exclude .PKGINFO --exclude .BUILDINFO --exclude .MTREE --exclude .INSTALL --exclude .CHANGELOG || exit 1
  g=${f%.pkg.tar.zst}.btrpkg; in=/exp/btr/$g
  [[ -n ${VERIFYZST:-} ]] && in=/exp/${VERIFYZST/#1/btrzst}/${f%.pkg.tar.zst}.pkg.btr.zst
  $B fetch $in $c/$g 2>/dev/null && $B install $c/$g $ROOT/b 2>/dev/null || { echo "btrpkg failed on $f"; exit 1; }
done <$SET
python3 /w/bench/compare.py $ROOT/a $ROOT/b | tee $OUT/${OUTNAME:-matrix}-verify.txt
# Are installed extents shared with the cache, and still compressed?
f=$(find $ROOT/b/usr/lib -type f -size +1M | head -1)
echo "sample: $f"; filefrag -v "$f" | sed -n '1,8p' | tee -a $OUT/${OUTNAME:-matrix}-verify.txt
teardown
