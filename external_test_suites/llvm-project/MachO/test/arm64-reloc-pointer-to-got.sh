#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-macho-pointer-got.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

cat >"$workdir/pointer-to-got.s" <<'EOF'
.globl _probe, _foo
.p2align 2
_probe:
  adrp x8, _foo@GOTPAGE
  ldr  x8, [x8, _foo@GOTPAGEOFF]
  ret

.p2align 2
_foo:
  ret

.data
.globl _got_delta
_got_delta:
  .long _foo@GOT - .
EOF

clang -target arm64-apple-macos13 -c "$workdir/pointer-to-got.s" \
  -o "$workdir/pointer-to-got.o"
./ld64 -dylib -arch arm64 -platform_version macos 13.0 13.0 \
  -o "$workdir/libpointer-to-got.dylib" "$workdir/pointer-to-got.o"

nm "$workdir/libpointer-to-got.dylib" | grep -q ' _foo$'
nm "$workdir/libpointer-to-got.dylib" | grep -q ' _got_delta$'
