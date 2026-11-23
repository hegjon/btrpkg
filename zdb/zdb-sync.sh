#!/bin/bash
# Client side of the zchunk database prototype: update a pacman sync database from a .zck,
# downloading only the package entries that changed.
#
#   zdb-sync.sh URL SYNCDIR REPO
#
# URL: where REPO.db.zck lives (e.g. http://192.168.2.2:1764/zdb/extra/os/x86_64). SYNCDIR keeps
# REPO.db.zck from the last sync (the delta source) and gets REPO.db, the plain tar pacman reads
# (libarchive needs no compression), stamped with the .zck's Last-Modified so that a later
# pacman -Sy against the normal mirror answers 304 instead of downloading it again.
# Prints one line: REPO, bytes received, the .zck's size, seconds.
#
# ZCK_BIN: directory with zckdl and unzck (LD_LIBRARY_PATH set to ../lib).
set -euo pipefail
[[ $# == 3 ]] || { sed -n '2,13p' "$0"; exit 2; }
URL=$1 SYNC=$2 R=$3
ZCK_BIN=${ZCK_BIN:-/opt/zdb/bin}
export LD_LIBRARY_PATH=${ZCK_BIN%/bin}/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
work=$(mktemp -d "$SYNC/.zdb-$R.XXXXXX"); trap 'rm -rf "$work"' EXIT
t0=$(date +%s.%N)
lm=$(curl -sfI "$URL/$R.db.zck" | tr -d '\r' | sed -n 's/^[Ll]ast-[Mm]odified: //p')
if [[ -n $lm && -s $SYNC/$R.db.zck && -s $SYNC/$R.db && $(date -d "$lm" +%s) == $(stat -c %Y "$SYNC/$R.db.zck") ]]; then
  printf '%s\t0\t%s\t0.00\tunchanged\n' "$R" "$(stat -c %s "$SYNC/$R.db.zck")"   # like pacman's 304
  exit 0
fi
src=()
[[ -s $SYNC/$R.db.zck ]] && src=(-s "$SYNC/$R.db.zck")
# zckdl: lead and header by range, then the chunks the source lacks (multi-range requests); it
# checks the header checksum and every chunk's, and writes REPO.db.zck into the cwd
log=$( cd "$work" && "$ZCK_BIN/zckdl" -v "${src[@]}" "$URL/$R.db.zck" 2>&1 ) || { echo "$log" >&2; exit 1; }
got=$(sed -n 's/.*Downloaded \([0-9]*\) bytes.*/\1/p' <<<"$log" | tail -1)
"$ZCK_BIN/unzck" -c "$work/$R.db.zck" > "$work/$R.db"
[[ -n $lm ]] && touch -d "$lm" "$work/$R.db" "$work/$R.db.zck"
mv "$work/$R.db" "$SYNC/$R.db"
mv "$work/$R.db.zck" "$SYNC/$R.db.zck"
t1=$(date +%s.%N)
printf '%s\t%s\t%s\t%s\n' "$R" "${got:-?}" "$(stat -c %s "$SYNC/$R.db.zck")" "$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b - a}')"
