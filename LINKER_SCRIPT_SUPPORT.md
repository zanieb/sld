# Linker Script Support

This page documents which linker script features sld supports, which are partially implemented,
and which are planned for the future. Each feature is marked with one of four statuses: `✅`
(supported), `🧪` (partial), `📅` (planned), or `❌` (not planned). A dedicated section at the
end lists the features required to link the Linux kernel.

## Top-Level Commands

| Feature | Status | Notes |
|---------|--------|-------|
| `GROUP(files...)` | ✅ | |
| `INPUT(files...)` | ✅ | |
| `AS_NEEDED(files...)` | ✅ | |
| `INCLUDE(file)` | 📅 | |
| `OUTPUT_FORMAT(...)` | ✅ | Parsed and ignored |
| `OUTPUT_ARCH(arch)` | ❌ | |
| `OUTPUT(filename)` | ❌ | |
| `SECTIONS { ... }` | ✅ | |
| `ENTRY(symbol)` | ✅ | |
| `VERSION { ... }` | ✅ | |
| `PROVIDE(sym = expr)` | 🧪 | Top-level values currently support absolute numeric values and simple symbol expressions with optional `+`/`-` numeric offsets |
| `PROVIDE_HIDDEN(sym = expr)` | 🧪 | Same expression limits as top-level `PROVIDE(...)` |
| `ASSERT(expr, "msg")` | 🧪 | Assertions are evaluated after layout, but symbol references and full location-counter semantics are not yet implemented |
| `MEMORY { ... }` | 🧪 | Region parsing supported; attribute flags and `>region` placement not yet implemented |
| `REGION_ALIAS(alias, region)` | ❌ | |
| `SEARCH_DIR(path)` | ❌ | |
| `STARTUP(filename)` | ❌ | |
| `TARGET(bfdname)` | ❌ | |
| `NOCROSSREFS(sections...)` | ❌ | |
| `INSERT [AFTER\|BEFORE] section` | ❌ | |
| Top-level symbol assignment (`sym = expr`) | 🧪 | Supports numeric values, symbol redirects with optional `+`/`-` numeric offsets, and `SEGMENT_START(...)`; arbitrary expressions are not yet supported |
| Compound assignment operators (`+=`, `-=`, etc.) | ❌ | |

## SECTIONS Block

| Feature | Status | Notes |
|---------|--------|-------|
| Output section definitions (`name : { ... }`) | ✅ | |
| Input section matchers (`*(pattern)`, `file(pattern)`) | ✅ | |
| Glob patterns in section and file names | ✅ | |
| `KEEP(...)` to prevent garbage collection | ✅ | |
| `PROVIDE(sym = expr)` inside sections | 🧪 | The symbol is placed at the current section boundary; the right-hand-side expression is not yet evaluated |
| `PROVIDE_HIDDEN(sym = expr)` inside sections | 🧪 | Same placement behavior as section-local `PROVIDE(...)`, with hidden visibility |
| Symbol assignment inside sections (`sym = expr`) | 🧪 | Supports `sym = .` plus `SEGMENT_START(...)`; other right-hand-side expressions are not yet honored |
| Location counter assignment (`. = expr`) | 🧪 | Hex address literals (e.g. `. = 0x1000`) supported between output sections only; not inside section contents |
| `ALIGN(n)` on the location counter (`. = ALIGN(n)`) | ✅ | |
| Per-section `ALIGN(n)` specifier | ✅ | |
| `ASSERT(expr, "msg")` inside `SECTIONS` | 🧪 | Same evaluator limits as top-level `ASSERT(...)` |
| `OVERLAY { ... }` | ❌ | |
| Output section type specifiers (`(NOLOAD)`, `(COPY)`, etc.) | 📅 | |
| `FILL(value)` and `=fillexp` | 📅 | |
| `AT(addr)` load-address specifier on output sections | 📅 | |
| Numeric address between section name and `:` (e.g. `name 0 : { ... }`) | 🧪 | Only numeric literals are currently supported |
| `SORT_BY_NAME(...)`, `SORT_BY_ALIGNMENT(...)`, `SORT_BY_INIT_PRIORITY(...)` | 📅 | |
| `EXCLUDE_FILE(...)` inside input section matchers | 📅 | |
| `BYTE(expr)`, `SHORT(expr)`, `LONG(expr)`, `QUAD(expr)` output data | ❌ | |
| `SUBALIGN(n)` forced input alignment | ❌ | |
| `ONLY_IF_RO` / `ONLY_IF_RW` output section constraints | ❌ | |

