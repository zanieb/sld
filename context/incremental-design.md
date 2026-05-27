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

## Remaining Seed Cost And Cache Ownership

On a Linux `uv` saved link measured on 2026-05-27, the synchronous snapshot-retention step still
installed 651 patchable input snapshots, including 645 hardlinked Rust artifacts retaining about
2.4 GB of logical input bytes. Avoiding temporary-name installation for fresh Rust hardlinks reduced
the median `Snapshot incremental inputs` phase from `114.40 ms` to `105.58 ms`. This is useful for
such a small change, but also indicates diminishing returns from linker-local syscall reductions.

The same change was checked against its immediate parent on post-seed `uv` relinks with cooled,
order-balanced Linux samples and asserted incremental logs. No-change reuse was flat
(`485.28 ms` before versus `482.19 ms` after over five pairs). Changed-input patching showed a
small drift (`470.44 ms` before versus `481.27 ms` after over ten pairs), but that fast path calls
the existing single-input snapshot refresh rather than the fresh-seed snapshot installation changed
here. Balanced first-position RSS measurements for changed-input patching were effectively equal
(`1516.40 MiB` before versus `1516.31 MiB` after). Keep monitoring the timing drift, but it is not
evidence that the fresh-seed hardlink shortcut executes on post-seed patches.

Parallelizing construction of the patchable-input set during seed snapshot preparation reduced the
same captured Linux `uv` `Snapshot incremental inputs` median from `104.86 ms` to `86.37 ms`.
In a separately cooled default-fork run, visible seed-return latency fell from `972.68 ms` to
`930.58 ms`, while immediate no-change reuse remained flat (`98.10 ms` versus `96.58 ms`) and
logged reuse while state publication was pending. Fresh-seed peak RSS also remained flat-to-lower
in the alternating sample (`4,133,086 KiB` versus `4,111,380 KiB`). A log-asserted candidate proof
retained both the no-change reuse and changed-input patch fast paths.

The first changed-input link immediately after a fresh forked seed is a separate case from
steady-state patching. In a four-pair alternating Linux `uv` run, that first edit was flat across
the patchable-input collection change (`395.19 ms` before versus `398.09 ms` after, with one
`4.20 s` cold baseline outlier retained in the sample). Log-inspected immediate-edit runs patched
correctly, but first had to derive missing patch metadata while seed publication was completing;
an already-established changed-input state patched in `163.09 ms`. A directional thread-count
probe reduced concurrent no-change reuse, but lengthened publication completion, so simply
throttling background publication would make the first real edit wait longer.

Parallel grouping of persisted sidecar records was rerun after moving generated outputs to `/tmp`,
because concurrent remote runs had filled `/home` and caused a baseline `SIGBUS`. In four
alternating fresh-seed `uv` pairs, it reduced median `Persist incremental index and sections` from
`992.02 ms` to `887.60 ms` and synchronous `Link` from `3523.88 ms` to `3402.31 ms`. The
foreground result did not hold: first changed-input patches initially moved from `167.23 ms` to
`147.43 ms`, but a twelve-round confirmation moved from `154.58 ms` to `157.30 ms`, with asserted
patch logs in every measured run. The grouping prototype is therefore not retained.

Using zstd fast level `-3` instead of level `1` for the indexed sidecar was also rejected. In six
alternating fresh-seed `uv` pairs under `/tmp`, median `Persist incremental index and sections`
regressed from `792.15 ms` to `861.72 ms`, synchronous `Link` regressed from `3370.78 ms` to
`3386.35 ms`, and the sidecar grew from `59,119,521` bytes to `93,608,892` bytes. Since the seed
path was already slower, no post-seed patch lane was warranted for that prototype.

Computing each compressed block's location hash in the existing parallel block-build loop instead
of during serial sidecar assembly was likewise rejected. In six alternating fresh-seed `uv` pairs
under `/tmp`, median `Persist incremental index and sections` regressed from `773.80 ms` to
`796.95 ms`, synchronous `Link` regressed from `3346.87 ms` to `3374.71 ms`, and sidecar size
remained exactly `59,119,521` bytes. No post-seed lane was warranted for a slower seed path.

`uv`'s package cache is not directly a replacement for these snapshots. Its link modes install
files from an immutable cache tree and explicitly copy a file such as `RECORD` before installation
mutates it. The linker instead receives rustc output paths whose old bytes must survive a possible
subsequent replacement so changed-input diffing remains correct. Wild already takes the applicable
part of that approach by hardlinking atomically replaced `.rlib` and `.rcgu.o` files.

