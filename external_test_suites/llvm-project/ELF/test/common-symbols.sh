#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-elf-common.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

cat >"$workdir/common.s" <<'EOF'
.globl _start
_start:
  ret

.comm sym1,4,4
.comm sym2,8,4
.comm sym3,2,2
.comm sym4,4,2
EOF

clang -target x86_64-unknown-linux-gnu -c "$workdir/common.s" -o "$workdir/common.o"
./ld.lld -m elf_x86_64 "$workdir/common.o" -o "$workdir/common.out"

nm "$workdir/common.out" | grep -q ' B sym1$'
nm "$workdir/common.out" | grep -q ' B sym2$'
nm "$workdir/common.out" | grep -q ' B sym3$'
nm "$workdir/common.out" | grep -q ' B sym4$'
