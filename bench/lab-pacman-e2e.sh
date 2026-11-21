#!/bin/bash
# Stock pacman against pacman with the btrpkg patches, on a lab machine's live system.
#   root@host: bash lab-pacman-e2e.sh OUTDIR "TARGETS..." [ROUNDS]
# OUTDIR must be outside the root subvolume (/var/log/...). Needs: the packages pacman-btrpkg and
# btrpkg-git (packaging/), or a development build through PACBTR/BTRPKG (e.g.
# PACBTR="env LD_LIBRARY_PATH=/opt/pacbtr /opt/pacbtr/pacman" BTRPKG=/usr/local/bin/btrpkg),
# /etc/btrpkg/pacman-test.conf (or CONF=FILE, e.g. /etc/pacman.conf for a real upgrade), /etc/btrpkg/keys,
# /tmp/fix-mtimes.py, snapper config `root`.
# Per round and scenario: empty the cache of the targets, drop caches, `pacman -S TARGETS`, sync,
# then record time, bytes written to the root's partition, data space used, `pacman -Qkk`, a
# manifest of every file of the targets (type, mode, owner, size, mtime, xattrs, sha256; no mtime
# for directories, which other packages share) and the targets' local database entries (desc
# without %INSTALLDATE% and %VALIDATION%, files, mtree, install). Then `snapper undochange` back.
# The manifests and database entries of the two scenarios must be identical.
# Optional env: REPOS (extra repository sections, appended to both configs), PREP (a command run
# before each scenario, untimed, e.g. installing an old version and changing a config file),
# EXTRA (paths whose whole tree goes into the manifest too, e.g. for .pacnew files).
set -uo pipefail
OUT=$1; read -ra T <<< "$2"; ROUNDS=${3:-1}
CONF=${CONF:-/etc/btrpkg/pacman-test.conf}; BCONF=/etc/btrpkg/pacman-btr.conf
PACBTR=${PACBTR:-/usr/bin/pacman-btrpkg}; BTRPKG=${BTRPKG:-/usr/bin/btrpkg}
mkdir -p $OUT
[[ $(findmnt -no TARGET -T $OUT) != / ]] || { echo "$OUT is in the root subvolume; use /var/log/..."; exit 1; }
exec > >(tee -a $OUT/log) 2>&1
if [[ -n ${REPOS:-} ]]; then
  cp $CONF $OUT/pacman-stock.conf; printf '%b\n' "$REPOS" >> $OUT/pacman-stock.conf; CONF=$OUT/pacman-stock.conf
