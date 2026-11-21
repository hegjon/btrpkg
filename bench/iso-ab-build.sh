#!/bin/bash
#
# Build two Omarchy edge ISOs from the same omarchy-iso commit and build cache, back to back: stock,
# then with the offline mirror and root image in the btrpkg format (omarchy-iso-make --btrpkg).
# Jonny runs this: the build formats loop images (mkfs.btrfs in build-root-image.sh, mkarchiso's
# EFI and live-root images), which the agent never runs.
#
#   iso-ab-build.sh [OMARCHY_ISO_CHECKOUT] [OUTDIR]
#
# Defaults: ~/Work/omarchy-iso-btrpkg (branch btrpkg), ~/Work/btrpkg/bench/results/iso-<date>.
# Uses the newest btrpkg-git and pacman-btrpkg from packaging/out. Passes --keep-pkg-cache, so
# the host's /var/cache/pacman/pkg is left alone (omarchy-iso-make empties it otherwise), and
# --no-boot-offer. Takes about an hour for both; nothing needs answering.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
ISO=${1:-$HOME/Work/omarchy-iso-btrpkg}
OUT=${2:-$here/bench/results/iso-$(date +%Y%m%d)}
[[ -x $ISO/bin/omarchy-iso-make ]] || { echo "no omarchy-iso checkout at $ISO" >&2; exit 1; }
grep -q -- '--btrpkg)' "$ISO/bin/omarchy-iso-make" || { echo "$ISO has no --btrpkg (branch btrpkg?)" >&2; exit 1; }
[[ -z $(git -C "$ISO" status --porcelain --untracked-files=no) ]] || { echo "$ISO has uncommitted changes" >&2; exit 1; }
mkdir -p "$OUT/btrpkg-packages"
rm -f "$OUT"/btrpkg-packages/*.pkg.tar.zst
for p in btrpkg-git pacman-btrpkg; do
  newest=$(find "$here/packaging/out" -maxdepth 1 -name "$p-[0-9]*.pkg.tar.zst" -printf '%T@ %p\n' | sort -n | tail -1 | cut -d' ' -f2-)
  [[ -n $newest ]] || { echo "no $p package in $here/packaging/out (run packaging/build.sh)" >&2; exit 1; }
  cp "$newest" "$OUT/btrpkg-packages/"
done
{
  echo "omarchy-iso $(git -C "$ISO" rev-parse --short HEAD) ($(git -C "$ISO" branch --show-current))"
  echo "btrpkg $(git -C "$here" rev-parse --short HEAD)"
  find "$OUT/btrpkg-packages" -name '*.pkg.tar.zst' -printf '%f\n'
  echo "host $(uname -r), $(nproc) CPUs, load $(cut -d' ' -f1-3 /proc/loadavg)"
} | tee "$OUT/versions.txt"

build() { # NAME [omarchy-iso-make options]
  local name=$1 t0 t1 iso; shift
  echo "== $name build, $(date '+%F %T')"
  t0=$(date +%s)
  (cd "$ISO" && ./bin/omarchy-iso-make --edge --keep-pkg-cache --no-boot-offer "$@") > "$OUT/build-$name.log" 2>&1 \
    || { echo "$name build failed; tail of $OUT/build-$name.log:" >&2; tail -30 "$OUT/build-$name.log" >&2; exit 1; }
  t1=$(date +%s)
  iso=$(find "$ISO/release" -maxdepth 1 -name 'omarchy-*.iso' -newermt "@$t0" -printf '%T@ %p\n' | sort -n | tail -1 | cut -d' ' -f2-)
  [[ -n $iso ]] || { echo "$name: no new ISO in $ISO/release" >&2; exit 1; }
  mv "$iso" "$OUT/omarchy-edge-$name.iso"
  printf '%s\t%s\t%s\n' "$name" $((t1 - t0)) "$(stat -c %s "$OUT/omarchy-edge-$name.iso")" | tee -a "$OUT/builds.tsv"
}
printf 'build\tseconds\tiso_bytes\n' > "$OUT/builds.tsv"
build stock
build btrpkg --btrpkg "$OUT/btrpkg-packages"
echo "== done $(date '+%F %T'): ISOs and logs in $OUT"
