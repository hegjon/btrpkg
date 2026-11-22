#!/bin/bash
# Planning (fetch-ids --index-only) and download (fetch-ids) time over HTTPS with added latency:
# the client runs in a container whose eth0 delays every packet by DELAY ms (netem), against the
# mirror's caddy on https://192.168.2.2:1763 (HTTP/2, internal CA). Connections are counted from
# caddy's access log (distinct client ports during the run).
#
#   net-latency-bench.sh BTRPKG N DELAY_MS [full N2]
set -euo pipefail
B=$(realpath "$1") N=$2 D=$3 MODE=${4:-plan} N2=${5:-10}
R=~/Work/btrpkg-repo REL=arch-mirror/extra/os/x86_64
CA=~/.local/share/caddy/pki/authorities/local/root.crt
W=$(mktemp -d "${TMPDIR:-/tmp}/nlb.XXXX")
/usr/bin/python3 - "$R/$REL/extra.db" "$N" > "$W/args" <<'EOF'
import sys, tarfile
t = tarfile.open(sys.argv[1]); out = []
for m in t:
    if not m.name.endswith('/desc'): continue
    d = t.extractfile(m).read().decode()
    f = dict()
    for blk in d.split('\n\n'):
        ls = blk.strip().split('\n')
        if ls and ls[0].startswith('%'): f[ls[0]] = ls[1:]
    kv = dict(x.split('=', 1) for x in f.get('%DATA%', []) if '=' in x)
    if 'btrpkg.id' in kv: out.append(f"{kv['btrpkg.index'][:-8]}={kv['btrpkg.id']}")
out.sort()
n = int(sys.argv[2]); step = max(1, len(out) // n)
print(' '.join(out[::step][:n]))
EOF
want=--index-only
[ "$MODE" = full ] && { want=""; tr ' ' '\n' < "$W/args" | head -"$N2" | tr '\n' ' ' > "$W/args2"; mv "$W/args2" "$W/args"; }
start=$(date +%s.%N)
docker run --rm --cap-add NET_ADMIN --add-host=mirror:192.168.2.2 -v "$B:/b:ro" -v "$CA:/ca.crt:ro" -v "$W:/w" exp-btrfs sh -c "
  [ $D -gt 0 ] && tc qdisc add dev eth0 root netem delay ${D}ms
  ping -c1 -q 192.168.2.2 | tail -1
  mkdir -p /w/d; s=\$(date +%s%N)
  BTRPKG_NETSTATS=1 /b fetch-ids --cacert /ca.crt ${EXTRA:-} $want --parts ../../../btrparts --server https://192.168.2.2:1763/$REL /w/d $(cat "$W/args") > /w/out 2> /w/err; rc=\$?
  e=\$(date +%s%N); echo \"rc \$rc  time \$(( (e - s) / 1000000 )) ms\"
  echo \"files \$(find /w/d -type f | wc -l)  bytes \$(du -sb /w/d | cut -f1)\"; chown -R 1000:1000 /w
"
sleep 1
/usr/bin/python3 - "$R/access.log" "$start" <<'EOF'
import sys, json
t0 = float(sys.argv[2]); ports = set(); n = 0
for l in open(sys.argv[1]):
    try: j = json.loads(l)
    except Exception: continue
    if j.get('ts', 0) < t0: continue
    rq = j.get('request', {})
    if not rq.get('headers', {}).get('User-Agent', [''])[0].startswith('btrpkg'): continue
    n += 1; ports.add(rq.get('remote_port')); proto = rq.get('proto')
print(f"requests {n}, connections {len(ports)}, proto {proto if n else '-'}")
EOF
tail -3 "$W/err"; rm -rf "$W"
