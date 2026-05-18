#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-elf-gc.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

cat >"$workdir/gc-sections.s" <<'EOF'
.globl _start, live, dead

.section .text._start,"ax",@progbits
_start:
  call live
  ret

.section .text.live,"ax",@progbits
live:
  ret

.section .text.dead,"ax",@progbits
dead:
  ret
EOF

clang -target x86_64-unknown-linux-gnu -c "$workdir/gc-sections.s" \
  -o "$workdir/gc-sections.o"
./ld.lld -m elf_x86_64 --gc-sections "$workdir/gc-sections.o" \
  -o "$workdir/gc-sections.out"

nm "$workdir/gc-sections.out" | grep -q ' live$'
if nm "$workdir/gc-sections.out" | grep -q ' dead$'; then
  echo "dead symbol unexpectedly survived --gc-sections"
  exit 1
fi
