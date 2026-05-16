# Goal: Needed Linker Script Support

This note records the linker-script support that feels worth pursuing for sld if the goal is:

- practical compatibility for ordinary builds that use linker scripts;
- honest and reliable support for the subset sld already exposes;
- avoiding a drift toward full GNU ld linker-script parity or support for highly specialized,
  deeply customized script ecosystems.

## Guiding Principle

sld should aim to support the small, common, build-unblocking subset of linker scripts well.
It does not need to become a general-purpose implementation of every advanced GNU ld script feature.

## Must-Do Areas

### 1. Finish the partial semantics of already-supported core features

These are already exposed in `LINKER_SCRIPT_SUPPORT.md`, so the implementation should become
solid enough that the support matrix reads as a dependable contract rather than a set of caveats.

- Top-level symbol assignment.
- Section-local symbol assignment.
- `PROVIDE(...)` and `PROVIDE_HIDDEN(...)`.
- `ASSERT(...)`.
- Expression handling for symbol references and the location counter where those already appear in
  the supported surface.

This does not imply support for every arbitrary linker-script expression in every position. The goal
is to make the currently advertised subset behave predictably and match the expectations of normal
scripts that use these constructs.

### 2. Support script inclusion when it blocks otherwise ordinary scripts

`INCLUDE(file)` feels worth supporting.

It is a practical composition feature, not an inherently advanced layout feature, and real scripts
are often split across files. Supporting it improves compatibility without pulling sld toward the
more complex corners of linker-script semantics.

### 3. Support search-path directives only if they are needed by common real-world scripts

`SEARCH_DIR(path)` is the other feature that plausibly belongs in the practical compatibility set.

It should be treated as demand-driven rather than automatic roadmap expansion:

- if ordinary libc, toolchain, or package-provided linker scripts rely on it in ways that currently
  block real builds, it is worth implementing;
- otherwise, it can stay deferred.

### 4. Keep the current structural subset robust

The existing core support is the right general shape for sld and should remain reliable:

- `SECTIONS { ... }`;
- output section definitions;
- input section matchers and globbing;
- `KEEP(...)`;
- simple location-counter setting and alignment;
- per-section `ALIGN(...)`;
- practical expression helpers already in use, such as `SIZEOF`, `ADDR`, `ALIGNOF`, and
  `SEGMENT_START(...)`.

These are the pieces that make linker-script support useful without committing the project to the
full complexity of linker-script compatibility work.

## Not Must-Do

The following do not feel required for sld's intended scope if the project does not want to support
really complex linker scripts:

- `PHDRS`;
- `OVERLAY`;
- `AT(addr)`;
- `>region` and `AT>region`;
- richer `MEMORY` placement semantics;
- `FILL(value)` and `=fillexp`;
- output data emitters such as `BYTE`, `SHORT`, `LONG`, and `QUAD`;
- `SUBALIGN`;
- `ONLY_IF_RO` / `ONLY_IF_RW`;
- `SORT_BY_NAME`, `SORT_BY_ALIGNMENT`, and `SORT_BY_INIT_PRIORITY`;
- `EXCLUDE_FILE`;
- ternary expressions;
- `DEFINED(sym)`;
- `SIZEOF_HEADERS`;
- highly specialized top-level directives such as `OUTPUT_ARCH`, `OUTPUT`, `TARGET`, `STARTUP`,
  and `NOCROSSREFS`.

These features mostly serve elaborate custom layout control, deeply specialized embedded/kernel
scripts, or broad GNU ld completeness rather than the narrow compatibility goal above.

## Documentation Implication

`LINKER_SCRIPT_SUPPORT.md` should read as a practical support matrix, not as a promise that sld is
working toward full linker-script completeness.

In particular, the Linux-kernel-oriented wishlist should be treated carefully. Linking the kernel
is a good example of a very complex linker-script target, so presenting that section as a natural
roadmap risks overstating the intended scope unless the project explicitly chooses that goal.
