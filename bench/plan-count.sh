#!/bin/bash
# How often pacman-btrpkg runs each btrpkg step for one -Sw of the robust test's targets:
# BtrPkgHelper is a wrapper that logs its arguments. One repository, so each step should run once.
set -u
PAC=${PACBTR:-/usr/bin/pacman-btrpkg}
C=/etc/btrpkg/pacman-test.conf; BC=/tmp/plan-count.conf; LOG=/tmp/plan-count.log
T=${T:-mise-bin omarchy-dev omarchy-local-ai omarchy-themes-intros owe omarchy-settings-dev gliff omawrite signed ufw-docker}
CACHE=/var/cache/pacman/pkg
W=/usr/local/bin/btrpkg-logged
printf '#!/bin/sh\necho "$*" >> %s\nexec /usr/bin/btrpkg "$@"\n' "$LOG" > $W; chmod 755 $W
for f in $(pacman --config $C -Sp --print-format "%f" $T); do rm -f $CACHE/${f%.pkg.tar.*}.pkg.btr $CACHE/${f%.pkg.tar.*}.pkg.btr.sig; done
rm -rf $CACHE/download-btrpkg $CACHE/download-btrpkg.want
awk -v w=$W '/^\[options\]/ {print; print "BtrPkgHelper = " w " --h2c"; next} {print}' $C > $BC
$PAC --config $BC -Syy >/dev/null 2>&1 # the test repository's database (another test may have left the system's)
: > $LOG; chmod 666 $LOG
$PAC --config $BC -Sw --noconfirm $T >/tmp/plan-count.out 2>&1; rc=$?
echo "$($PAC --version | grep -o 'Pacman v[^ ]*') $(pacman -Q pacman-btrpkg): rc $rc"
echo "  fetch-ids --index-only: $(grep -c -- '--index-only' $LOG)"
echo "  pull-ids --plan:        $(grep -c -- 'pull-ids --plan' $LOG)"
echo "  fetch-ids (data):       $(grep -- 'fetch-ids' $LOG | grep -vc -- '--index-only')"
echo "  pull-ids (assemble):    $(grep -- 'pull-ids' $LOG | grep -vc -- '--plan')"
rm -f $W $BC
