#!/bin/bash
#
# pacman-btrpkg's -U of .pkg.btr files and its validation record, on a lab machine (root).
#
#   pacman-gaps-test.sh PKGFILE.pkg.btr
#
# PKGFILE: a cache file whose package is installed at that version (the tests reinstall it), with
# its .sig next to it, signed by a key in /etc/btrpkg/keys.
# PACBTR: the pacman under test (default: /usr/bin/pacman-btrpkg). Its config needs BtrPkgHelper.
set -uo pipefail
P=$1
PACBTR=${PACBTR:-/usr/bin/pacman-btrpkg}
W=/var/tmp/btrpkg-gaps
name=$(basename "$P" | sed -E 's/-[^-]+-[^-]+-[^-]+\.pkg\.btr$//')
desc() { cat /var/lib/pacman/local/"$1"-[0-9]*/desc; }
field() { desc "$1" | awk -v f="%$2%" '$0 == f { on = 1; next } /^$/ { on = 0 } on' | paste -sd' '; }
pass=0 fail=0
ok() { echo "ok   - $*"; pass=$((pass + 1)); }
bad() { echo "FAIL - $*"; fail=$((fail + 1)); }
check() { local what=$1; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }

rm -rf "$W"; mkdir -p "$W"/{signed,nosig,bad,badsig,fake/usr/share/btrgaps-fake}
cp "$P" "$P.sig" "$W/signed/"
cp "$P" "$W/nosig/"
cp "$P" "$W/bad/"
cp "$P" "$W/badsig/"
other=$(find /var/cache/pacman/pkg -name '*.pkg.btr.sig' ! -name "$(basename "$P").sig" | head -1)
cp "$other" "$W/badsig/$(basename "$P").sig"
f=$(basename "$P")

echo "== -U with its signify signature"
out=$($PACBTR -U --noconfirm "$W/signed/$f" 2>&1); rc=$?
check "-U of a signed .pkg.btr succeeds (rc $rc)" test $rc = 0
check "no GPGME involvement" bash -c "! grep -qi gpgme <<<\"\$1\"" _ "$out"
check "%VALIDATION% is none, not pgp: '$(field "$name" VALIDATION)'" test "$(field "$name" VALIDATION)" = none
check "%XDATA% records the signer: '$(field "$name" XDATA)'" bash -c "grep -q 'btrpkg-validation=signify:' <<<\"\$1\"" _ "$(field "$name" XDATA)"
q=$(/usr/bin/pacman -Qi "$name" 2>&1)
check "stock pacman reads it without a warning" bash -c "! grep -qi warning <<<\"\$1\"" _ "$q"
echo "   stock -Qi: $(grep -E '^(Validated By|Extended Data)' <<<"$q" | tr -s ' ' | paste -sd'|')"
q=$($PACBTR -Qip "$W/signed/$f" 2>&1)
check "-Qip: no GPGME error" bash -c "! grep -qi gpgme <<<\"\$1\"" _ "$q"

echo "== -U without a signature (LocalFileSigLevel = Optional)"
$PACBTR -U --noconfirm "$W/nosig/$f" >/dev/null 2>&1; rc=$?
check "installs (rc $rc)" test $rc = 0
check "validation none, no signer record: '$(field "$name" XDATA)'" bash -c "test \"\$1\" = none && ! grep -q btrpkg-validation <<<\"\$2\"" _ "$(field "$name" VALIDATION)" "$(field "$name" XDATA)"

echo "== -U without a signature, LocalFileSigLevel = Required"
sed 's/^LocalFileSigLevel.*/LocalFileSigLevel = Required/' /etc/pacman.conf > "$W/required.conf"
grep -q '^BtrPkgHelper' "$W/required.conf" || sed -i '/^\[options\]/a BtrPkgHelper = /usr/bin/btrpkg' "$W/required.conf"
out=$($PACBTR --config "$W/required.conf" -U --noconfirm "$W/nosig/$f" 2>&1); rc=$?
check "refused (rc $rc): $(tail -1 <<<"$out")" test $rc != 0

echo "== -U with another package's signature"
out=$($PACBTR -U --noconfirm "$W/badsig/$f" 2>&1); rc=$?
check "refused (rc $rc): $(grep -m1 -i signature <<<"$out")" test $rc != 0

echo "== -U of a file whose data was changed"
base=$(btrpkg dump "$P" | awk '$1 == "header" { for (i = 1; i <= NF; i++) if ($i == "base") print $(i + 1) }')
off=$(btrpkg dump "$P" | awk '$1 == "c" { print $2; exit }')
pos=$((base + off + 100))
orig=$(dd if="$W/bad/$f" bs=1 skip=$pos count=1 2>/dev/null | od -An -tu1 | tr -d ' ')
printf "\\$(printf '%03o' $(( (orig + 1) % 256 )))" | dd of="$W/bad/$f" bs=1 seek=$pos conv=notrunc 2>/dev/null
out=$($PACBTR -U --noconfirm "$W/bad/$f" 2>&1); rc=$?
check "refused (rc $rc): $(grep -m1 -iE 'match|error' <<<"$out")" test $rc != 0

echo "== a package's own .PKGINFO can't claim a signature"
printf '%s\n' 'pkgname = btrgaps-fake' 'pkgbase = btrgaps-fake' 'pkgver = 1-1' 'pkgdesc = btrpkg gaps test' \
  'url = https://example.org' 'builddate = 1' 'packager = test' 'size = 2' 'arch = any' 'license = none' \
  'xdata = pkgtype=pkg' 'xdata = btrpkg-validation=signify:fake.pub' > "$W/fake/.PKGINFO"
echo x > "$W/fake/usr/share/btrgaps-fake/x"
(cd "$W/fake" && bsdtar --zstd -cf ../btrgaps-fake-1-1-any.pkg.tar.zst .PKGINFO usr)
$PACBTR -U --noconfirm "$W/btrgaps-fake-1-1-any.pkg.tar.zst" >/dev/null 2>&1
check "the claim is dropped: '$(field btrgaps-fake XDATA)'" bash -c "! grep -q btrpkg-validation <<<\"\$1\"" _ "$(field btrgaps-fake XDATA)"
$PACBTR -R --noconfirm btrgaps-fake >/dev/null 2>&1

echo "== -S from the repository (the cache file and its .sig)"
$PACBTR -S --noconfirm "$name" >/dev/null 2>&1; rc=$?
check "reinstalls (rc $rc)" test $rc = 0
check "%VALIDATION% is sha256 only: '$(field "$name" VALIDATION)'" test "$(field "$name" VALIDATION)" = sha256
check "%XDATA% records the signer: '$(field "$name" XDATA)'" bash -c "grep -q 'btrpkg-validation=signify:btrpkg-test.pub' <<<\"\$1\"" _ "$(field "$name" XDATA)"
check "files intact: $(/usr/bin/pacman -Qkk "$name" 2>&1 | tail -1)" bash -c "/usr/bin/pacman -Qkk \"\$1\" 2>&1 | tail -1 | grep -q ' 0 altered'" _ "$name"
rm -rf "$W"
echo "passed $pass, failed $fail"
[[ $fail == 0 ]]