fi
{ cat $CONF | sed "/^\[options\]/a BtrPkgHelper = $BTRPKG --h2c"; } > $BCONF
CACHE=$(pacman-conf --config $CONF CacheDir | head -1)
part=$(lsblk -sno NAME,TYPE $(findmnt -no SOURCE / | sed 's/\[.*//') | awk '$2=="part"{gsub(/[^a-zA-Z0-9]/,"",$1); print $1; exit}')
written() { awk '{print $7*512}' /sys/class/block/$part/stat; }
used() { btrfs filesystem sync / >/dev/null; btrfs filesystem usage -b / 2>/dev/null | grep -m1 '^Data,' | grep -o 'Used:[0-9]*' | cut -d: -f2; }
now() { date +%s.%N; }
pacman --config $CONF -Syy >/dev/null 2>&1 || { echo "pacman -Syy failed"; exit 1; }
echo "host $(hostname) targets: ${T[*]}"
pacman -Q "${T[@]}" | tr '\n' ' '; echo
newfiles=$(pacman --config $CONF -Sp --print-format '%f' "${T[@]}")
S=$(snapper -c root create -c number -p -d "btrpkg pacman e2e baseline"); echo "baseline snapshot $S"
clean_cache() {
  for f in $newfiles; do b=${f%.pkg.tar.*}; rm -f "$CACHE/$f" "$CACHE/$f.sig" "$CACHE/$b.pkg.btr" "$CACHE/$b.pkg.btr.sig" "$CACHE/$b.pkg.btr.part"; done
}
manifest() { # $1 out file
  for p in "${T[@]}"; do pacman -Qlq $p; done | sort -u | while read -r f; do
    if [[ -L $f ]]; then echo "$f l $(stat -c '%u %g' "$f") $(readlink "$f")"
    elif [[ -d $f ]]; then echo "$f d $(stat -c '%a %u %g' "$f")"
    elif [[ -e $f ]]; then echo "$f f $(stat -c '%a %u %g %s %Y' "$f") $(getfattr -h -d -m - -e hex "$f" 2>/dev/null | grep -v '^#' | grep . | tr '\n' ' ')$(sha256sum < "$f" | cut -c1-64)"
    else echo "$f MISSING"; fi
  done > $1
  for x in ${EXTRA:-}; do
    find $x -printf '%p %y %m %U %G %s %l\n' 2>/dev/null | sort | while read -r f rest; do
      echo "extra $f $rest $([[ -f $f && ! -L $f ]] && sha256sum < "$f" | cut -c1-64)"
    done
  done >> $1
}
dbcopy() { # $1 out dir
  mkdir -p $1
  for p in "${T[@]}"; do
    d=$(ls -d /var/lib/pacman/local/$p-[0-9]* 2>/dev/null | head -1); [[ -n $d ]] || continue
    awk '/^%(INSTALLDATE|VALIDATION)%/{skip=1; next} /^$/{skip=0} !skip' $d/desc > $1/$p.desc
    grep -A3 '^%VALIDATION%' $d/desc | tr '\n' ' ' > $1/$p.validation
    for x in files install; do [[ -f $d/$x ]] && cp $d/$x $1/$p.$x; done
    [[ -f $d/mtree ]] && zcat $d/mtree > $1/$p.mtree
  done
}
run() { # $1 scenario $2 round
  clean_cache
  if [[ -n ${PREP:-} ]]; then bash -c "$PREP" > $OUT/$1-$2.prep 2>&1 || { echo "PREP failed:"; cat $OUT/$1-$2.prep; exit 1; }; fi
  sync; echo 3 > /proc/sys/vm/drop_caches; sleep 2
  local w0 u0 a b ok
  w0=$(written); u0=$(used); a=$(now)
  if [[ $1 == stock ]]; then
    /usr/bin/pacman --config $CONF -S --noconfirm "${T[@]}" > $OUT/$1-$2.out 2>&1; ok=$?
  elif [[ $1 == pacbtr ]]; then
    pacbtr --config $CONF --h2c --no-snapshot "${T[@]}" > $OUT/$1-$2.out 2>&1; ok=$?
  else
    $PACBTR --config $BCONF -S --noconfirm "${T[@]}" > $OUT/$1-$2.out 2>&1; ok=$?
  fi
  sync; b=$(now)
  local w1 u1 bad
  w1=$(written); u1=$(used)
  bad=$(pacman -Qkk "${T[@]}" 2>&1 | grep 'total files' | awk '{s+=$(NF-2)} END{print s+0}')
  manifest $OUT/$1-$2.manifest; dbcopy $OUT/$1-$2.db
  ls "$CACHE" | grep -c '\.pkg\.btr$' > $OUT/$1-$2.btrcount
  printf '%s\t%s\t%s\t%.2f\t%s\t%s\t%s\t%s\n' $1 $2 $ok $(awk -v a=$a -v b=$b 'BEGIN{print b-a}') $((w1-w0)) $((u1-u0)) $bad $(cat $OUT/$1-$2.btrcount) | tee -a $OUT/results.tsv
  snapper -c root undochange $S..0 >/dev/null 2>&1 || { echo "undochange failed"; exit 1; }
  python3 /tmp/fix-mtimes.py "${T[@]}" >/dev/null
  clean_cache; sync
}
printf 'scenario\tround\texit\tseconds\twritten\tspace\tqkk_altered\tbtr_cache_files\n' | tee $OUT/results.tsv
# scenarios: stock (pacman), btr (patched pacman with BtrPkgHelper), pacbtr (iteration 1)
read -ra SC <<< "${SCEN:-stock btr}"
for r in $(seq 1 $ROUNDS); do
  for k in $(seq 0 $(( ${#SC[@]} - 1 ))); do run ${SC[$(( (k + r - 1) % ${#SC[@]} ))]} $r; done
done
for r in $(seq 1 $ROUNDS); do
  for s in "${SC[@]}"; do
    [[ $s == stock ]] && continue
    if cmp -s $OUT/stock-$r.manifest $OUT/$s-$r.manifest && diff -rq $OUT/stock-$r.db $OUT/$s-$r.db -x '*.validation' >/dev/null; then
      echo "round $r $s: installed files and database entries identical to stock"
    else
      echo "round $r $s: DIFFERENT from stock"; diff $OUT/stock-$r.manifest $OUT/$s-$r.manifest | head -20; diff -r -x '*.validation' $OUT/stock-$r.db $OUT/$s-$r.db | head -20
    fi
  done
  echo "validation stock: $(cat $OUT/stock-$r.db/*.validation | head -c 200)"
  echo "validation btr:   $(cat $OUT/btr-$r.db/*.validation | head -c 200)"
done
snapper -c root delete $S
pacman -Qkk "${T[@]}" 2>&1 | grep 'total files' | awk '{s+=$(NF-2)} END{print "after reset: altered", s+0}'