## Expressions and Functions

| Feature | Status | Notes |
|---------|--------|-------|
| Arithmetic operators: `+`, `-`, `*`, `/` | ✅ | |
| Comparison operators: `<`, `>`, `<=`, `>=`, `==`, `!=` | ✅ | |
| Bitwise operators: `&`, `\|`, `^`, `~`, `<<`, `>>` | ✅ | |
| Logical operators: `&&`, `\|\|` | ✅ | |
| Unary operators: `-`, `!`, `~` | ✅ | |
| Numeric literals: decimal and hexadecimal | ✅ | |
| Numeric literal K/M suffixes (e.g. `64K`, `2M`) | ✅ | |
| Symbol references and location counter (`.`) | 🧪 | Parsed generally, but assertion evaluation skips symbol references and treats `.` as `0` |
| Parenthesised sub-expressions | ✅ | |
| `SIZEOF(section)` | ✅ | |
| `ALIGNOF(section)` | ✅ | |
| `ADDR(section)` | ✅ | |
| `LOADADDR(section)` | 🧪 | Implemented as alias for `ADDR` (returns VMA); full LMA requires `AT(addr)` support |
| `ALIGN(expr)` | 🧪 | Standalone evaluation is supported; assertion evaluation does not yet model a non-zero current location counter |
| `LENGTH(region)` | ✅ | |
| `ORIGIN(region)` | ✅ | |
| `MIN(a, b)` | ✅ | |
| `MAX(a, b)` | ✅ | |
| Ternary operator (`condition ? a : b`) | 📅 | |
| `DEFINED(sym)` | 📅 | |
| `SIZEOF_HEADERS` | 📅 | |
| `SEGMENT_START(segment, default)` | ✅ | Supports `"text"`, `"data"`, `"bss"`, `"rodata"`; `"text"`, `"data"`, and `"bss"` use `-Ttext`/`-Tdata`/`-Tbss` when provided, while `"rodata"` and unknown segment names return `default` |

## MEMORY Command

The `MEMORY` command defines named memory regions with an origin address and a length. sld parses
`MEMORY` blocks including the `ORIGIN`/`org`/`o` and `LENGTH`/`len`/`l` attribute keywords and
their expressions. Attribute flags such as `(rwx)` are not yet parsed. Placement directives that
assign an output section to a named region (`>region`, `AT>region`) are not yet implemented.

| Feature | Status | Notes |
|---------|--------|-------|
| `MEMORY { ... }` block parsing | ✅ | |
| Region name | ✅ | |
| `ORIGIN`/`org`/`o` attribute | ✅ | |
| `LENGTH`/`len`/`l` attribute | ✅ | |
| Attribute flags (`(rwx)`, `(rx)`, etc.) | 📅 | |
| `>region` output section placement | 📅 | |
| `AT>region` load-region placement | 📅 | |

## Linux Kernel Requirements

The Linux kernel's build system uses a rich set of linker script features across `vmlinux.lds` and
related architecture-specific scripts. Several of these features are not yet fully supported by
sld. The table below lists each such feature along with its current status, so contributors can
see at a glance what remains before sld can link the kernel.

| Feature | Status | Notes |
|---------|--------|-------|
| `OVERLAY { ... }` sections | ❌ | |
| Output section type specifiers (`(NOLOAD)`, `(COPY)`) | 📅 | |
| `FILL(value)` and `=fillexp` | 📅 | |
| `AT(addr)` load-address specifier on output sections | 📅 | |
| `>region` and `AT>region` memory region placement | 📅 | |
| `SORT_BY_NAME(...)`, `SORT_BY_ALIGNMENT(...)`, `SORT_BY_INIT_PRIORITY(...)` | 📅 | |
| `EXCLUDE_FILE(...)` inside input section matchers | 📅 | |
| `CONSTRUCTORS` command | 📅 | |
| `PHDRS` command for explicit program header definition | 📅 | |
| Ternary operator (`condition ? a : b`) | 📅 | |
| `DEFINED(sym)` function | 📅 | |
| `SIZEOF_HEADERS` built-in symbol | 📅 | |
