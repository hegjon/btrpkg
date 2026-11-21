#!/bin/bash
# HTTP/1.1 vs HTTP/2 vs HTTP/3 for btrpkg and for pacman-style downloads, on a lab machine.
#   root@host: bash proto-bench.sh OUTDIR IDS PKGS [ROUNDS]
# IDS: lines NAME=IDHEX (btrpkg pull-ids arguments); PKGS: lines of .pkg.tar.zst file names.
# Server: the desktop's test repo, plain http on :1764 (h1, h2c) and https on :1763 (h1, h2, h3;
# CA in /etc/btrpkg/repo-ca.crt). Variants: http-h1 http-h2c https-h1 https-h2 https-h3.
# Workloads per variant and round (order rotates):
#   btr-reuse  btrpkg pull-ids --installed / (chunks of unchanged files come from the installed
#              versions, the rest is downloaded: a real upgrade)
#   btr-full   btrpkg pull-ids without reuse (every chunk downloaded)
#   pkg        curl --parallel --parallel-max 5 of the .pkg.tar.zst files (pacman's
#              ParallelDownloads = 5; pacman uses libcurl the same way)
# Records wall, user and sys seconds, bytes, requests. Downloads go to /var/tmp/protobench
# (deleted after every run); page cache dropped before every run.
set -uo pipefail
OUT=$1; IDS=$2; PKGS=$3; ROUNDS=${4:-3}
CA=/etc/btrpkg/repo-ca.crt; H=192.168.2.2; W=/var/tmp/protobench
mkdir -p $OUT; exec > >(tee -a $OUT/log) 2>&1
mapfile -t ID < $IDS; mapfile -t PK < $PKGS
echo "host $(hostname) $(ip route get $H | grep -o 'dev [^ ]*') rtt $(ping -c 5 -q $H | tail -1 | cut -d= -f2) qdisc: $(tc qdisc show dev $(ip route get $H | grep -o 'dev [^ ]*' | cut -d' ' -f2) | head -1)"
url() { case $1 in http-*) echo "http://$H:1764/omarchy/x86_64";; *) echo "https://$H:1763/omarchy/x86_64";; esac; }
ver() { case $1 in *-h1) echo h1;; *-h2c|*-h2) echo h2;; *-h3) echo h3;; esac; }
cflag() { case $1 in http-h1|https-h1) echo --http1.1;; http-h2c) echo --http2-prior-knowledge;; https-h2) echo --http2;; https-h3) echo --http3-only;; esac; }
run() { # $1 variant $2 workload $3 round
  rm -rf $W; mkdir -p $W; sync; echo 3 > /proc/sys/vm/drop_caches; sleep 1
  local u; u=$(url $1); local log=$OUT/$1-$2-$3.out
  TIMEFORMAT='%R %U %S'
  case $2 in
    btr-reuse) { time btrpkg --http $(ver $1) --cacert $CA pull-ids --installed / $u $W "${ID[@]}" > $log 2>&1; } 2> $OUT/t ;;
    btr-full)  { time btrpkg --http $(ver $1) --cacert $CA pull-ids $u $W "${ID[@]}" > $log 2>&1; } 2> $OUT/t ;;
    pkg) { time curl -s -f --cacert $CA $(cflag $1) --parallel --parallel-max 5 -w '%{size_download} %{http_version}\n' \
             $(for p in "${PK[@]}"; do echo "-o $W/$p $u/$p"; done) > $log 2>&1; } 2> $OUT/t ;;
  esac
  local ok=$?
  local bytes req
  if [[ $2 == pkg ]]; then
    bytes=$(awk '{s+=$1} END{print s+0}' $log); req=${#PK[@]}
  else # download NAME index_bytes fetched_chunks fetched_bytes ...
    bytes=$(awk -F'\t' '$1=="download"{s+=$3+$5} END{print s+0}' $log)
    req=$(awk -F'\t' '$1=="download"{s+=$4+2} END{print s+0}' $log)
  fi
  read -r real user sys < $OUT/t
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' $1 $2 $3 $ok $real $user $sys $bytes $req | tee -a $OUT/results.tsv
  rm -rf $W
}
printf 'variant\tworkload\tround\texit\treal\tuser\tsys\tbytes\trequests\n' | tee $OUT/results.tsv
V=(http-h1 http-h2c https-h1 https-h2 https-h3)
for r in $(seq 1 $ROUNDS); do
  for k in $(seq 0 4); do v=${V[$(( (k + r - 1) % 5 ))]}
    for w in btr-reuse btr-full pkg; do run $v $w $r; done
  done
done
