#!/bin/bash
# Robustness of pacman's btrpkg downloads on a lab machine (root@host: bash pacman-robust.sh).
# Downloads only (-Sw), nothing is installed. Checks:
#   1. the network half runs as the DownloadUser, pull-ids as root
#   2. failover: a dead CacheServer first, then the real Server
#   3. interrupt during the chunk download, then run again: only the rest is fetched
#   4. a chunk that fails its hash on the first server is fetched from the next
#   5. DisableSandbox: still works (helper runs as root, no Landlock)
#   6. -Sc removes a leftover download directory
set -uo pipefail
PAC=${PACBTR:-/usr/bin/pacman-btrpkg}; BTRPKG=${BTRPKG:-/usr/bin/btrpkg}   # dev build: see lab-pacman-e2e.sh
C=/etc/btrpkg/pacman-test.conf; BC=/tmp/robust.conf
T=${T:-mise-bin omarchy-dev omarchy-local-ai omarchy-themes-intros owe omarchy-settings-dev gliff omawrite signed ufw-docker}
CACHE=/var/cache/pacman/pkg
DEV=$(ip -o route get 192.168.2.2 | grep -o 'dev [^ ]*' | cut -d' ' -f2)
rx() { awk -v d="$DEV:" '$1==d{print $2}' /proc/net/dev; }
clean() { for f in $(pacman --config $C -Sp --print-format "%f" $T); do rm -f $CACHE/${f%.pkg.tar.*}.pkg.btr $CACHE/${f%.pkg.tar.*}.pkg.btr.sig; done; rm -rf $CACHE/download-btrpkg $CACHE/download-btrpkg.want; }
conf() { # $1: extra lines for [options], $2: server lines for [omarchy]
  awk -v o="$1" -v s="$2" -v b="$BTRPKG" '
    /^\[options\]/ {print; print "BtrPkgHelper = " b " --h2c"; if (o!="") print o; next}
    /^\[omarchy\]/ {print; if (s!="") print s; inrepo=1; next}
    /^\[/ {inrepo=0}
    inrepo && /^Server/ {print; next}
    {print}' $C > $BC
}
fail=0; ok() { echo "ok - $*"; }; no() { echo "NOT OK - $*"; fail=1; }
cnt() { ls $CACHE | grep -c '\.pkg\.btr$'; }
pacman --config $C -Syy >/dev/null 2>&1
N=$(echo $T | wc -w)

# 1. who runs what
clean; conf "" ""
( while sleep 0.05; do ps -eo user:12,args | awk '/btrpkg/ && /(fetch-ids|pull-ids)/ && !/awk/{print $1, ($0 ~ /fetch-ids/ ? "fetch-ids" : "pull-ids")}'; done ) > /tmp/who.txt & W=$!
$PAC --config $BC -Sw --noconfirm $T >/tmp/r1.log 2>&1; rc=$?; kill $W
sort -u /tmp/who.txt > /tmp/who.u
[[ $rc == 0 && $(cnt) -ge $N ]] && ok "download ($N cache files)" || { no "download rc=$rc"; tail -5 /tmp/r1.log; }
grep -q '^alpm fetch-ids' /tmp/who.u && ! grep -q '^root fetch-ids' /tmp/who.u && ok "fetch-ids ran as alpm only" || no "fetch-ids user: $(cat /tmp/who.u | tr '\n' ' ')"
grep -q '^root pull-ids' /tmp/who.u && ok "pull-ids ran as root" || no "pull-ids not seen as root: $(cat /tmp/who.u | tr '\n' ' ')"
[[ ! -e $CACHE/download-btrpkg ]] && ok "download directory removed after success" || no "download directory left"

# 2. dead CacheServer first
clean; conf "" "CacheServer = http://192.168.2.2:1/omarchy/x86_64"
r0=$(rx); $PAC --config $BC -Sw --noconfirm $T >/tmp/r2.log 2>&1; rc=$?; r1=$(rx)
[[ $rc == 0 && $(cnt) -ge $N ]] && ok "dead CacheServer: fell back to Server ($(( (r1-r0)/1000000 )) MB)" || { no "failover rc=$rc"; tail -5 /tmp/r2.log; }

# 3. interrupt, then resume (only from-scratch downloads make the effect visible: no installed reuse
#    for omarchy-themes-intros' new videos)
clean; conf "" ""
r0=$(rx); $PAC --config $BC -Sw --noconfirm $T >/tmp/r3a.log 2>&1 & P=$!
while ! pgrep -f 'fetch-ids --want' >/dev/null; do sleep 0.02; kill -0 $P 2>/dev/null || break; done
sleep 0.25; kill -INT $P; pkill -INT -f 'fetch-ids --want'; wait $P; rca=$?; r1=$(rx)
part=$(find $CACHE/download-btrpkg/chunks -type f 2>/dev/null | wc -l)
$PAC --config $BC -Sw --noconfirm $T >/tmp/r3b.log 2>&1; rc=$?; r2=$(rx)
first=$(( (r1-r0)/1000000 )); second=$(( (r2-r1)/1000000 ))
[[ $rc == 0 && $(cnt) -ge $N ]] && ok "resume: first run stopped (rc $rca, $part chunks kept, $first MB), second run $second MB, together $((first+second)) MB" || { no "resume rc=$rc"; tail -5 /tmp/r3b.log; }

# 4. corrupt chunks on the first server must come from the next
clean; conf "" ""
# /var/tmp/robust-repo: a copy of the test repo's tree, x86_64/ and btrparts/ beside it (the
# database's btrpkg.parts=../btrparts holds for every server of the repository, so a cache server
# replicates the layout): scp -r desktop:Work/btrpkg-repo/omarchy/{x86_64,btrparts}
# /var/tmp/robust-repo/. Every btrpart >100k is damaged in the copy.
if [[ -d /var/tmp/robust-repo/btrparts ]]; then
  rm -rf /var/tmp/badrepo; cp -a --reflink=always /var/tmp/robust-repo /var/tmp/badrepo
  find /var/tmp/badrepo/btrparts -type f -size +100k | while read v; do printf 'XXXX' | dd of=$v bs=1 seek=1000 conv=notrunc 2>/dev/null; done
  conf "" "CacheServer = file:///var/tmp/badrepo/x86_64"
  $PAC --config $BC -Sw --noconfirm $T >/tmp/r4.log 2>&1; rc=$?
  [[ $rc == 0 && $(cnt) -ge $N ]] && ok "corrupt chunk on the first server: taken from the next" || { no "corrupt chunk rc=$rc"; tail -5 /tmp/r4.log; }
  rm -rf /var/tmp/badrepo
else echo "skip - corrupt chunk (no /var/tmp/robust-repo)"; fi

# 5. DisableSandbox
clean; conf "DisableSandbox" ""
$PAC --config $BC -Sw --noconfirm $T >/tmp/r5.log 2>&1; rc=$?
[[ $rc == 0 && $(cnt) -ge $N ]] && ok "DisableSandbox: works without the download user" || { no "DisableSandbox rc=$rc"; tail -5 /tmp/r5.log; }

# 6. -Sc removes leftovers (in a cache directory of its own: the real one keeps packages for rollbacks)
SC=/var/tmp/robust-sc/; rm -rf $SC; mkdir -p $SC/download-btrpkg/btrparts/aa; touch $SC/download-btrpkg/btrparts/aa/x $SC/download-btrpkg.want $SC/keep.txt
conf "CacheDir = $SC" ""; $PAC --config $BC -Sc --noconfirm >/dev/null 2>&1
[[ ! -e $SC/download-btrpkg && ! -e $SC/download-btrpkg.want && -e $SC/keep.txt ]] && ok "-Sc removes the leftover download directory (and keeps other files)" || no "-Sc: $(ls -A $SC | tr '\n' ' ')"
rm -rf $SC
clean
exit $fail
