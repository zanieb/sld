#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-macho-got-load.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

cat >"$workdir/main.s" <<'EOF'
.globl _foo, _bar, _probe
.p2align 2
_probe:
  adrp x8, _foo@GOTPAGE
  ldr  x8, [x8, _foo@GOTPAGEOFF]
  adrp x8, _bar@GOTPAGE
  ldr  x8, [x8, _bar@GOTPAGEOFF]
  ret
EOF

cat >"$workdir/foobar.s" <<'EOF'
.globl _foo, _bar
_foo:
  ret
_bar:
  ret
EOF

clang -target arm64-apple-macos13 -c "$workdir/main.s" -o "$workdir/main.o"
clang -target arm64-apple-macos13 -c "$workdir/foobar.s" -o "$workdir/foobar.o"
./ld64 -dylib -arch arm64 -platform_version macos 13.0 13.0 \
  -install_name @rpath/libfoobar.dylib \
  -o "$workdir/libfoobar.dylib" "$workdir/foobar.o"
./ld64 -dylib -arch arm64 -platform_version macos 13.0 13.0 \
  -o "$workdir/libgot-load.dylib" "$workdir/main.o" "$workdir/libfoobar.dylib"

nm "$workdir/libgot-load.dylib" | grep -q ' _probe$'
otool -L "$workdir/libgot-load.dylib" | grep -q 'libfoobar.dylib'
