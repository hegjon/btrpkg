#!/bin/bash
# Build libzstd 1.5.7's decoder (no asm, like the kernel) as userspace with the kernel's compiler
# flags (taken from a kbuild V=1 module build of linux-omarchy 7.2.15), in variants that drop one
# group of flags each, linked into zbench. Return thunk: the SRSO "safe RET" thunk the kernel uses
# on Zen 1-4 (every return goes through call+lea+ret), or a plain ret.
set -euo pipefail
cd "$(dirname "$0")"
Z=${Z:?path to zstd-1.5.7/lib}
SRC="$Z/common/entropy_common.c $Z/common/error_private.c $Z/common/fse_decompress.c $Z/common/zstd_common.c $Z/common/xxhash.c $Z/decompress/huf_decompress.c $Z/decompress/zstd_ddict.c $Z/decompress/zstd_decompress.c $Z/decompress/zstd_decompress_block.c"
cat > thunks.S <<'EOF'
	.text
#ifdef SRSO
	.globl __x86_return_thunk
__x86_return_thunk:
	call 1f
	ud2
1:	lea 8(%rsp), %rsp
	ret
	int3
#else
	.globl __x86_return_thunk
__x86_return_thunk:
	ret
	int3
#endif
.macro thunk reg
	.globl __x86_indirect_thunk_\reg
__x86_indirect_thunk_\reg:
	call 2f
1:	pause
	lfence
	jmp 1b
2:	mov %\reg, (%rsp)
	ret
	int3
.endm
	thunk rax; thunk rbx; thunk rcx; thunk rdx; thunk rsi; thunk rdi; thunk rbp
	thunk r8; thunk r9; thunk r10; thunk r11; thunk r12; thunk r13; thunk r14; thunk r15
	.section .note.GNU-stack,"",@progbits
EOF
BASE="-O3 -DZSTD_DISABLE_ASM -march=x86-64 -mtune=generic -mno-sse -mno-sse2 -mno-mmx -mno-avx -mno-3dnow -mno-sse4a -mno-80387 -mno-fp-ret-in-387 -mno-red-zone -mpreferred-stack-boundary=3 -mskip-rax-setup -fno-strict-aliasing -fno-strict-overflow -fno-delete-null-pointer-checks -fno-common -funsigned-char -fconserve-stack -fno-asynchronous-unwind-tables -fno-stack-check -fno-stack-clash-protection -fno-PIE -fno-pic"
LAYOUT="-fno-jump-tables -falign-jumps=1 -falign-loops=1 -fmin-function-alignment=16 -fpatchable-function-entry=16,16 -fcf-protection=branch"
UBSAN="-fsanitize=bounds-strict -fsanitize=shift"
INIT="-ftrivial-auto-var-init=zero -fzero-init-padding-bits=all -fstrict-flex-arrays=3"
SP="-fstack-protector-strong"
MIT="-mfunction-return=thunk-extern -mharden-sls=all -mindirect-branch=thunk-extern -mindirect-branch-register -mindirect-branch-cs-prefix"
build() { # name thunk-flavour flags...
  local n=$1 t=$2; shift 2; rm -rf "o-$n"; mkdir -p "o-$n"
  for s in $SRC; do gcc -c "$@" -I"$Z" -I"$Z/common" "$s" -o "o-$n/$(basename "$s" .c).o"; done
  gcc -c $t thunks.S -o "o-$n/thunks.o"
  gcc -O2 -no-pie -I"$Z" zbench.c "o-$n"/*.o -o "zb-$n" $([[ "$*" == *sanitize* ]] && echo -lubsan)
}
build k-full   -DSRSO $BASE $LAYOUT $UBSAN $INIT $SP $MIT
build k-ret    ""     $BASE $LAYOUT $UBSAN $INIT $SP $MIT
build k-noubsan -DSRSO $BASE $LAYOUT $INIT $SP $MIT
build k-noinit -DSRSO $BASE $LAYOUT $UBSAN $SP $MIT
build k-nosp   -DSRSO $BASE $LAYOUT $UBSAN $INIT $MIT
build k-nolayout -DSRSO $BASE $UBSAN $INIT $SP $MIT
build k-o3base ""     $BASE
build k-o2base ""     ${BASE/-O3/-O2}
