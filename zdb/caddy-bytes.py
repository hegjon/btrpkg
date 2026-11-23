#!/usr/bin/env python3
"""Bytes caddy sent to one client for /zdb/ since a line number of access.log.
caddy-bytes.py LOG START_LINE IP  ->  requests, bytes (response bodies), status counts"""
import collections, json, sys
log, start, ip = sys.argv[1], int(sys.argv[2]), sys.argv[3]
n = b = 0
st = collections.Counter()
with open(log) as f:
    for i, line in enumerate(f, 1):
        if i <= start:
            continue
        try:
            e = json.loads(line)
        except ValueError:
            continue
        r = e.get("request", {})
        if r.get("remote_ip") != ip or not r.get("uri", "").startswith("/zdb/"):
            continue
        n += 1
        b += e.get("size", 0)
        st[e.get("status")] += 1
print(f"requests {n} bytes {b} status {dict(st)}")
