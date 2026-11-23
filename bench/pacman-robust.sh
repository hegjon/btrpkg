#!/bin/bash
# Robustness of pacman's btrpkg downloads on a lab machine (root@host: bash pacman-robust.sh).
# Downloads only (-Sw), nothing is installed. Checks:
#   1. the network half runs as the DownloadUser, pull-ids as root
#   2. failover: a dead CacheServer first, then the real Server
#   3. interrupt during the chunk download, then run again: only the rest is fetched
#   4. a chunk that fails its hash on the first server is fetched from the next
#   5. DisableSandbox: still works (helper runs as root, no Landlock)
#   6. -Sc removes a leftover download directory
#   7.-9. fallbacks to the package file (no index, failed download, without the download user)
# Throughout, a target whose planned btrpkg download is larger than its package file comes as the
# package file (1 checks that against pacman's debug lines); every other one as a cache file.
set -uo pipefail
PAC=${PACBTR:-/usr/bin/pacman-btrpkg}; BTRPKG=${BTRPKG:-/usr/bin/btrpkg}   # dev build: see lab-pacman-e2e.sh
C=/etc/btrpkg/pacman-test.conf; BC=/tmp/robust.conf
T=${T:-mise-bin omarchy-dev omarchy-local-ai omarchy-themes-intros owe omarchy-settings-dev gliff omawrite signed ufw-docker}
CACHE=/var/cache/pacman/pkg
DEV=$(ip -o route get 192.168.2.2 | grep -o 'dev [^ ]*' | cut -d' ' -f2)
rx() { awk -v d="$DEV:" '$1==d{print $2}' /proc/net/dev; }
clean() { for f in $(pacman --config $C -Sp --print-format "%f" $T); do rm -f $CACHE/${f%.pkg.tar.*}.pkg.btr $CACHE/${f%.pkg.tar.*}.pkg.btr.sig "$CACHE/$f" "$CACHE/$f.sig"; done; rm -rf $CACHE/download-btrpkg $CACHE/download-btrpkg.want; }
conf() { # $1: extra lines for [options], $2: server lines for [omarchy], $3: "only" drops the others
  awk -v o="$1" -v s="$2" -v only="${3:-}" -v b="$BTRPKG" '
    /^\[options\]/ {print; print "BtrPkgHelper = " b " --h2c"; if (o!="") print o; next}
    /^\[omarchy\]/ {print; if (s!="") print s; inrepo=1; next}
    /^\[/ {inrepo=0}
    inrepo && /^Server/ {if (only=="") print; next}
    {print}' $C > $BC
}
# package files (not .pkg.btr) of $T in the cache, and removing them again
plain() { local n=0; for f in $(pacman --config $C -Sp --print-format "%f" $T); do [[ -e $CACHE/$f ]] && n=$((n+1)); done; echo $n; }
# .pkg.btr files of $T in the cache
btrs() { local n=0; for f in $(pacman --config $C -Sp --print-format "%f" $T); do [[ -e $CACHE/${f%.pkg.tar.*}.pkg.btr ]] && n=$((n+1)); done; echo $n; }
unplain() { for f in $(pacman --config $C -Sp --print-format "%f" $T); do rm -f "$CACHE/$f" "$CACHE/$f.sig"; done; }
fail=0; ok() { echo "ok - $*"; }; no() { echo "NOT OK - $*"; fail=1; }
# every target in the cache, each as the form test 1 found (SMALLER: those whose package file is the
# smaller download)
all_there() { [[ $(btrs) == $((N-SMALLER)) && $(plain) == "$SMALLER" ]]; }
# $C's [omarchy] is the test repository, but in the system's DBPath: put the system's databases
# back on exit (-Syy: the test db's mtime is newer, so a plain -Sy would keep it)
trap 'pacman -Syy >/dev/null 2>&1' EXIT
pacman --config $C -Syy >/dev/null 2>&1
N=$(echo $T | wc -w)

# 1. who runs what: the helper is a wrapper that logs its user and arguments (sampling ps missed
#    fetch-ids runs shorter than its interval)
clean
WRAP=/usr/local/bin/btrpkg-who; : > /tmp/who.txt; chmod 666 /tmp/who.txt
printf '#!/bin/sh\necho "$(id -un) $*" >> /tmp/who.txt\nexec %s "$@"\n' "$BTRPKG" > $WRAP; chmod 755 $WRAP
B0=$BTRPKG; BTRPKG=$WRAP; conf "" ""; BTRPKG=$B0
$PAC --config $BC -Sw --noconfirm --debug $T >/tmp/r1.log 2>&1; rc=$?; rm -f $WRAP
awk '{print $1, (/ fetch-ids / ? "fetch-ids" : (/ pull-ids / ? "pull-ids" : "other"))}' /tmp/who.txt | sort -u > /tmp/who.u
conf "" ""
SMALLER=$(grep -c 'as btrpkg, .*: using the package file' /tmp/r1.log)
[[ $rc == 0 ]] && all_there && ok "download ($(btrs) cache files, $SMALLER package files where those are smaller)" || { no "download rc=$rc btr=$(btrs) plain=$(plain) smaller=$SMALLER"; grep 'using the package file' /tmp/r1.log; }
grep -q '^alpm fetch-ids' /tmp/who.u && ! grep -q '^root fetch-ids' /tmp/who.u && ok "fetch-ids ran as alpm only" || no "fetch-ids user: $(cat /tmp/who.u | tr '\n' ' ')"
grep -q '^root pull-ids' /tmp/who.u && ok "pull-ids ran as root" || no "pull-ids not seen as root: $(cat /tmp/who.u | tr '\n' ' ')"
[[ ! -e $CACHE/download-btrpkg ]] && ok "download directory removed after success" || no "download directory left"

# 2. dead CacheServer first
clean; conf "" "CacheServer = http://192.168.2.2:1/omarchy/x86_64"
r0=$(rx); $PAC --config $BC -Sw --noconfirm $T >/tmp/r2.log 2>&1; rc=$?; r1=$(rx)
[[ $rc == 0 ]] && all_there && ok "dead CacheServer: fell back to Server ($(( (r1-r0)/1000000 )) MB)" || { no "failover rc=$rc btr=$(btrs) plain=$(plain)"; tail -5 /tmp/r2.log; }

# 3. interrupt, then resume (only from-scratch downloads make the effect visible: no installed reuse
#    for omarchy-themes-intros' new videos)
clean; conf "" ""
r0=$(rx); $PAC --config $BC -Sw --noconfirm $T >/tmp/r3a.log 2>&1 & P=$!
while ! pgrep -f 'fetch-ids --want' >/dev/null; do sleep 0.02; kill -0 $P 2>/dev/null || break; done
sleep 0.25; kill -INT $P; pkill -INT -f 'fetch-ids --want'; wait $P; rca=$?; r1=$(rx)
part=$(find $CACHE/download-btrpkg/btrparts -type f 2>/dev/null | wc -l)
$PAC --config $BC -Sw --noconfirm $T >/tmp/r3b.log 2>&1; rc=$?; r2=$(rx)
first=$(( (r1-r0)/1000000 )); second=$(( (r2-r1)/1000000 ))
[[ $rc == 0 ]] && all_there && ok "resume: first run stopped (rc $rca, $part chunks kept, $first MB), second run $second MB, together $((first+second)) MB" || { no "resume rc=$rc btr=$(btrs) plain=$(plain)"; tail -5 /tmp/r3b.log; }

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
  [[ $rc == 0 ]] && all_there && ok "corrupt chunk on the first server: taken from the next" || { no "corrupt chunk rc=$rc btr=$(btrs) plain=$(plain)"; tail -5 /tmp/r4.log; }
  rm -rf /var/tmp/badrepo
else echo "skip - corrupt chunk (no /var/tmp/robust-repo)"; fi

# 5. DisableSandbox
clean; conf "DisableSandbox" ""
$PAC --config $BC -Sw --noconfirm $T >/tmp/r5.log 2>&1; rc=$?
[[ $rc == 0 ]] && all_there && ok "DisableSandbox: works without the download user" || { no "DisableSandbox rc=$rc btr=$(btrs) plain=$(plain)"; tail -5 /tmp/r5.log; }

# 7.-9. fallback to the package file. A copy of the test repository is the only server.
if [[ -d /var/tmp/robust-repo/btrparts ]]; then
  first=${T%% *}
  ff=$(pacman --config $C -Sp --print-format "%f" $first); stem=${ff%.pkg.tar.*}
  # 7. no server has the index of one package: it comes as its package file, the rest as btrpkg
  rm -rf /var/tmp/noidx; cp -a --reflink=always /var/tmp/robust-repo /var/tmp/noidx
  rm -f /var/tmp/noidx/x86_64/$stem.pkg.btr /var/tmp/noidx/x86_64/$stem.pkg.btr.sig
  clean; unplain; conf "" "Server = file:///var/tmp/noidx/x86_64" only
  $PAC --config $BC -Syy >/dev/null 2>&1
  $PAC --config $BC -Sw --noconfirm $T >/tmp/r7.log 2>&1; rc=$?
  [[ $rc == 0 && $(btrs) == $((N-1-SMALLER)) && $(plain) == $((1+SMALLER)) && -e $CACHE/$ff ]] && grep -q "no server has the index of $first" /tmp/r7.log \
    && ok "no index anywhere: $first as its package file, $(btrs) as btrpkg" || { no "no index rc=$rc btr=$(btrs) plain=$(plain)"; tail -5 /tmp/r7.log; }
  # 8. the btrpkg download fails (no btrparts at all): every package as its package file
  clean; unplain; rm -rf /var/tmp/noidx/btrparts; cp -a /var/tmp/robust-repo/x86_64/$stem.pkg.btr* /var/tmp/noidx/x86_64/
  $PAC --config $BC -Sw --noconfirm $T >/tmp/r8.log 2>&1; rc=$?
  [[ $rc == 0 && $(plain) == "$N" && $(btrs) == 0 ]] && grep -q "downloading the package files instead" /tmp/r8.log \
    && ok "btrpkg download failed: all $(plain) as package files" || { no "no btrparts rc=$rc btr=$(btrs) plain=$(plain)"; tail -5 /tmp/r8.log; }
  # 9. as 7, without the download user (DisableSandbox, pacstrap's way)
  rm -rf /var/tmp/noidx; cp -a --reflink=always /var/tmp/robust-repo /var/tmp/noidx
  rm -f /var/tmp/noidx/x86_64/$stem.pkg.btr /var/tmp/noidx/x86_64/$stem.pkg.btr.sig
  clean; unplain; conf "DisableSandbox" "Server = file:///var/tmp/noidx/x86_64" only
  $PAC --config $BC -Sw --noconfirm $T >/tmp/r9.log 2>&1; rc=$?
  [[ $rc == 0 && $(btrs) == $((N-1-SMALLER)) && $(plain) == $((1+SMALLER)) && -e $CACHE/$ff ]] && grep -q "no server has the index of $first" /tmp/r9.log \
    && ok "no index, DisableSandbox: $first as its package file, $(btrs) as btrpkg" || { no "no index (DisableSandbox) rc=$rc btr=$(btrs) plain=$(plain)"; tail -5 /tmp/r9.log; }
  clean; unplain; rm -rf /var/tmp/noidx
  conf "" ""; $PAC --config $BC -Syy >/dev/null 2>&1
else echo "skip - fallback (no /var/tmp/robust-repo)"; fi

# 6. -Sc removes leftovers (in a cache directory of its own: the real one keeps packages for rollbacks)
SC=/var/tmp/robust-sc/; rm -rf $SC; mkdir -p $SC/download-btrpkg/btrparts/aa; touch $SC/download-btrpkg/btrparts/aa/x $SC/download-btrpkg.want $SC/keep.txt
conf "CacheDir = $SC" ""; $PAC --config $BC -Sc --noconfirm >/dev/null 2>&1
[[ ! -e $SC/download-btrpkg && ! -e $SC/download-btrpkg.want && -e $SC/keep.txt ]] && ok "-Sc removes the leftover download directory (and keeps other files)" || no "-Sc: $(ls -A $SC | tr '\n' ' ')"
rm -rf $SC
clean
exit $fail
