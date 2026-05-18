#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-elf-eh-frame-gc.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

cat >"$workdir/eh-frame-gc.s" <<'EOF'
.text
.globl foo
.type foo,@function
foo:
  .cfi_startproc
  .cfi_personality 155, DW.ref.__gxx_personality_v0
  .cfi_endproc

.section .test_personality_section,"a",@progbits
DW.ref.__gxx_personality_v0:
  .quad 0
EOF

clang -target x86_64-unknown-linux-gnu -c "$workdir/eh-frame-gc.s" \
  -o "$workdir/eh-frame-gc.o"
./ld.lld -m elf_x86_64 -shared --gc-sections "$workdir/eh-frame-gc.o" \
  -o "$workdir/eh-frame-gc.so"

if command -v readelf >/dev/null 2>&1; then
  readelf -SW "$workdir/eh-frame-gc.so" | grep -q '.test_personality_section'
else
  echo "eh-frame-gc skipped"
fi
