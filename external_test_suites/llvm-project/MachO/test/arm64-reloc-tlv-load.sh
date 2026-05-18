#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-macho-tlv-load.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT
sdk_root="$(xcrun --sdk macosx --show-sdk-path)"

cat >"$workdir/main.s" <<'EOF'
.globl _foo, _bar, _probe
.p2align 2
_probe:
  adrp x8, _foo@TLVPPAGE
  ldr  x8, [x8, _foo@TLVPPAGEOFF]
  adrp x8, _bar@TLVPPAGE
  ldr  x8, [x8, _bar@TLVPPAGEOFF]
  ret
EOF

cat >"$workdir/foobar.s" <<'EOF'
.globl _foo, _bar

.section  __DATA,__thread_data,thread_local_regular
_foo$tlv$init:
  .long 123
_bar$tlv$init:
  .long 456

.section  __DATA,__thread_vars,thread_local_variables
_foo:
  .quad __tlv_bootstrap
  .quad 0
  .quad _foo$tlv$init
_bar:
  .quad __tlv_bootstrap
  .quad 0
  .quad _bar$tlv$init
EOF

clang -target arm64-apple-macos13 -c "$workdir/main.s" -o "$workdir/main.o"
clang -target arm64-apple-macos13 -c "$workdir/foobar.s" -o "$workdir/foobar.o"
./ld64 -dylib -arch arm64 -platform_version macos 13.0 13.0 \
  -syslibroot "$sdk_root" -lSystem \
  -install_name @rpath/libtlv-exports.dylib \
  -o "$workdir/libtlv-exports.dylib" "$workdir/foobar.o"
./ld64 -dylib -arch arm64 -platform_version macos 13.0 13.0 \
  -syslibroot "$sdk_root" -lSystem \
  -o "$workdir/libtlv-load.dylib" "$workdir/main.o" "$workdir/libtlv-exports.dylib"

nm "$workdir/libtlv-load.dylib" | grep -q ' _probe$'
otool -L "$workdir/libtlv-load.dylib" | grep -q 'libtlv-exports.dylib'