Related Cargo-native cache experiments reinforce that distinction. A root-output contract plus
native transient-input stabilization produced strong edit-loop wins on macOS, but a Cargo-native
`rlib` cache did not provide immutable old inputs to the linker: simultaneous cache and incremental
SLD use changed dependency `rlib`s and forced full relinks, while a staged cache handoff remained
slower than ordinary linking for its measured `ty` edit loop.

A larger seed win would require a producer-side contract: for example, Cargo or rustc could provide
immutable, content-addressed prior input objects, or eventually provide the section diff directly.
That direction matches the original design note, which anticipated both hardlinked prior objects
and a future compiler-supplied diff.

Changing the persisted record encoding, for example by adopting `rkyv`, is not a direct solution to
the remaining seed foreground cost. Index and section publication is already deferred after output
completion on the normal path. It may be worth prototyping for first-edit availability, because the
synchronous profile attributes `763.70-805.42 ms` to persisting the incremental index and section
sidecar, but only if more detailed profiling shows representation or compression work dominating
that phase and the versioned compatibility cost remains justified.

Temporary diagnostic spans on the Linux `uv` fixture further separated first-edit readiness from an
established edit loop. A corrected immediate-after-forked-seed trace still patched successfully, but
spent `291.11 ms` checking all incremental input identities while state publication and cold fixture
paths were in play; metadata reads contributed `18.16 ms` plus `9.74 ms`, and selective indexed
record loading contributed `12.61 ms`. That sample is directional rather than a stable benchmark
claim, since the profiler and concurrent publisher change the execution conditions.

Once the same output had completed one changed-input patch, four alternating log-asserted edits
completed in `40.86-42.56 ms`. In those warm steady-state edits, input identity checks fell to
`2.04-2.70 ms`, while reading incremental metadata remained `16.82-17.72 ms` and the patch body
remained `19.33-20.97 ms`. This sharpens a possible future encoding experiment: a smaller hot
metadata representation or binary deserialization format could improve repeated post-seed patches,
but it should be evaluated as a recurring hydration optimization, not as a direct seed-time fix.

Deferring patch-record location parsing on metadata-only reads implements a smaller version of that
hot-path optimization without changing the persisted format. The canonical index still retains the
exact raw `patch-records` table, and metadata-only index rewrites emit that table unchanged; it is
parsed into locations only when a newly changed input actually needs indexed sidecar records.

On the same captured Linux `uv` link invocation, a controlled fixed-size `.rodata` byte edit was
used to compare the prior retained binary with this change. Three alternating fresh lanes per binary
measured a seed, an exact no-change reuse, a first changed-input patch, and four established edit
toggles. Each measured edit lane logged changed-input patching. `zsh` `%M` peak-memory values were
interpreted as MiB after a `256 MiB` calibration allocation reported `265`.

| Phase | Previous median `Link` | Deferred-location median `Link` | Difference | Previous / deferred peak RSS |
| --- | ---: | ---: | ---: | ---: |
| Fresh synchronous seed | `3118.82 ms` | `3155.23 ms` | `+1.2%` | `4027 / 4029 MiB` |
| Exact no-change reuse | `60.69 ms` | `47.81 ms` | `-21.2%` | `51 / 47 MiB` |
| First changed-input patch | `82.61 ms` | `83.21 ms` | `+0.7%` | `43 / 40 MiB` |
| Established changed-input patch, 12 toggles | `43.03 ms` | `36.24 ms` | `-15.8%` | `28 / 26 MiB` |

The candidate is retained: seed and first-edit costs are effectively flat in this sample, while
recurring reuse and established edits are materially faster. Relative to the previously measured
`uv` full `sld` link of `855.20 ms`, this warmed diagnostic lane is `23.6x` faster, but it remains a
focused saved-link measurement rather than a replacement for the checked-in multi-project benchmark
matrix.

An audit of the sibling `rust-toolchain` experiments found that Cargo-native cache plus incremental
SLD composition was already tested and rejected for the measured edit loops. Clone or guarded
hardlink artifact materialization remains a planned but unexecuted experiment there, and no
completed `rkyv`-style metadata hydration experiment was found. Deferring the ELF patch-location
table is therefore complementary to those producer/cache questions rather than a repetition of
them.
