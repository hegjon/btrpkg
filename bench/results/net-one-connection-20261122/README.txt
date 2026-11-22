One connection for all of a run's downloads (net.rs: one long-lived curl multi handle per thread;
indexes and signatures requested all at once: store::fetch_indexes, used by fetch-ids, pull-ids and
upgrade). bench/net-latency-bench.sh: client in a container with netem delay on its eth0, against
the mirror's caddy (https://192.168.2.2:1763, HTTP/2, h1 with --http h1); connections counted from
caddy's access log. Packages: 100 spread over Arch extra (8.0 MB of indexes and signatures).

                                          before (r72+level 12)       after
fetch-ids --index-only, 100 pkgs, RTT 0   1.25 s, 200 connections     0.05 s, 1 connection
  RTT 50 ms, HTTP/2                       19.5 s, 200 connections     0.36 s, 1 connection
  RTT 100 ms, HTTP/2                      37.4 s, 200 connections     (1.66 s before the fsync change)
  RTT 50 ms, HTTP/1.1                     19.2 s, 200 connections     0.88 s, 8 connections
fetch-ids, 10 packages (43.6 MB), RTT 50  2.86 s, 21 connections      0.97 s, 1 connection

(The RTT 0 floor was 200 fsyncs: index and signature files in the download directory are now
written without fsync, like the btrparts; they are checked against their ID before any use.)

p14s with the new binary as BtrPkgHelper: pacman-robust 12/12, pacman-gaps 16/16 (omarchy-local-ai),
plan-count 1/1/1.
