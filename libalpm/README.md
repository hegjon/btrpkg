# libalpm patches

pacman branch `btrpkg`, based on `54d94116` (Arch's pacman 7.1.0.r9 build), as `git format-patch`
files. Apply with `git am` on that commit of https://gitlab.archlinux.org/pacman/pacman.

1. `libalpm: read btrpkg indexes and their signatures`: `lib/libalpm/btrpkg.c` parses and checks
   a version 1 index with the same checks and verdicts as `src/format.rs`, and verifies the
   signify signature (Ed25519 through OpenSSL). `test/btrpkg` (`meson test -C build btrpkg`):
   73 subtests on committed fixtures (a real index and signature, every bit flip and cut, crafted
   indexes from `bench/craft_indexes.py`).

Cross-checks run against this repo's reader (see PLAN, iteration 2 milestone 2): `btrpkg dump`
and `btrdump` print identical text for all 384 real indexes (51 in the omarchy test repo, 18 in
btrtest, the 315 set), the 315 transports' chunks pass `btr_check_chunk`, and 9,000 indexes from
`bench/fuzz_entries.py` get the same verdict and output, with no ASan/UBSan reports.
