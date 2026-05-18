#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-macho-subtractor.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

cat >"$workdir/reloc-subtractor.s" <<'EOF'
.text
.globl _entry
.p2align 2
_entry:
  ret

.section __DATA,__data
.globl _subtractor_value, _lhs, _rhs
_subtractor_value:
  .quad _lhs - _rhs + 4
_lhs:
  .quad 1
_rhs:
  .quad 2

.subsections_via_symbols
EOF

clang -target arm64-apple-macos13 -c "$workdir/reloc-subtractor.s" -o "$workdir/reloc-subtractor.o"
./ld64 -dylib -arch arm64 -platform_version macos 13.0 13.0 \
  -o "$workdir/libreloc-subtractor.dylib" "$workdir/reloc-subtractor.o"

nm "$workdir/libreloc-subtractor.dylib" | grep -q ' _subtractor_value$'
nm "$workdir/libreloc-subtractor.dylib" | grep -q ' _lhs$'
