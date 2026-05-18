#!/usr/bin/env bash
set -euo pipefail

workdir="$(mktemp -d "${TMPDIR:-/tmp}/sld-llvm-elf-archive.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

if [ "$(uname -s)" = "Darwin" ] && ! command -v llvm-ar >/dev/null 2>&1; then
  echo "archive-activation skipped"
  exit 0
fi

cat >"$workdir/main.s" <<'EOF'
.globl _start
_start:
  call foo
  ret
EOF

cat >"$workdir/foo.s" <<'EOF'
.globl foo
foo:
  ret
EOF

clang -target x86_64-unknown-linux-gnu -c "$workdir/main.s" -o "$workdir/main.o"
clang -target x86_64-unknown-linux-gnu -c "$workdir/foo.s" -o "$workdir/foo.o"
if command -v llvm-ar >/dev/null 2>&1; then
  llvm-ar rcs "$workdir/libfoo.a" "$workdir/foo.o"
else
  ar rcs "$workdir/libfoo.a" "$workdir/foo.o"
fi

./ld.lld -m elf_x86_64 "$workdir/main.o" "$workdir/libfoo.a" \
  -o "$workdir/archive.out"

nm "$workdir/archive.out" | grep -q ' T foo$'
