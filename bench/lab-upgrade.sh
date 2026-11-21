#!/bin/bash
# pacman vs btrpkg upgrades on a lab machine's live system (no formatting anything).
#   root@host: bash lab-upgrade.sh OUTDIR [ROUNDS]
# OUTDIR must be outside the root subvolume (e.g. /var/log/..., which is @log on Omarchy): every
# reset rolls @ back to the baseline snapshot and would take the results with it.
# Needs: /tmp/fix-mtimes.py, /usr/local/bin/pacbtr, /etc/btrpkg/keys/*.pub, /etc/btrpkg/pacman-test.conf (its [omarchy]
# points at the btrpkg test repo), snapper config `root`.
# Takes a baseline snapshot S of the current ("old") state, then per round and scenario:
#   empty the package cache of the target versions, drop caches, run the upgrade, sync, record
#   wall time, bytes downloaded, bytes written to the root's partition, data space used (btrfs),
#   check `pacman -Qkk`, then `snapper undochange S..0` back to the old state.
#   pacman: pacman -S --needed TARGETS (download + extract + hooks)
#   btrpkg: pacbtr --no-snapshot TARGETS (index + chunks, reuse from installed files, place,
#           pacman -U --dbonly + hooks); its snapshot is timed separately (snapper create).
# Both download from the same server; the order rotates between rounds.
set -uo pipefail
OUT=$1; ROUNDS=${2:-3}; CONF=/etc/btrpkg/pacman-test.conf
# scenarios: pacman, or the name of a btrpkg binary in /usr/local/bin (A/B: SCEN="pacman btrpkg0 btrpkg")
SCEN=${SCEN:-pacman btrpkg}
for s in $SCEN; do [[ $s == pacman || -x /usr/local/bin/$s ]] || { echo "no /usr/local/bin/$s"; exit 1; }; done
mkdir -p $OUT
[[ $(stat -f -c %i $OUT) != $(stat -f -c %i /) || $(findmnt -no TARGET -T $OUT) != / ]] || { echo "$OUT is in the root subvolume; use /var/log/..."; exit 1; }
exec > >(tee -a $OUT/log) 2>&1
CACHE=$(pacman-conf --config $CONF CacheDir | head -1)
part=$(lsblk -sno NAME,TYPE $(findmnt -no SOURCE / | sed 's/\[.*//') | awk '$2=="part"{gsub(/[^a-zA-Z0-9]/,"",$1); print $1; exit}')
[[ -n $part ]] || { echo "no backing partition"; exit 1; }
written() { awk '{print $7*512}' /sys/class/block/$part/stat; }
used() { btrfs filesystem sync / >/dev/null; btrfs filesystem usage -b / 2>/dev/null | grep -m1 '^Data,' | grep -o 'Used:[0-9]*' | cut -d: -f2; }
now() { date +%s.%N; }

pacman --config $CONF -Sy >/dev/null 2>&1 || { echo "pacman -Sy failed"; exit 1; }
mapfile -t T < <(pacbtr --config $CONF --h2c --list 2>/dev/null)
(( ${#T[@]} )) || { echo "nothing to upgrade through btrpkg"; exit 1; }
echo "host $(hostname) part $part cpu '$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)' cache $CACHE"
echo "targets (${#T[@]}): ${T[*]}"
pacman -Q "${T[@]}" | tr '\n' ' '; echo
newfiles=$(pacman --config $CONF -Sp --print-format '%f' "${T[@]}" 2>/dev/null)
echo "pacman would download: $(pacman --config $CONF -Sp --print-format '%s' "${T[@]}" | awk '{s+=$1} END{print s}') bytes"

t0=$(now); S=$(snapper -c root create -c number -p -d "btrpkg lab-upgrade baseline"); t1=$(now)
echo "baseline snapshot $S ($(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.2f", b-a}') s to create)"
clean_cache() { for f in $newfiles; do rm -f "$CACHE/$f" "$CACHE/$f.sig"; done; rm -f "$CACHE"/*.pkg.btr "$CACHE"/*.pkg.btr.part; }
reset() {
  snapper -c root undochange $S..0 >/dev/null 2>&1 || { echo "undochange failed"; exit 1; }
  # undochange stamps restored files with the current time; put the packages' times back
  python3 /tmp/fix-mtimes.py "${T[@]}" >/dev/null
  clean_cache; sync
}
run() { # $1 scenario, $2 round
  clean_cache; sync; echo 3 > /proc/sys/vm/drop_caches; sleep 2
  local w0 u0 a b w1 u1 dl ok
  w0=$(written); u0=$(used); a=$(now)
  if [[ $1 == pacman ]]; then
    pacman --config $CONF -S --noconfirm --needed "${T[@]}" > $OUT/$1-$2.out 2>&1; ok=$?
    sync; b=$(now)
    dl=$(for f in $newfiles; do stat -c %s "$CACHE/$f" "$CACHE/$f.sig" 2>/dev/null; done | awk '{s+=$1} END{print s+0}')
  else
    /usr/local/bin/${1} upgrade --config $CONF --h2c --no-snapshot "${T[@]}" > $OUT/$1-$2.out 2>&1; ok=$?
    sync; b=$(now)
    dl=$(( $(grep -o 'fetched_bytes: [0-9]*' $OUT/$1-$2.out | grep -o '[0-9]*$' || echo 0) + $(grep -o 'index_bytes: [0-9]*' $OUT/$1-$2.out | grep -o '[0-9]*$' || echo 0) ))
  fi
  w1=$(written); u1=$(used)
  local bad; bad=$(pacman -Qkk "${T[@]}" 2>&1 | grep -E 'total files' | awk '{s+=$(NF-2)} END{print s+0}')
  local vers; vers=$(pacman -Q "${T[@]}" | md5sum | cut -c1-8)
  printf '%s\t%s\t%s\t%.2f\t%s\t%s\t%s\t%s\t%s\n' $1 $2 $ok $(awk -v a=$a -v b=$b 'BEGIN{print b-a}') $dl $((w1-w0)) $((u1-u0)) $bad $vers | tee -a $OUT/results.tsv
  reset
}
printf 'scenario\tround\texit\tseconds\tdownloaded\twritten\tspace\tqkk_altered\tversions\n' | tee $OUT/results.tsv
read -ra SC <<< "$SCEN"
for r in $(seq 1 $ROUNDS); do
  # rotate the order each round
  for k in $(seq 0 $(( ${#SC[@]} - 1 ))); do run ${SC[$(( (k + r - 1) % ${#SC[@]} ))]} $r; done
done
echo "load at end: $(cut -d' ' -f1-3 /proc/loadavg)"
snapper -c root delete $S
