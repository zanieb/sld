# Incremental linking in sld

sld's incremental mode is built for iterative development: keep the output of a previous link
around, decide whether it is still usable, and do the smallest correct update that the saved state
allows. The implementation is deliberately conservative. It prefers a full relink over a risky
incremental update, but the current fast paths already cover more than "nothing changed".

This note describes the current model in the repository today. The changed-input patching details
below are primarily about the ELF path; Mach-O has incremental plumbing and smoke coverage, but it
is not the production focus of this document.

## Enabling incremental mode

There are two equivalent ways to opt in:

```sh
sld --incremental ...
```

```sh
SLD_INCREMENTAL=1 sld ...
```

`--no-incremental` overrides the environment variable. The command-line parsing and precedence are
covered in `libsld/src/args.rs`.

Incremental state lives beside the output in an `.incr` directory:

```text
target/debug/app
target/debug/app.incr/
```

The per-output state directory contains the persisted metadata, section sidecars, snapshots, and a
local `log`. sld also appends a global incremental log that can be printed with:

```sh
sld log
```

The global log location follows the platform state directory, or `SLD_STATE_DIR` when that variable
is set.

## The current fast-path ladder

sld's incremental behavior is easiest to understand as a ladder of progressively more expensive
choices.

| Situation | Result |
| --- | --- |
| Incremental mode is disabled | Normal full link |
| No prior state exists | Full relink, then write fresh incremental state |
| Link options or input ordering changed | Full relink |
| Output and tracked inputs still match | Reuse the existing output |
| An input was rewritten but its contents are unchanged | Refresh metadata and reuse the output |
| Some changed object files stay within the currently patchable subset | Patch the existing output in place |
| A change is outside the patchable subset, state is stale, or safety checks fail | Full relink |

The code path that decides between those outcomes lives in `libsld/src/incremental.rs`. Two log
messages are especially useful when reading a run:

```text
reused existing output before loading inputs
patched N changed input file(s) before loading inputs
```

Those messages mean sld took a metadata-driven fast path without first loading every input into the
ordinary full-link pipeline.

## Reusing unchanged output

The cheapest case is a no-change relink. sld reads the saved state, checks that the output and
tracked inputs still match, and returns the existing binary immediately. The implementation logs:

```text
reused existing output before loading inputs
```

That path is important for developer loops that relink aggressively even when the final linker
inputs did not actually change.

sld also handles a subtler case: a build system rewrites an object file, but the bytes are the same.
Instead of treating the fresh inode or mtime as a semantic change, sld compares against the saved
snapshot, refreshes the stored file identity, and still reuses the output. The log records:

```text
updated 1 rewritten input file before loading inputs
reused existing output before loading inputs
```

That distinction matters in real builds, where unchanged compiler outputs may be replaced on disk
even though the linked program is unchanged.

## Patching changed inputs

When an input really changed, sld can now patch some outputs in place instead of always relinking
from scratch. The rough rule is:

1. The changed bytes must be explainable by persisted incremental metadata.
2. The affected sections must still fit the recorded output layout, or fit within explicit
   incremental padding.
3. Any dependent relocation, generated-section, dynamic-relocation, or frame metadata must be
   patchable consistently.

When that succeeds, sld updates the binary, persists refreshed metadata, and logs both the number
of changed files and the number of narrowed sections:

```text
patched 1 changed input file before loading inputs
patched 1 changed input sections before loading inputs
```

The in-tree tests exercise patching for:

- changed `.data`, `.rodata`, and `.text` content,
- multiple changed inputs and multiple changed sections,
- compiler-produced replacement objects, not just byte flips,
- relocation addend changes and moved relocation targets,
- direct and dynamic relocation metadata,
- `.eh_frame` and `.eh_frame_hdr` additions and removals,
- merged-string data when the changed bytes are still representable,
- archives and thin archives when the mutation preserves the supported shape,
- restoration back to the original bytes after a prior incremental patch.

The representative test fixtures live under `sld/tests/sources/elf/incremental-*`, and the harness
logic is in `sld/tests/integration_tests.rs`.

## Capacity growth and padding

