#!/bin/bash
# Upgrade download with the old version installed and no old cache file: what `pull --installed
# ROOT` fetches, per package pair, for one or two btrpkg binaries (to compare the
# moved-file lookup with a build without it).
#
#   [PARTS=../btrparts] bench/moved-pull-bench.sh REPODIR OLDSTEM NEWSTEM [BTRPKG...]
#
# The old version is pulled and installed into a scratch root, with a pacman local database entry
# (desc, files) made from its package file's list, as pacman would leave it. Prints per binary:
# btrparts from installed files (of those moved) and to download, and the pull time.
set -euo pipefail
repo=$1 old=$2 new=$3; shift 3
bins=("${@:-btrpkg}")
w=$(mktemp -d "${TMPDIR:-/tmp}/moved-pull.XXXXXX")
trap 'rm -rf "$w"' EXIT
b0=${bins[0]}
parts=${PARTS:-../btrparts}
"$b0" pull --unverified --no-fsync --parts "$parts" "$repo" "$old" "$w/old.pkg.btr" >/dev/null 2>&1
"$b0" install "$w/old.pkg.btr" "$w/root" >/dev/null 2>&1
rm -f "$w/old.pkg.btr"
name=$(bsdtar -xOf "$repo/$old.pkg.tar.zst" .PKGINFO | sed -n 's/^pkgname = //p')
ver=$(bsdtar -xOf "$repo/$old.pkg.tar.zst" .PKGINFO | sed -n 's/^pkgver = //p')
db="$w/root/var/lib/pacman/local/$name-$ver"
mkdir -p "$db"
printf '%%NAME%%\n%s\n\n%%VERSION%%\n%s\n\n' "$name" "$ver" > "$db/desc"
{ echo '%FILES%'; bsdtar -tf "$repo/$old.pkg.tar.zst" | grep -v '^\.' | sort; echo; } > "$db/files"
nbytes=$(stat -c %s "$repo/$new.pkg.tar.zst")
for b in "${bins[@]}"; do
    rm -f "$w/new.pkg.btr"
    s=$(date +%s%N)
    line=$("$b" pull --unverified --no-fsync --parts "$parts" --installed "$w/root" "$repo" "$new" "$w/new.pkg.btr" 2>"$w/err" | grep '^download')
    e=$(date +%s%N)
    moved=$(grep -o 'installed_moved: [0-9]*' "$w/err" | grep -o '[0-9]*$' || echo -)
    # download NAME INDEX FETCHED_CHUNKS FETCHED_BYTES REUSED_CHUNKS REUSED_BYTES INSTALLED_CHUNKS
    read -r _ _ ib fc fb _ _ ic <<<"$line"
    printf '%s\t%s\tinstalled %s btrparts (moved %s)\tdownload %s btrparts, %.1f MB (index %.2f MB, package file %.1f MB)\t%d ms\n' \
        "$new" "${b##*/}" "$ic" "$moved" "$fc" "$(echo "($fb+$ib)/1000000" | bc -l)" "$(echo "$ib/1000000" | bc -l)" "$(echo "$nbytes/1000000" | bc -l)" $(( (e - s) / 1000000 ))
done
