#!/bin/bash
# Install the same packages into two scratch roots: stock pacman (the mirror's .pkg.tar.*) and
# pacman-btrpkg (index + btrparts from the shared directory), then compare the roots. Nothing
# touches the machine's own root or databases.  scratch-root-compare.sh WORKDIR PKG...
# The mirror: omarchy, core and extra at http://192.168.2.2:1764 ($repo/os/$arch for Arch).
set -uo pipefail
W=$1; shift
M=http://192.168.2.2:1764
rm -rf "$W"; mkdir -p "$W"
conf() { # $1: name, $2: extra [options] lines
  cat > "$W/$1.conf" <<EOF
[options]
Architecture = auto
SigLevel = Required DatabaseOptional
LocalFileSigLevel = Optional
DBPath = $W/$1-root/var/lib/pacman/
CacheDir = $W/$1-cache/
HookDir = $W/no-hooks/
$2
[omarchy]
Server = $M/omarchy-mirror/\$arch
[core]
Server = $M/arch-mirror/\$repo/os/\$arch
[extra]
Server = $M/arch-mirror/\$repo/os/\$arch
EOF
  mkdir -p "$W/$1-root/var/lib/pacman" "$W/$1-cache" "$W/$1-root" "$W/no-hooks"
}
conf stock ""
conf btr "BtrPkgHelper = /usr/bin/btrpkg"
for s in stock btr; do
  P=/usr/bin/pacman; [[ $s == btr ]] && P=/usr/bin/pacman-btrpkg
  $P --config "$W/$s.conf" -Syy >/dev/null 2>&1 || { echo "FAIL $s sync"; exit 1; }
  # -dd: only these packages; --noscriptlet: an empty root has no shell
  t0=$EPOCHREALTIME
  $P --config "$W/$s.conf" -r "$W/$s-root" -S --noconfirm -dd --noscriptlet "$@" > "$W/$s.out" 2>&1; rc=$?
  echo "$s: exit $rc, $(awk -v a="$t0" -v b="$EPOCHREALTIME" 'BEGIN{printf "%.1f", b-a}') s, cache: $(find "$W/$s-cache" -maxdepth 1 -name '*.pkg.btr' | wc -l) .pkg.btr, $(find "$W/$s-cache" -maxdepth 1 -name '*.pkg.tar.*' ! -name '*.sig' | wc -l) .pkg.tar"
  [[ $rc == 0 ]] || tail -5 "$W/$s.out"
done
python3 /tmp/compare-roots.py "$W/stock-root" "$W/btr-root" | tail -3
# files the package databases list, checked in the btrpkg root
pacman --config "$W/btr.conf" -r "$W/btr-root" -Qkk "$@" 2>&1 | grep -E "altered|missing" | awk '{s+=$(NF-2)} END {print "pacman -Qkk in the btrpkg root: " NR " packages, " s+0 " altered files"}'
