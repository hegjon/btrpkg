#!/bin/bash
# Download through pacman-btrpkg with a throwaway dbpath and cache: the btrpkg path only, nothing
# installed, the system's databases untouched.  btrparts-client-check.sh CONF PKG...
set -uo pipefail
conf=$1; shift
W=/var/tmp/btrparts-check; rm -rf $W; mkdir -p $W/db $W/cache
# our DBPath and CacheDir, BtrPkgHelper = the packaged btrpkg (whatever CONF had), and the
# iteration-1 tunnel URL (127.0.0.1:8790, ssh -R from the desktop) replaced by the mirror itself
sed -e '/^BtrPkgHelper/d' -e 's|http://127.0.0.1:8790/|http://192.168.2.2:1764/|' \
  -e "s|^\[options\]|[options]\nDBPath = $W/db/\nCacheDir = $W/cache/\nBtrPkgHelper = /usr/bin/btrpkg|" "$conf" > $W/pacman.conf
pacman-btrpkg --config $W/pacman.conf -Syy > $W/out 2>&1 || { echo "FAIL sync"; tail -3 $W/out; exit 1; }
for p in "$@"; do
  # the first repository's entry for P (its btrpkg.parts, if any)
  d=$(for f in $(pacman-btrpkg --config $W/pacman.conf -Sp --print-format '%r' -dd "$p" 2>/dev/null | head -1); do
        bsdtar -xOf $W/db/sync/$f.db 2>/dev/null; done |
      awk -v p="$p" '/^%NAME%$/{getline n} n==p && /^btrpkg.parts=/{print; exit}')
  pacman-btrpkg --config $W/pacman.conf -Sw --noconfirm -dd "$p" > $W/out 2>&1; rc=$?
  f=$(find $W/cache -name "$p-[0-9]*.pkg.btr" -printf '%f\n' | head -1)
  # -Sw checked every btrpart against the index and the index against the database's ID
  if [[ $rc == 0 && -n $f ]] && btrpkg info "$W/cache/$f" >/dev/null 2>&1; then
    echo "ok   $p: $f (${d:-no btrpkg.parts})"
  else
    echo "FAIL $p: rc $rc, file '${f:-none}'"; tail -4 $W/out
  fi
done
rm -rf $W
