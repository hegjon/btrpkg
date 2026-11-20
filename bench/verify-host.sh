#!/bin/bash
# Correctness check of fetch + install on a lab machine's own btrfs, without formatting anything:
# throwaway subvolumes on the root filesystem (LUKS2 + btrfs, compress=zstd:3, as installed).
#   root@host: bash verify-host.sh DIR        DIR holds pkg/*.pkg.tar.zst, btr/*.btrpkg, set.txt,
#                                             btrpkg (binary) and compare_trees.py
# Cache subvolume inside @pkg (the real package cache's subvolume), roots inside @, so reflinks
# cross subvolumes and mounts as in real use. Everything it makes is deleted at the end.
set -euo pipefail
D=$(realpath "$1"); B=$D/btrpkg
CACHE=/var/cache/pacman/pkg/btrpkg-verify; R=/btrpkg-verify
[[ $(findmnt -no FSTYPE /) == btrfs && $(findmnt -no FSTYPE /var/cache/pacman/pkg) == btrfs ]] || { echo "not btrfs"; exit 1; }
for s in $CACHE $R; do [[ -e $s ]] && { echo "$s exists, refusing"; exit 1; }; done
cleanup() { btrfs subvolume delete $CACHE $R/btr $R/ref >/dev/null 2>&1 || true; btrfs subvolume delete $R >/dev/null 2>&1 || true; }
trap cleanup EXIT
btrfs subvolume create $CACHE >/dev/null; btrfs subvolume create $R >/dev/null
btrfs subvolume create $R/btr >/dev/null; btrfs subvolume create $R/ref >/dev/null

echo "== bsdtar reference"
t0=$(date +%s.%N)
while read -r f; do bsdtar -xpf $D/pkg/$f -C $R/ref --exclude .PKGINFO --exclude .BUILDINFO --exclude .MTREE --exclude .INSTALL --exclude .CHANGELOG; done <$D/set.txt
sync; t1=$(date +%s.%N)
echo "== btrpkg fetch (encoded writes) + install"
nfail=0
while read -r f; do
  n=${f%.pkg.tar.zst}
  $B fetch --unverified $D/btr/$n.btrpkg $CACHE/$n.pkg.btr 2>>$D/fetch.log || { echo "FETCH FAIL $n"; nfail=$((nfail+1)); continue; }
  $B install $CACHE/$n.pkg.btr $R/btr 2>>$D/install.log || { echo "INSTALL FAIL $n"; nfail=$((nfail+1)); }
done <$D/set.txt
sync; t2=$(date +%s.%N)
awk -v a=$t0 -v b=$t1 -v c=$t2 'BEGIN{printf "bsdtar %.1f s, btrpkg %.1f s\n", b-a, c-b}'
echo "failures: $nfail"
grep -c 'encoded write unavailable' $D/fetch.log && echo "WARNING: encoded writes fell back" || echo "encoded writes: used"
echo "== compare"
python3 $D/compare_trees.py $R/ref $R/btr && echo "TREES MATCH"
echo "== extents of one large installed file"
big=$(find $R/btr/usr/lib -type f -size +2M | head -1); echo "$big"; filefrag -v "$big" | sed -n '4,6p'
[[ -n "${SUDO_UID:-}" ]] && chown -R ${SUDO_UID}:${SUDO_GID} $D || true
