#!/bin/bash
#
# Test omarchy-iso's btrpkg offline mirror (builder/btrpkg-offline-mirror.sh) on a lab machine,
# without building an ISO: the same steps build-iso.sh and the installer take, on a small package
# set, side by side with the stock .pkg.tar.zst path, in throwaway subvolumes.
#
#   iso-mirror-test.sh HELPERDIR OUTDIR
#
# HELPERDIR holds index-offline-mirror.sh, btrpkg-offline-mirror.sh and compare-roots.py.
# Needs root, pacman-btrpkg, btrpkg and arch-install-scripts.
set -euo pipefail
H=$1 OUT=$2
IMAGE=(filesystem bash coreutils sed grep gzip)
SHIP=(sof-firmware alsa-firmware tailscale)
DEV=$(findmnt -no SOURCE / | sed 's/\[.*//')
TOP=/mnt/btrpkg-top
T=/var/tmp/iso-mirror-test
mkdir -p "$OUT" "$TOP"
exec > >(tee "$OUT/log") 2>&1
findmnt "$TOP" >/dev/null || mount -o subvolid=5 "$DEV" "$TOP"
# pacstrap -K starts a gpg-agent per root with a "ROOT//etc/pacman.d/gnupg" homedir (double
# slash), so gpgconf --homedir ROOT/etc/... misses its socket; kill by command line instead.
kill_root_agents() { # ROOT
  local pid
  for pid in $(pgrep -x gpg-agent); do
    tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF -- "--homedir $1/" && kill "$pid"
  done
  return 0
}
cleanup() {
  for s in "$TOP"/@isotest-*; do
    [[ -d $s ]] || continue
    kill_root_agents "$s"; sleep 1
    btrfs subvolume delete -R "$s" >/dev/null
  done
  umount "$TOP" 2>/dev/null || true
}
trap cleanup EXIT

rm -rf "$T"; mkdir -p "$T/tar" "$T/db"
echo "== download (stock pacman, the machine's repos)"
/usr/bin/pacman --dbpath "$T/db" -Sy >/dev/null
/usr/bin/pacman --dbpath "$T/db" --cachedir "$T/tar" --noconfirm -Sw "${IMAGE[@]}" "${SHIP[@]}" >/dev/null
bash "$H/index-offline-mirror.sh" "$T/tar" 4
echo "== convert"
bash "$H/btrpkg-offline-mirror.sh" convert "$T/tar" "$T/btr" 4
# as build-iso.sh: what the per-machine packages need that the image lacks
mapfile -t ship_files < <(comm -23 <(/usr/bin/pacman --dbpath "$T/db" -Sp --print-format '%f' "${SHIP[@]}" | sort) \
  <(/usr/bin/pacman --dbpath "$T/db" -Sp --print-format '%f' "${IMAGE[@]}" | sort))
printf '%s\n' "${ship_files[@]}" > "$T/shipped"
echo "== ship ${#ship_files[@]}: ${ship_files[*]}"
bash "$H/btrpkg-offline-mirror.sh" ship "$T/btr" "$T/shipped" "$T/ship"
(cd "$T/ship" && bash "$H/btrpkg-offline-mirror.sh" files . | xargs -d '\n' sha256sum > "$T/ship.sha256" && sha256sum --check --strict --quiet "$T/ship.sha256") && echo "checksums ok: $(wc -l < "$T/ship.sha256") files"

conf() { # SERVER [extra options...]
  printf '[options]\nHoldPkg = pacman glibc\nArchitecture = auto\nSigLevel = Required DatabaseOptional\nLocalFileSigLevel = Optional\n'
  shift_server=$1; shift
  for o in "$@"; do printf '%s\n' "$o"; done
  printf '[offline]\nSigLevel = Never\nServer = file://%s/\n' "$shift_server"
}

