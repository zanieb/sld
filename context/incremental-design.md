# Incremental Design Notes

This note describes the incremental linker shape that exists in-tree today, and the design
constraints that should guide future work.

It is intentionally aligned with David Lattimore's 2024 design note:

- <https://davidlattimore.github.io/posts/2024/11/19/designing-wilds-incremental-linking.html>

The implementation is not yet the full end-state from that post. The important thing is that the
current code follows the same basic direction:

1. Preserve enough state from an initial incremental link to make future relinks cheap.
2. Reuse an existing output when nothing semantically changed.
3. Patch changed inputs in place when the linker can prove that doing so is safe.
4. Fall back conservatively when those proofs are not available.

## Operating Modes

`libsld/src/incremental.rs` currently behaves like four practical modes:

1. Non-incremental linking.
2. Initial incremental linking, which performs a full link and writes reusable state.
3. Incremental reuse, where an existing output can be accepted before loading all inputs.
4. Incremental changed-input patching, where the previous output is updated in place.

If a fast path is unsafe or unavailable, sld falls back to a full relink and logs the reason.

Typical log lines include:

```text
full relink: no previous incremental state
reused existing output before loading inputs
patched 1 changed input file before loading inputs
patched 1 changed input sections before loading inputs
full relink: linker arguments changed
```

These log messages are part of the design surface. They are how benchmarks and tests distinguish a
real incremental fast path from a silent fallback.

## Persisted State

Incremental linking writes a sibling state directory next to the output:

```text
<output>.incr/
```

The current implementation persists several classes of data there:

- Link configuration and input-order fingerprints.
- Input file identities and saved snapshots.
- Section metadata and patchability information.
- Relocation, dynamic relocation, and FDE metadata needed by later updates.
- Build ID state when the fast build ID path is available.
- Markers and logs that record interrupted or incomplete updates.

The state format is versioned in `libsld/src/incremental.rs`. New state revisions should be treated
as compatibility boundaries, not incidental churn.

## Reuse, Patch, Fallback

The current decision flow is deliberately conservative:

1. Check whether incremental mode is enabled.
2. Check whether previous state exists and is readable.
3. Check whether linker arguments, input ordering, and update markers still match expectations.
4. If the output is still valid, reuse it.
5. If specific inputs changed, try to map those changes onto patchable sections and update only the
   necessary output ranges.
6. If any requirement is not satisfied, relink fully and refresh incremental state.

This is the right shape. An incorrect fast path is much worse than a missed fast path.

## What "Patchable" Means Today

The changed-input path is intentionally narrower than "any changed object can be linked
incrementally."

The implementation can patch some same-layout changed object files in place, including important
cases involving:

- Selected `.text`, `.data`, and `.rodata` changes.
- Relocated data that requires refreshed relocation metadata.
- Dynamic relocation updates.
- `.eh_frame` / FDE bookkeeping changes.
- Some size growth when incremental padding has been reserved.

The implementation also intentionally falls back for changes outside the patchable subset, such as
bytes that cannot be mapped to safe patch sections or section growth that exceeds available slack.

The fixture suite under `sld/tests/sources/elf/incremental-*` is the best executable specification
of that boundary.

## Safety Invariants

Future changes should preserve these invariants:

- A reused or patched output must be justified by persisted state that still matches the current
  invocation.
- A changed-input patch must update both output bytes and the metadata needed by the next
  incremental run.
- Interrupted state updates must not be mistaken for valid reusable state.
- Tests and benchmarks must assert the incremental path they expect, rather than only checking that
  the linker exited successfully.
- Fallback reasons should remain explicit enough to diagnose lost incrementality.

The recent metadata refresh fix is a useful cautionary example: the output update itself can be
correct while the persisted bookkeeping becomes stale. That kind of bug only shows up when the next
incremental run tries to build on top of prior state.

## Alignment With The Original Design Note

The current implementation is aligned with the design note in the ways that matter most right now:

- It treats incremental linking as "diff plus apply", even when that diff is inferred from saved
  inputs rather than supplied by the compiler.
- It persists link state beside the output.
- It aims to avoid reprocessing all inputs when a narrower update is safe.
- It uses diagnostic logging to explain why a relink did or did not stay incremental.
- It falls back rather than pretending unsupported cases are incremental.

The major gap is also clear: changed-input incrementality is not yet broad enough, or cheap enough,
to be consistently faster than a full sld relink across all large Rust projects.

## Recent Implementation Notes

Recent work reduced the memory cost of the initial incremental seed path by:

- Interning repeated record text used in incremental state bookkeeping.
- Memory-mapping output bytes during seed finalization instead of heap-copying the whole output.

Those optimizations do not change the high-level design, but they do matter operationally. The seed
link has to stay affordable, or users will avoid enabling incrementality even if the patch path is
excellent.
