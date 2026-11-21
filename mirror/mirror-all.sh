#!/bin/bash
# Run repo-mirror.sh for every repository in repos.conf, one after the other (btrpkg-mirror.timer).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
rc=0
while read -r repo up dir keyring want parts; do
  [[ -z $repo || $repo == \#* ]] && continue
  REPO=$repo UP=$up M=$HOME/Work/btrpkg-repo/$dir KEYRING=$keyring WANT=$here/$want PARTS=${parts:-../btrparts} "$here/repo-mirror.sh" || { echo "$repo: failed"; rc=1; }
done < "$here/repos.conf"
exit $rc
