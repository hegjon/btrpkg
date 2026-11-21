#!/bin/bash
# Keep a btrpkg mirror of Omarchy's edge repository up to date (run by btrpkg-mirror.timer, or by hand).
#   mirror/omarchy-mirror.sh
# Each run:
#   1. fetch upstream's omarchy.db (conditional GET; nothing to do when it hasn't changed and the
#      last run succeeded)
#   2. download the .pkg.tar.zst and .sig of every wanted package (wanted.txt, plus packages clients
#      asked the mirror for and got a 404, learned from caddy's access log; ALL=1: every package)
#      that the mirror doesn't have yet
#   3. btrpkg repo-update --db-in: check each new file against the database's SHA-256 and Omarchy's
#      OpenPGP signature (gpgv, Omarchy's keyring), convert, publish chunks and the index signed with
#      the mirror's key, then replace the served omarchy.db with upstream's plus btrpkg %DATA%.
# Packages not converted keep their plain entry; pacman downloads them as usual, from the next
# server when the mirror has no file. Old versions stay (clients with an older database can still
# fetch them); removing unused files and chunks is the repo pipeline's job (PLAN step 7).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
UP=${UP:-http://127.0.0.1:9129/repo/omarchy-edge-x86_64}       # pacoloco, then pkgs.omarchy.org
M=${M:-$HOME/Work/btrpkg-repo/omarchy-mirror/x86_64}
KEY=${KEY:-$HOME/Work/btrpkg-repo/keys/btrpkg-test.sec}
B=${B:-$here/../target/release/btrpkg}
WANT=${WANT:-$here/wanted.txt}
LOG=${LOG:-$HOME/Work/btrpkg-repo/access.log}      # caddy's JSON access log: 404s teach the wanted list
JOBS=${JOBS:-4}
mkdir -p "$M"
exec 9>"$M/.lock"
flock -n 9 || { echo "another run is busy"; exit 0; }
log() { echo "$(date '+%F %T') $*"; }

# Omarchy's keyring as gpgv wants it (binary), refreshed when the system's copy changes
KR=$M/.omarchy-keyring.gpg
if [[ ! -s $KR || /usr/share/pacman/keyrings/omarchy.gpg -nt $KR ]]; then
  gpg --dearmor < /usr/share/pacman/keyrings/omarchy.gpg > "$KR.tmp" && mv "$KR.tmp" "$KR"
fi

# 1. upstream database
cur=$M/.upstream.db; unchanged=
code=$(curl -sS -o "$cur.new" -w '%{http_code}' -z "$cur" "$UP/omarchy.db" || echo 000)
case $code in
  200) mv "$cur.new" "$cur" ;;
  304) rm -f "$cur.new"
       if [[ -e $M/.done && $M/.done -nt $cur && -z ${FORCE:-} ]]; then unchanged=1; fi ;;
  *)   rm -f "$cur.new"; log "upstream database: HTTP $code"; exit 1 ;;
esac
[[ -s $cur ]] || { log "no upstream database"; exit 1; }

# 2. package files: NAME FILENAME of every package in upstream's database
list=$(mktemp); trap 'rm -f "$list"' EXIT
bsdtar -xOf "$cur" '*/desc' | awk '/^%FILENAME%/{getline; f=$0} /^%NAME%/{getline; n=$0} /^%BASE%/{print n, f}' > "$list"
# (every desc has %BASE% after %NAME% and %FILENAME%; check the count)
[[ $(wc -l < "$list") -eq $(bsdtar -tf "$cur" | grep -c '/desc$') ]] || { log "could not read the database"; exit 1; }
# learn: packages a client asked the mirror for and didn't get (404) are converted from now on
learned=$M/.wanted-learned; [[ -e $learned ]] || : > "$learned"
if [[ -r $LOG ]]; then
  python3 - "$LOG" "$list" "$learned" <<'PY'
import json, sys, re
log, lst, out = sys.argv[1:4]
byfile = dict(reversed(l.split()) for l in open(lst) if l.strip())
have = set(open(out).read().split())
new = set()
for line in open(log, errors="replace"):
    try: e = json.loads(line)
    except ValueError: continue
    m = re.fullmatch(r"/omarchy-mirror/[^/]+/([^/]+\.pkg\.tar\.(?:zst|xz))", e.get("request", {}).get("uri", ""))
    if m and e.get("status") == 404 and m.group(1) in byfile and byfile[m.group(1)] not in have:
        new.add(byfile[m.group(1)])
if new:
    with open(out, "a") as f: f.writelines(n + "\n" for n in sorted(new))
    print("learned from 404s:", " ".join(sorted(new)))
PY
fi
if [[ -n $unchanged && ! $learned -nt $M/.done ]]; then log "upstream unchanged, nothing new to convert"; exit 0; fi
got=0; failed=0
while read -r name file; do
  if [[ -z ${ALL:-} ]] && ! grep -qxF "$name" "$WANT" "$learned"; then continue; fi
  for f in "$file" "$file.sig"; do
    [[ -s $M/$f ]] && continue
    if curl -sSf -o "$M/$f.part" "$UP/$f"; then mv "$M/$f.part" "$M/$f"; got=$((got+1)); else rm -f "$M/$f.part"; failed=$((failed+1)); log "download failed: $f"; fi
  done
done < "$list"
log "downloaded $got files ($failed failed)"

# 3. convert and publish; the served database is replaced last
"$B" repo-update --key "$KEY" --gpg-keyring "$KR" --db-in "$cur" --jobs "$JOBS" "$M" omarchy
touch "$M/.done"
log "published: $(bsdtar -xOf "$M/omarchy.db" '*/desc' | grep -c '^btrpkg.id=') of $(wc -l < "$list") packages with btrpkg data"
