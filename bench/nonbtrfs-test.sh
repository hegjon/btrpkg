#!/bin/bash
# NOT RUN YET: written for the README section on other filesystems; check before relying on it.
# btrpkg fetch/pull/install on filesystems other than btrfs: tmpfs, ext4, xfs.
# ext4 and xfs are made on the bench partition (nvme0n1p3, wiped). Run as root:
#   docker run --rm --privileged -v /dev:/dev -v ~/Work/btrpkg:/w -v /var/cache/pacman/pkg:/pkgs:ro \
#     --tmpfs /exp:size=4g exp-btrfs bash /w/bench/nonbtrfs-test.sh
# (installs xfsprogs into the throwaway container first if FSTYPES includes xfs)
set -euo pipefail
B=/w/target/release/btrpkg; S=/w/bench/out-store; P=/w/bench/out-delta-pkgs; OUT=/w/bench/results
SET=/w/bench/smoke-set.txt; DEV=/dev/nvme0n1p3; T=/mnt/t
FSTYPES=${FSTYPES:-"tmpfs ext4 xfs"}
[[ $FSTYPES == *xfs* ]] && { pacman -Sy --noconfirm --needed xfsprogs >/dev/null 2>&1 || { echo "xfsprogs install failed"; exit 1; }; }
trap 'umount $T 2>/dev/null; chown -R 1000:1000 $OUT' EXIT
mkdir -p $T

for fs in $FSTYPES; do
  echo "=================== $fs"
  case $fs in
    tmpfs) mount -t tmpfs -o size=3g tmpfs $T ;;
    *)
      findmnt -S $DEV >/dev/null && { echo "$DEV is mounted, refusing"; exit 1; }
      size=$(blockdev --getsize64 $DEV); (( size > 100*1024**3 && size < 120*1024**3 )) || { echo "unexpected size $size"; exit 1; }
      wipefs -aq $DEV
      case $fs in ext4) mkfs.ext4 -q -F $DEV ;; xfs) mkfs.xfs -q -f $DEV ;; esac
      mount $DEV $T ;;
  esac
  R=$T/root; C=$R/var/cache/pacman/pkg; mkdir -p $C $T/ref
  u0=$(df -B1 --output=used $T | tail -1)

  echo "-- fetch + install (.pkg.btr, smoke set)"
  while read -r f; do
    g=${f%.pkg.tar.zst}
    $B fetch /w/bench/out-s4k/$g.btrpkg $C/$g.pkg.btr 2>>$T/fetch.log
    $B install $C/$g.pkg.btr $R 2>>$T/install.log
    bsdtar -xp --numeric-owner -f /pkgs/$f -C $T/ref \
      --exclude .PKGINFO --exclude .BUILDINFO --exclude .MTREE --exclude .INSTALL --exclude .CHANGELOG
  done <$SET
  echo "-- pull + install (hyprland 0.57.1-2, then 0.57.2-2 reusing its chunks)"
  $B pull $S hyprland-0.57.1-2-x86_64 $C/hyprland-0.57.1-2.pkg.btr 2>>$T/fetch.log >/dev/null
  $B pull $S hyprland-0.57.2-2-x86_64 $C/hyprland-0.57.2-2.pkg.btr 2>>$T/fetch.log | cut -f2-
  $B install $C/hyprland-0.57.2-2.pkg.btr $R 2>>$T/install.log
  bsdtar -xp --numeric-owner -f $P/hyprland-0.57.2-2-x86_64.pkg.tar.zst -C $T/ref \
    --exclude .PKGINFO --exclude .BUILDINFO --exclude .MTREE --exclude .INSTALL --exclude .CHANGELOG
  rm -rf $R/a; mkdir -p $R/a; mv $C $T/cache-keep; rmdir $R/var/cache/pacman $R/var/cache 2>/dev/null || true
  # compare everything installed (minus the cache dir) with the bsdtar reference
  python3 /w/bench/compare.py $T/ref $R | tee $OUT/nonbtrfs-$fs-verify.txt | grep -E "compared|problems"
  mkdir -p $R/var/cache/pacman; mv $T/cache-keep $C
  echo "fallbacks:"; sort $T/fetch.log $T/install.log | grep -o "btrpkg: [a-z ]*unavailable ([^)]*)[^;]*; [a-z ]*" | sort | uniq -c
  grep -ho "fallback_bytes: [0-9]*\|fallback_chunks: [0-9]*\|copied_bytes: [0-9]*\|cloned_bytes: [0-9]*" $T/fetch.log $T/install.log | \
    awk '{split($0,a,": "); s[a[1]]+=a[2]} END {for (k in s) printf "  %s total %d\n", k, s[k]}'
  u1=$(df -B1 --output=used $T | tail -1)
  ref=$(du -sb $T/ref | cut -f1)
  echo "space: used by root + cache $(( (u1-u0) - $(du -sB1 $T/ref | cut -f1) )) bytes (approx.), reference tree $ref bytes"
  f=$R/usr/share/hypr/wall0.png; filefrag -v $f 2>/dev/null | sed -n '4p' || true
  umount $T
done
