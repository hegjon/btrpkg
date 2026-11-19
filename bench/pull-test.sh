#!/bin/bash
# btrpkg publish/pull end to end on a fresh btrfs on the bench partition (nvme0n1p3, wiped).
# Not a loop image: the desktop's udiskie automounts any btrfs that appears on a loop device.
# Run as root in the privileged exp-btrfs container:
#   docker run --rm --privileged -v /dev:/dev -v ~/Work/btrpkg:/w \
#     -v /var/cache/pacoloco/pkgs:/pkgs:ro --tmpfs /exp:size=2g exp-btrfs bash /w/bench/pull-test.sh
# Needs bench/out-store (btrpkg publish of the old and new versions) and bench/out-delta-pkgs.
# Layout as on Omarchy: @ as root, @pkg at <root>/var/cache/pacman/pkg, compress=zstd:3.
set -euo pipefail
B=/w/target/release/btrpkg; S=/w/bench/out-store; P=/w/bench/out-delta-pkgs
OUT=/w/bench/results; T=/mnt/t; R=/mnt/root; C=$R/var/cache/pacman/pkg
PAIRS="hyprland-0.57.1-2:hyprland-0.57.2-2 hyprutils-0.14.3-1:hyprutils-0.14.4-1
aquamarine-0.15.1-1:aquamarine-0.16.0-1 hyprtoolkit-0.6.0-2:hyprtoolkit-0.6.0-3
hyprgraphics-0.5.1-4:hyprgraphics-0.6.0-1 hyprcursor-0.1.13-7:hyprcursor-0.1.14-1
xdg-desktop-portal-hyprland-1.4.1-2:xdg-desktop-portal-hyprland-1.5.0-1
hyprland-guiutils-0.2.2-5:hyprland-guiutils-0.2.2-6 hyprpicker-0.4.7-4:hyprpicker-0.4.8-1"

used() { btrfs filesystem sync $T >/dev/null; btrfs filesystem usage -b $T 2>/dev/null | grep -m1 '^Data,' | grep -o 'Used:[0-9]*' | cut -d: -f2; }
L=/dev/nvme0n1p3
[[ -b $L ]] || { echo "$L missing"; exit 1; }
if findmnt -S $L >/dev/null; then echo "$L is mounted, refusing"; exit 1; fi
size=$(blockdev --getsize64 $L); (( size > 100*1024**3 && size < 120*1024**3 )) || { echo "unexpected size $size"; exit 1; }
wipefs -aq $L
mkfs.btrfs -q -f $L
mkdir -p $T $R
trap 'umount -R $R 2>/dev/null; umount $T 2>/dev/null; chown -R 1000:1000 $OUT' EXIT
mount -o compress=zstd:3 $L $T
btrfs -q subvolume create $T/@ $T/@pkg
mount -o compress=zstd:3,subvol=@ $L $R
mkdir -p $C
mount -o compress=zstd:3,subvol=@pkg $L $C

echo "== initial install of the old versions (nothing cached)"
u0=$(used)
for pr in $PAIRS; do o=${pr%%:*}; $B pull $S $o-x86_64 $C/$o.pkg.btr 2>/dev/null; $B install $C/$o.pkg.btr $R 2>/dev/null; done | tee $OUT/pull-hypr-old.tsv
u1=$(used)
echo "== upgrade: pull the new versions, reusing chunks of the cached old ones"
for pr in $PAIRS; do n=${pr##*:}; $B pull $S $n-x86_64 $C/$n.pkg.btr 2>/tmp/pull.err || { cat /tmp/pull.err; exit 1; }; if grep -q unavailable /tmp/pull.err; then cat /tmp/pull.err; fi; done | tee $OUT/pull-hypr-new.tsv
u2=$(used)
echo "== install the new versions over the old ones, then remove the old cache files"
for pr in $PAIRS; do n=${pr##*:}; $B install $C/$n.pkg.btr $R 2>/dev/null; done
u3=$(used)
for pr in $PAIRS; do o=${pr%%:*}; rm $C/$o.pkg.btr $C/$o.pkg.btr.btridx; done
u4=$(used)
mb() { awk "BEGIN{printf \"%.1f MB\", $1/1e6}"; }
echo "data used: old installed + cached $(mb $u1); + new versions pulled $(mb $u2); new installed over old $(mb $u3); old cache files removed $(mb $u4)"

echo "== verify (old cache files already removed): new versions installed from the pulled cache files vs bsdtar"
mkdir -p $R/a $R/b
for pr in $PAIRS; do
  n=${pr##*:}
  bsdtar -xp --numeric-owner -f $P/$n-x86_64.pkg.tar.zst -C $R/a \
    --exclude .PKGINFO --exclude .BUILDINFO --exclude .MTREE --exclude .INSTALL --exclude .CHANGELOG
  $B install $C/$n.pkg.btr $R/b 2>/dev/null
done
python3 /w/bench/compare.py $R/a $R/b | tee $OUT/pull-hypr-verify.txt
filefrag -v $R/b/usr/share/hypr/wall0.png | sed -n '4,5p'
filefrag -v $R/b/usr/bin/Hyprland | sed -n '4,5p'


echo "== corrupt chunk in the store: pull must fail and leave nothing behind"
cp -a $S /exp/store-bad
h=$(python3 - /exp/store-bad/index/hyprland-0.57.2-2-x86_64.btridx <<'EOF'
import struct, sys
b = open(sys.argv[1], "rb").read()
meta_len, n = struct.unpack_from("<QQ", b, 16)
t = 64 + meta_len
print(b[t + 24:t + 56].hex())   # first chunk of the image
EOF
)
f=/exp/store-bad/chunks/${h:0:2}/$h
printf '\xff' | dd of=$f bs=1 seek=100 conv=notrunc status=none
mkdir -p $C/fresh
if $B pull /exp/store-bad hyprland-0.57.2-2-x86_64 $C/fresh/hyprland.pkg.btr 2>&1; then echo "FAIL: corrupt chunk accepted"; exit 1; fi
ls -A $C/fresh | grep . && { echo "FAIL: partial file left"; exit 1; } || echo "ok: refused, nothing left"
echo "== tampered index digest"
printf '\x00' | dd of=/exp/store-bad/index/hyprland-0.57.2-2-x86_64.btridx bs=1 seek=200 conv=notrunc status=none
if $B pull /exp/store-bad hyprland-0.57.2-2-x86_64 $C/fresh/hyprland.pkg.btr 2>&1; then echo "FAIL: bad index accepted"; exit 1; fi
ls -A $C/fresh | grep . && { echo "FAIL: partial file left"; exit 1; } || echo "ok: refused, nothing left"
