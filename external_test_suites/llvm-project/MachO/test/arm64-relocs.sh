#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-macho-relocs.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

cat >"$workdir/arm64-relocs.s" <<'EOF'
.text
.globl _foo, _bar
.p2align 2
_foo:
  bl _bar
  adrp x2, _baz@PAGE
  ldr x2, [x2, _baz@PAGEOFF]
  ret

.p2align 2
_bar:
  ret

.data
.globl _baz
_baz:
  .quad 42

.subsections_via_symbols
EOF

clang -target arm64-apple-macos13 -c "$workdir/arm64-relocs.s" -o "$workdir/arm64-relocs.o"
./ld64 -dylib -arch arm64 -platform_version macos 13.0 13.0 \
  -o "$workdir/libarm64-relocs.dylib" "$workdir/arm64-relocs.o"

nm "$workdir/libarm64-relocs.dylib" | grep -q ' _foo$'
nm "$workdir/libarm64-relocs.dylib" | grep -q ' _baz$'
