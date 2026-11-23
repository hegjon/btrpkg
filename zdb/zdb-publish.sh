#!/bin/bash
# Server side of the zchunk database prototype: republish pacman sync databases as zchunk files.
#
#   zdb-publish.sh OUT UPSTREAM REPO...
#
# For each REPO: conditional GET of UPSTREAM/REPO/os/x86_64/REPO.db (e.g. the desktop's pacoloco,
# http://127.0.0.1:9129/repo/archlinux), then, if it changed, normalize the tar headers
# (normalize.py) and write OUT/REPO/os/x86_64/REPO.db.zck: one chunk per package entry
# (zck -s %FILENAME%), compressed with a fixed zstd dictionary OUT/.dict/REPO.dict. The
# dictionary is trained once, from the first version; a new dictionary changes every chunk, so
# every client would download the whole file again. The .zck gets the upstream db's mtime
# (Last-Modified), so a client can stamp its rebuilt REPO.db with it and a later pacman -Sy
# against the upstream mirror answers 304. Old versions are kept as REPO.db.zck.<mtime> (tests).
#
# ZCK_BIN: directory with zck (default ~/.local/opt/zchunk/bin, LD_LIBRARY_PATH set from it).
set -euo pipefail
[[ $# -ge 3 ]] || { sed -n '2,15p' "$0"; exit 2; }
OUT=$1 UP=$2; shift 2
HERE=$(cd "$(dirname "$0")" && pwd)
ZCK_BIN=${ZCK_BIN:-$HOME/.local/opt/zchunk/bin}
export LD_LIBRARY_PATH=${ZCK_BIN%/bin}/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
mkdir -p "$OUT/.upstream" "$OUT/.dict"
for r in "$@"; do
  up=$OUT/.upstream/$r.db dir=$OUT/$r/os/x86_64 zck=$OUT/$r/os/x86_64/$r.db.zck
  mkdir -p "$dir"
  args=(-sfL -R -o "$up.part")
  [[ -e $up ]] && args+=(-z "$up")   # If-Modified-Since: a 304 leaves no (or an empty) file
  curl "${args[@]}" "$UP/$r/os/x86_64/$r.db"
  if [[ -s $up.part ]]; then mv "$up.part" "$up"; else rm -f "$up.part"; fi
  if [[ -e $zck && $(stat -c %Y "$zck") == $(stat -c %Y "$up") ]]; then echo "$r: unchanged"; continue; fi
  tmp=$(mktemp -d "$OUT/.work.XXXXXX"); trap 'rm -rf "$tmp"' EXIT
  python3 "$HERE/normalize.py" "$up" "$tmp/$r.db.tar"
  if [[ ! -e $OUT/.dict/$r.dict ]]; then
    python3 "$HERE/normalize.py" --entries "$tmp/$r.db.tar" "$tmp/train" >/dev/null
    # zstd wants samples of at least 10x the dictionary size (and counts less than their sum)
    size=$(find "$tmp/train" -type f -printf "%s\n" | awk "{s+=\$1} END {print s}"); max=$(( size / 16 < 114688 ? size / 16 : 114688 ))
    zstd -q --train --maxdict=$max -r "$tmp/train" -o "$OUT/.dict/$r.dict"
  fi
  "$ZCK_BIN/zck" -s '%FILENAME%' -D "$OUT/.dict/$r.dict" -o "$tmp/$r.db.zck" "$tmp/$r.db.tar" >/dev/null
  touch -r "$up" "$tmp/$r.db.zck"
  [[ -e $zck ]] && cp -p "$zck" "$zck.$(stat -c %Y "$zck")"
  mv "$tmp/$r.db.zck" "$zck"
  rm -rf "$tmp"; trap - EXIT
  echo "$r: $(stat -c %s "$up") bytes upstream -> $(stat -c %s "$zck") bytes zck ($(date -u -r "$zck" '+%F %T'))"
done
