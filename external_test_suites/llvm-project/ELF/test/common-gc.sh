#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-elf-common-gc.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

cat >"$workdir/common-gc.s" <<'EOF'
.comm bar,4,4
.comm foo,4,4

.text
.globl _start
_start:
  .quad foo
EOF

clang -target x86_64-unknown-linux-gnu -c "$workdir/common-gc.s" \
  -o "$workdir/common-gc.o"
./ld.lld -m elf_x86_64 "$workdir/common-gc.o" -o "$workdir/common-nogc.out"
./ld.lld -m elf_x86_64 --gc-sections "$workdir/common-gc.o" \
  -o "$workdir/common-gc.out"

nm "$workdir/common-nogc.out" | grep -q ' B bar$'
nm "$workdir/common-nogc.out" | grep -q ' B foo$'
nm "$workdir/common-gc.out" | grep -q ' B foo$'
if nm "$workdir/common-gc.out" | grep -q ' B bar$'; then
  echo "bar unexpectedly survived --gc-sections"
  exit 1
fi
