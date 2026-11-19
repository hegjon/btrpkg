#!/bin/bash
# Size of .pkg.btr wrapped in an outer zstd (.pkg.btr.zst), per package.
# usage: zstd-outer.sh DIR "zstd options"
d=$1; opt=$2; S=$HOME/.hermes/cache/scratch/zz/$(basename $d)
mkdir -p $S; rm -f $S/*
printf '%s\0' $d/*.btrpkg | xargs -0 -P 32 -I{} sh -c 'zstd -q '"$opt"' -T1 -c "$1" > "$2/$(basename "$1").zst"' _ {} $S
echo "$d [$opt]: inner $(cat $d/*.btrpkg | wc -c)  outer $(cat $S/*.zst | wc -c)"
