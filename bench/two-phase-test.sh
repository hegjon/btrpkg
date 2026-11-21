#!/bin/bash
# Two-phase download (fetch-ids unprivileged with Landlock, then pull-ids from the directory),
# as a normal user against the test repo on http://127.0.0.1:8790. Checks: a plain run, resume,
# server failover, and the missing-chunk round after a locally changed installed file.
set -uo pipefail
B=${B:-$(dirname $0)/../target/release/btrpkg}
R=${R:-$HOME/Work/btrpkg-repo/omarchy/x86_64}
U=http://127.0.0.1:8790/omarchy/x86_64
T=$(mktemp -d); trap 'rm -rf $T' EXIT
id() { $B dump $R/index/$1.btridx | awk '/^id/{print $2}'; }
P1=owe-lockfeed-0.2.18-1-x86_64; P2=aether-4.33.0-1-x86_64
ARGS="$P1=$(id $P1) $P2=$(id $P2)"
fail=0; ok() { echo "ok - $1"; }; no() { echo "NOT OK - $1"; fail=1; }

mkdir -p $T/d1 $T/c1
$B fetch-ids --landlock --progress --server $U $T/d1 $ARGS > $T/p1 2>$T/e1 && ok "fetch-ids with landlock" || { no "fetch-ids"; cat $T/e1; }
grep -c '^total' $T/p1 | grep -qx 2 && grep -q "^done	$P2" $T/p1 && ok "progress lines" || no "progress lines"
$B fetch-ids --landlock --progress --server $U $T/d1 $ARGS > $T/p2 2>&1
awk -F'\t' '$1=="total" && $3!=0' $T/p2 | grep -q . && no "resume: fetched again" || ok "resume: nothing fetched twice"
$B --no-fsync pull-ids --missing-to $T/m $T/d1 $T/c1 $ARGS > /dev/null 2>$T/e2 && ok "pull-ids from the directory" || { no "pull-ids"; cat $T/e2; }
[ -s $T/c1/$P2.pkg.btr.sig ] && ok "signature copied" || no "signature copied"
# landlock: DIR/index is a symlink to a directory outside DIR, so writing the index there must be refused
mkdir -p $T/d3 $T/outside; ln -s $T/outside $T/d3/index
$B fetch-ids --landlock --server $U $T/d3 $P1=$(id $P1) >/dev/null 2>$T/e4; rc=$?
[ $rc != 0 ] && [ -z "$(ls $T/outside)" ] && ok "landlock refuses a write outside DIR ($(grep -o 'Permission denied' $T/e4 | head -1))" || { no "landlock: wrote outside DIR"; cat $T/e4; }
$B fetch-ids --server $U $T/d3 $P1=$(id $P1) >/dev/null 2>&1; [ -n "$(ls $T/outside)" ] && ok "(control: without --landlock the write goes through)" || no "control"
# failover: dead server first, then a directory store
rm -rf $T/d2; mkdir -p $T/d2
$B fetch-ids --server http://127.0.0.1:1/x --server $R $T/d2 $ARGS > /dev/null 2>$T/e3 && grep -q "trying the next server\|no server\|127.0.0.1:1" $T/e3 && ok "failover to the second server" || { no "failover"; cat $T/e3; }
# changed installed file: install P2, change a file in place (same size), pull again with reuse
mkdir -p $T/root; $B install $T/c1/$P2.pkg.btr $T/root >/dev/null 2>&1
f=$(cd $T/root && find . -type f -size +200k | head -1)
printf 'XXXXXXXX' | dd of=$T/root/$f bs=1 seek=100 conv=notrunc 2>/dev/null
rm -rf $T/d4 $T/c4; mkdir -p $T/d4 $T/c4
$B fetch-ids --installed $T/root --server $U $T/d4 $P2=$(id $P2) > /dev/null 2>&1
$B --no-fsync --installed $T/root pull-ids --missing-to $T/m4 $T/d4 $T/c4 $P2=$(id $P2) > /dev/null 2>$T/e5; rc=$?
[ $rc = 3 ] && [ -s $T/m4 ] && ok "changed file: pull-ids exits 3 and lists $(wc -l < $T/m4) chunks" || { no "exit 3 (got $rc)"; cat $T/e5; }
ls $T/c4 | grep -q . && no "partial cache file left" || ok "no partial cache file"
$B fetch-ids --want $T/m4 --server $U $T/d4 $P2=$(id $P2) > /dev/null 2>&1 && ok "fetch-ids --want" || no "--want"
$B --no-fsync --installed $T/root pull-ids $T/d4 $T/c4 $P2=$(id $P2) > /dev/null 2>$T/e6 && ok "second pull-ids" || { no "second pull-ids"; cat $T/e6; }
cmp <($B dump $T/c4/$P2.pkg.btr) <($B dump $R/index/$P2.btridx) && ok "cache file has the right index" || no "index differs"
$B install $T/c4/$P2.pkg.btr $T/root2 >/dev/null 2>&1; $B install $T/c1/$P2.pkg.btr $T/root3 >/dev/null 2>&1
diff -r $T/root2 $T/root3 >/dev/null && ok "installed tree identical to a fresh pull" || no "trees differ"
exit $fail
