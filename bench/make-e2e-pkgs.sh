#!/bin/bash
# Two versions of a test package for the libalpm btrpkg tests, built by hand (as root, so it can
# carry security.capability): run in a container with OUT mounted.
#   bash make-e2e-pkgs.sh OUT
# Contents: usr/bin/btrpkg-e2e-tool (300 KB, cap_net_raw), etc/btrpkg-e2e/big.conf (200 KB,
# backup; changes in 2.0), etc/btrpkg-e2e/small.conf (backup; unchanged), a symlink, a hardlink,
# and an install script.
set -euo pipefail
OUT=$1
mk() { # $1 version
  local v=$1 d; d=$(mktemp -d)
  mkdir -p $d/usr/bin $d/etc/btrpkg-e2e $d/usr/share/btrpkg-e2e
  python3 - "$d" "$v" <<'EOF'
import random, sys
d, v = sys.argv[1], sys.argv[2]
r = random.Random(1)
tool = bytearray(r.randbytes(300_000))
if v == "2.0": tool[150_000:150_100] = b"\x02" * 100   # one chunk changes
open(f"{d}/usr/bin/btrpkg-e2e-tool", "wb").write(tool)
big = "".join(f"setting{i} = {r.randrange(10**9)}\n" for i in range(9000))
if v == "2.0": big += "new_setting = 1\n"
open(f"{d}/etc/btrpkg-e2e/big.conf", "w").write(big)
open(f"{d}/etc/btrpkg-e2e/small.conf", "w").write("small = 1\n")
open(f"{d}/usr/share/btrpkg-e2e/readme", "w").write(f"btrpkg e2e {v}\n" * 2000)
EOF
  chmod 755 $d/usr/bin/btrpkg-e2e-tool
  setfattr -n security.capability -v 0x0100000200200000000000000000000000000000 $d/usr/bin/btrpkg-e2e-tool
  ln -s ../../bin/btrpkg-e2e-tool $d/usr/share/btrpkg-e2e/tool-link
  ln $d/usr/share/btrpkg-e2e/readme $d/usr/share/btrpkg-e2e/readme-hardlink
  cat > $d/.INSTALL <<'EOF'
post_install() { echo "btrpkg-e2e: post_install $1" > /var/log/btrpkg-e2e.install; }
post_upgrade() { echo "btrpkg-e2e: post_upgrade $1 from $2" > /var/log/btrpkg-e2e.install; }
EOF
  local size; size=$(du -sb --exclude=.INSTALL $d | cut -f1)
  cat > $d/.PKGINFO <<EOF
pkgname = btrpkg-e2e
pkgbase = btrpkg-e2e
pkgver = $v-1
pkgdesc = btrpkg end-to-end test package
url = https://github.com/hegjon/btrpkg
builddate = 1763700000
packager = btrpkg tests
size = $size
arch = any
license = MIT
backup = etc/btrpkg-e2e/big.conf
backup = etc/btrpkg-e2e/small.conf
EOF
  find $d -exec touch -h -d @1763700000 {} +
  ( cd $d
    LANG=C bsdtar -cf - --format=mtree --options='!all,use-set,type,uid,gid,mode,time,size,md5,sha256,link' \
      --null --files-from <(find . -mindepth 1 \( -name .PKGINFO -o -name .INSTALL -o ! -name '.*' \) -print0 | LC_ALL=C sort -z) \
      --exclude .MTREE | gzip -c -f -n > .MTREE
    touch -d @1763700000 .MTREE
    LANG=C bsdtar --xattrs -cnf - --format=pax --null --files-from \
      <(printf '%s\0' .MTREE .PKGINFO .INSTALL; find . -mindepth 1 ! -name '.*' -printf '%P\0' | LC_ALL=C sort -z) \
      | zstd -q -19 -o $OUT/btrpkg-e2e-$v-1-any.pkg.tar.zst )
  rm -rf $d
}
mk 1.0
mk 2.0
ls -la $OUT
