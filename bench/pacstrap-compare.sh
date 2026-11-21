#!/bin/bash
# pacstrap against pacstrap-btrpkg on a lab machine, into throwaway subvolumes of its own btrfs
# (nothing is formatted). root@host:
#   bash pacstrap-compare.sh OUTDIR CONF PKGLIST [ROUNDS] [MODE]
# MODE image  (default): omarchy-iso's root-image flags, `-C CONF -c -G -M ROOT PKGS`, with the
#                        config's CacheDir an empty subvolume on the same filesystem
#      target:           `-C CONF -G -M ROOT PKGS`, the cache inside the new root (pacstrap's default)
# Per round and scenario (order alternates): fresh subvolumes, drop caches, pacstrap, sync; record
# seconds, bytes written to the root's partition, the new root's and the cache's referenced and
# exclusive bytes. Then compare-roots.py between the two roots of a round, and delete them.
# OUTDIR must be outside the root subvolume (/var/log/...). Needs /tmp/compare-roots.py,
# pacstrap (arch-install-scripts) and pacstrap-btrpkg (pacman-btrpkg).
set -uo pipefail
OUT=$1; CONF=$2; PKGS=$3; ROUNDS=${4:-1}; MODE=${5:-image}
mapfile -t T < <(grep -v '^#\|^$' "$PKGS")
mkdir -p "$OUT"; exec > >(tee -a "$OUT/log") 2>&1
dev=$(findmnt -no SOURCE / | sed 's/\[.*//')
TOP=/mnt/btrpkg-top
mkdir -p $TOP; mountpoint -q $TOP || mount -o subvolid=5 "$dev" $TOP || exit 1
part=$(lsblk -sno NAME,TYPE "$dev" | awk '$2=="part"{gsub(/[^a-zA-Z0-9]/,"",$1); print $1; exit}')
written() { awk '{print $7*512}' /sys/class/block/$part/stat; }
netdev=$(ip -o route get 192.168.2.2 | grep -o 'dev [^ ]*' | cut -d' ' -f2)
rx() { awk -v d="$netdev:" '$1==d{print $2}' /proc/net/dev; }
disk() { compsize -x "$1" 2>/dev/null | awk '$1=="TOTAL"{print $3}'; }   # on-disk size (compressed), human units
du_x() { btrfs filesystem du -s --raw "$1" | awk 'NR==2{print $1, $2}'; }   # total exclusive
now() { date +%s.%N; }
echo "mode=$MODE packages=${#T[@]} conf=$CONF dev=$dev part=$part $(pacman-btrpkg --version | grep -o 'Pacman v[^ ]*') btrpkg $(pacman -Q btrpkg-git | cut -d' ' -f2)"
printf 'scenario\tround\texit\tseconds\twritten\treceived\troot_total\troot_excl\troot_disk\tcache_total\tcache_excl\tcache_disk\tbtr_files\tpkg_files\n' > "$OUT/results.tsv"

run() { # $1 scenario (stock|btr) $2 round
  local s=$1 r=$2 root=$TOP/@pstrap-$1-$2 cache=$TOP/@pstrap-cache-$1-$2 conf=$OUT/$1-$2.conf flags
  btrfs subvolume create "$root" >/dev/null
  if [[ $MODE == image ]]; then
    btrfs subvolume create "$cache" >/dev/null
    sed -e '/^CacheDir/d' -e "/^\[options\]/a CacheDir = $cache/" "$CONF" > "$conf"
    flags=(-C "$conf" -c -G -M)
  else
    cp "$CONF" "$conf"; flags=(-C "$conf" -G -M)
  fi
  # stock: the real pacman, even where /usr/local/bin/pacman points to pacman-btrpkg
  local cmd=(env PATH=/usr/bin:/usr/sbin pacstrap); [[ $s == btr ]] && cmd=(pacstrap-btrpkg)
  sync; echo 3 > /proc/sys/vm/drop_caches
  local w0 t0 t1 w1 rc
  w0=$(written); n0=$(rx); t0=$(now)
  "${cmd[@]}" "${flags[@]}" "$root" "${T[@]}" > "$OUT/$s-$r.out" 2>&1; rc=$?
  sync; t1=$(now); w1=$(written); n1=$(rx)
  local cdir=$cache; [[ $MODE == target ]] && cdir=$root/var/cache/pacman/pkg
  local rt rx ct cx
  read -r rt rx < <(du_x "$root"); read -r ct cx < <(du_x "$cdir" 2>/dev/null || echo "0 0")
  local nb np
  nb=$(find "$cdir" -maxdepth 1 -name '*.pkg.btr' | wc -l); np=$(find "$cdir" -maxdepth 1 -name '*.pkg.tar.*' ! -name '*.sig' | wc -l)
  printf '%s\t%s\t%s\t%.2f\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$s" "$r" "$rc" "$(awk -v a="$t0" -v b="$t1" 'BEGIN{print b-a}')" \
    $((w1 - w0)) $((n1 - n0)) "$rt" "$rx" "$(disk "$root")" "$ct" "$cx" "$(disk "$cdir")" "$nb" "$np" | tee -a "$OUT/results.tsv"
  [[ $rc == 0 ]] || tail -15 "$OUT/$s-$r.out"
}
cleanup_round() { # $1 round
  for s in stock btr; do
    umount -R "$TOP/@pstrap-$s-$1" 2>/dev/null
    btrfs subvolume delete -R "$TOP/@pstrap-$s-$1" >/dev/null 2>&1
    btrfs subvolume delete -R "$TOP/@pstrap-cache-$s-$1" >/dev/null 2>&1
  done
}
for r in $(seq 1 "$ROUNDS"); do
  cleanup_round "$r"
  if (( r % 2 )); then run stock "$r"; run btr "$r"; else run btr "$r"; run stock "$r"; fi
  echo "== round $r: compare-roots stock btr"
  python3 /tmp/compare-roots.py "$TOP/@pstrap-stock-$r" "$TOP/@pstrap-btr-$r" > "$OUT/compare-$r.txt" 2>&1
  echo "compare exit $?"; head -40 "$OUT/compare-$r.txt"
  cleanup_round "$r"
done
umount $TOP && rmdir $TOP