Some changed-input updates need more room than the previous output layout reserved. sld exposes:

```sh
--incremental-padding-percent=N
```

The padding knob lets selected patchable output areas absorb bounded growth. The integration tests
use it for grown data sections and for generated sections such as `.eh_frame`. The benchmark runner
also supports changed-input growth cases.

Padding is intentionally explicit because it changes the incremental layout strategy. When a test
or benchmark relies on reserved capacity, it may stop byte-comparing the initial incremental output
against an ordinary full link: the larger incremental layout is the feature being tested.

## Conservative fallbacks

Incremental mode is not a promise to patch every change. It is a promise to stay correct and take a
faster path only when the saved metadata justifies it.

Examples of intentional fallbacks already covered in-tree include:

- a changed input section that is outside the current patchable subset,
- archive membership changes,
- section growth without enough reserved capacity,
- reordered linker inputs,
- added or removed linker inputs,
- an interrupted prior incremental update,
- output build-ID handling that cannot be updated incrementally,
- stale or incomplete incremental state.

The log makes those decisions inspectable. Typical lines look like:

```text
changed-input patch unavailable before loading inputs: ...
full relink: input file changed: ...
full relink: previous incremental update did not complete
```

That logging is not just a debugging aid. The benchmark runner and integration suite assert on these
messages so that a performance experiment cannot silently become a full fallback relink.

## State safety

sld's incremental state is part of the correctness contract:

- state writes are versioned,
- output updates are bracketed by an `update-in-progress` marker,
- a stale marker forces the next link to relink fully,
- metadata updates refresh rewritten-input identities,
- changed-input patches persist refreshed sidecar metadata for later incremental runs,
- repeated links after a patch must reuse the updated state rather than drifting back toward stale
  bookkeeping.

The metadata-refresh hardening work in `libsld/src/incremental.rs` is especially important here:
patching the binary is not enough; the persisted description of the patched binary must move forward
with it.

## Interaction with section GC

ELF incremental mode currently disables section garbage collection in the effective link path. The
argument parser preserves the user's `gc_sections` setting, but `should_gc_sections()` returns
`false` while incremental mode is active. That behavior is covered by tests in
`libsld/src/args/elf.rs`.

This is a layout-stability choice. It avoids letting GC reshuffle the output underneath an update
mechanism whose core job is to preserve and reason about prior layout.

## Benchmarking the feature

`BENCHMARKING.md` documents the benchmark runner syntax in detail. The short version is:

1. Capture a reproducible `run-with` link invocation.
2. Add a paired incremental benchmark that points at the same save-dir.
3. Warm up once to seed incremental state.
4. Time only the reuse or changed-input path you want to study.
5. Assert on sld's incremental log so the benchmark fails if it accidentally measures a fallback.

The checked-in `benchmarks/incremental-linux.toml` file provides concrete changed-input benchmarks
for `ruff`, `ty`, and `uv`. Each one:

- passes `--incremental`,
- mutates a specific ELF text section,
- expects sld to log a changed-input patch before loading inputs,
- requires the output binary to change,
- keeps ordinary `bfd`, `lld`, and `mold` timings in the same report for comparison.

When the report command sees paired full and incremental cases, it prints:

- sld's speedup over the other linker bins, and
- sld incremental speedup over the corresponding full sld link.

The benchmark docs also call out two important measurement details:

- pass `--no-fork` where supported so the timed process is the actual linker process,
- prefer explicit changed-input object sections for durable benchmark fixtures instead of relying on
  automatic section discovery in large Rust debug links.

## What this means in practice

Today, incremental sld is useful in three distinct ways:

1. No-change relinks can return almost immediately by reusing the prior output.
2. Rewritten-but-identical inputs avoid unnecessary relinks while keeping saved identities fresh.
3. A growing set of real changed-input cases can patch in place, including relocation-sensitive and
   generated-section-sensitive updates that used to require a full relink.

The final point is the important one. Incremental linking is no longer only a cache hit on "nothing
happened"; it is a constrained update protocol with explicit safety checks, observable fallbacks,
and a correctness harness designed to keep widening the patchable subset without weakening the
contract.
