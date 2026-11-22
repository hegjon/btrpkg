One connection for all of a run's downloads (net.rs: one long-lived curl multi handle per thread;
indexes and signatures requested all at once: store::fetch_indexes, used by fetch-ids, pull-ids and
upgrade). bench/net-latency-bench.sh: client in a container with netem delay on its eth0, against
the mirror's caddy (https://192.168.2.2:1763, HTTP/2, h1 with --http h1); connections counted from
caddy's access log. Packages: 100 spread over Arch extra (8.0 MB of indexes and signatures).

                                          before (r72+level 12)       after
fetch-ids --index-only, 100 pkgs, RTT 0   1.25 s, 200 connections     0.05 s, 1 connection
  RTT 50 ms, HTTP/2                       19.5 s, 200 connections     0.36 s, 1 connection
  RTT 100 ms, HTTP/2                      37.4 s, 200 connections     0.68 s, 1 connection
  RTT 50 ms, HTTP/1.1                     19.2 s, 200 connections     0.88 s, 8 connections
fetch-ids, 10 packages (43.6 MB), RTT 50  2.86 s, 21 connections      0.97 s, 1 connection

(The RTT 0 floor was 200 fsyncs: index and signature files in the download directory are now
written without fsync, like the btrparts; they are checked against their ID before any use.)

p14s with the new binary as BtrPkgHelper: pacman-robust 12/12, pacman-gaps 16/16 (omarchy-local-ai),
plan-count 1/1/1.

HTTP/3 (same bench, EXTRA="--http h3"; caddy's QUIC on udp 1763; libcurl 8.21 with ngtcp2 in the
container, 8.23 gave the same)
                                          HTTP/2                      HTTP/3
fetch-ids --index-only, 100 pkgs:
  one connection per request (r72), RTT 50  19.5 s                    12.9 s
                                    RTT 100 37.4 s                    24.4 s
  one connection (5da1439), RTT 0           0.05 s                    0.05 s
                            RTT 50          0.36 s                    0.28 s
                            RTT 100         0.69 s                    0.53 s
fetch-ids, 30 packages (198 MB), RTT 50:
  5da1439 (checks and writes on the transfer loop)  2.63 s            7.25 s
  btrpart worker thread                             2.51 s            2.43 s (2 QUIC connections)
  same, at most one connection per server           2.51 s            2.99 s
  RTT 0, worker thread                              0.31 s            0.29 s (were 0.43 / 0.45 s)
- QUIC runs in userspace: while fetch-ids checked and wrote a 16 MB batch on the transfer loop's
  thread, no packet was read or acknowledged; TCP is served by the kernel meanwhile. The curl CLI
  fetching 1000 btrparts at 64 in parallel was fine over HTTP/3 (0.67 s vs 1.01 s HTTP/2), which
  pointed at btrpkg's loop. Now get_many hands batches to a worker thread (bounded channel, 2).
- Up to 4 connections per server stay allowed: HTTP/3 opens a second one when the server's stream
  credit runs out (caddy/quic-go hands it out lazily); forcing one costs 0.5 s here. HTTP/2 stays on
  one connection at 64 in parallel (libcurl allows 100 streams per connection).
- More in flight helps at high latency: --parallel 128 / 256 took 1.63 / 1.11 s (HTTP/2, 2 / 3
  connections) and 1.51 / 1.09 s (HTTP/3). The default stays 64 (one connection).
- One big file (libreoffice-fresh, 156 MB) with the curl CLI at RTT 50: HTTP/2 110 MB/s, HTTP/3
  257 MB/s.
- p14s with the worker-thread binary: pacman-robust 12/12, pacman-gaps 16/16.