for s in stock btr; do btrfs subvolume create "$TOP/@isotest-$s" >/dev/null; btrfs subvolume create "$TOP/@isotest-$s/root" >/dev/null; done
echo "== image: stock (CacheDir = the .pkg.tar.zst mirror, as build-iso.sh)"
conf "$T/tar" "CacheDir = $T/tar/" > "$T/stock.conf"
env PATH=/usr/bin:/usr/sbin pacstrap -C "$T/stock.conf" -c -G -M "$TOP/@isotest-stock/root" "${IMAGE[@]}" > "$OUT/image-stock.out" 2>&1
echo "== image: btrpkg (cache on the image filesystem, as build-root-image.sh)"
mkdir "$TOP/@isotest-btr/btrpkg-cache"
conf "$T/btr" "BtrPkgHelper = /usr/bin/btrpkg" "CacheDir = $TOP/@isotest-btr/btrpkg-cache/" > "$T/btr.conf"
pacstrap-btrpkg -C "$T/btr.conf" -c -G -M "$TOP/@isotest-btr/root" "${IMAGE[@]}" > "$OUT/image-btr.out" 2>&1
grep -E "btrpkg|warning|error" "$OUT/image-btr.out" | head -5 || true
echo "cache: $(find "$TOP/@isotest-btr/btrpkg-cache" -name '*.pkg.btr' | wc -l) .pkg.btr, $(du -sm --apparent-size "$TOP/@isotest-btr/btrpkg-cache" | cut -f1) MiB"
big=$(find "$TOP/@isotest-btr/root/usr/bin" -type f -size +500k | head -1)
echo "extents of ${big#"$TOP"/}: $(filefrag -v "$big" | grep -c encoded) encoded, $(filefrag -v "$big" | grep -c shared) shared"
rm -rf "$TOP/@isotest-btr/btrpkg-cache"
btrfs filesystem sync "$TOP"
echo "after removing the cache: $(filefrag -v "$big" | grep -c encoded) encoded, $(filefrag -v "$big" | grep -c shared) shared"

echo "== per-machine install, archinstall's flags (stock: offline mirror bind-mounted as the cache)"
conf "$T/tar" > "$T/stock-live.conf"
mkdir -p "$TOP/@isotest-stock/root/var/cache/pacman/pkg"
mount --bind "$T/tar" "$TOP/@isotest-stock/root/var/cache/pacman/pkg"
env PATH=/usr/bin:/usr/sbin pacstrap -C "$T/stock-live.conf" -K "$TOP/@isotest-stock/root" "${SHIP[@]}" --noconfirm --needed > "$OUT/delta-stock.out" 2>&1
umount "$TOP/@isotest-stock/root/var/cache/pacman/pkg"
echo "== per-machine install from the shipped btrpkg mirror (target cache, no bind mount)"
conf "$T/ship" "BtrPkgHelper = /usr/bin/btrpkg" > "$T/btr-live.conf"
pacstrap-btrpkg -C "$T/btr-live.conf" -K "$TOP/@isotest-btr/root" "${SHIP[@]}" --noconfirm --needed > "$OUT/delta-btr.out" 2>&1
grep -E "btrpkg|warning|error" "$OUT/delta-btr.out" | head -5 || true
echo "target cache: $(find "$TOP/@isotest-btr/root/var/cache/pacman/pkg" -name '*.pkg.btr' -printf '%f ' )"
echo "== a package the shipped mirror lacks must fail cleanly"
if pacstrap-btrpkg -C "$T/btr-live.conf" "$TOP/@isotest-btr/root" nano --noconfirm --needed > "$OUT/missing.out" 2>&1; then
  echo "UNEXPECTED: nano installed"; else echo "refused as expected: $(grep -m1 -iE 'error|failed' "$OUT/missing.out")"; fi
echo "== compare"
for s in stock btr; do kill_root_agents "$TOP/@isotest-$s"; done
python3 "$H/compare-roots.py" "$TOP/@isotest-stock/root" "$TOP/@isotest-btr/root" > "$OUT/compare.txt" 2>&1 && rc=0 || rc=$?
tail -4 "$OUT/compare.txt"; echo "compare exit $rc"
for s in stock btr; do echo "$s: $(btrfs filesystem du -s --raw "$TOP/@isotest-$s/root" | awk 'NR==2 {print $1}') bytes referenced, $(compsize -b "$TOP/@isotest-$s/root" 2>/dev/null | awk '/^TOTAL/ {print $3 " on disk"}')"; done
