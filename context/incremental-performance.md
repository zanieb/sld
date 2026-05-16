# Incremental performance notes

This note captures the main performance lesson from the recent incremental work: changed-input
incremental relinks are extremely sensitive to how much persisted state they have to hydrate before
they can patch. The algorithmic win comes from avoiding a full relink, but the engineering win comes
from making the proof step cheap enough that it does not erase the benefit.

## Benchmark discipline

`BENCHMARKING.md` and `benchmarks/incremental-linux.toml` already encode the right measurement shape:

- use the benchmark runner rather than timing an entire build,
- keep linkers on the real process with `--no-fork`,
- mutate a concrete object section for changed-input tests,
- require `expect_sld_log` so the measured run actually took the intended incremental path,
- require `expect_output_change = true` when the benchmark is supposed to prove a linked-output
  difference.

That benchmark contract is part of the result. Without it, it is very easy to benchmark a fallback
relink while believing the changed-input patch path was measured.

## The headline lesson

Across the recent local benchmark snapshots, ordinary full sld links are already much faster than GNU
ld and moderately faster than mold on the checked-in `ruff`, `ty`, and `uv` workloads:

| Workload | Full sld vs GNU ld | Full sld vs mold |
| --- | ---: | ---: |
| `ruff` | `12.11x` | `1.54x` |
| `ty` | `12.37x` | `1.46x` |
| `uv` | `12.70x` | `1.55x` |

Those figures come from the latest local
`/private/tmp/wild-benchmark-results/incremental-linux-metadata-only.bench-results` snapshot.

The more important result is what happened to changed-input incremental relinks once the metadata path
became cheap:

| Workload | Incremental changed vs GNU ld | vs mold | vs full sld |
| --- | ---: | ---: | ---: |
| `ruff` | `44.16x` | `5.63x` | `3.65x` |
| `ty` | `79.17x` | `9.34x` | `6.40x` |
| `uv` | `54.23x` | `6.61x` | `4.27x` |

These are the "incremental speedup over baseline" figures printed by the benchmark runner from the
same metadata-only snapshot.

## What changed the result

The neighboring snapshots tell the story:

- `/private/tmp/wild-benchmark-results/incremental-linux-lazy.bench-results`
- `/private/tmp/wild-benchmark-results/incremental-linux-overlay.bench-results`
- `/private/tmp/wild-benchmark-results/incremental-linux-final.bench-results`
- `/private/tmp/wild-benchmark-results/incremental-linux-metadata-only.bench-results`

Before the metadata-only path, changed-input incremental relinks were still meaningfully faster than
GNU ld, but they were slower than both mold and full non-incremental sld on the same workloads. In the
`lazy`, `overlay`, and `final` snapshots, the changed-input path was only about `0.23x` to `0.40x` as
fast as full sld and about `0.34x` to `0.70x` as fast as mold.

The metadata-only snapshot flips that outcome. The changed-input patch path becomes several times
faster than mold and several times faster than full sld.

The practical conclusion is:

1. Persisted-state IO and record hydration are first-order costs for incremental linking.
2. A changed-input path that still reloads too much historical metadata can lose even when the actual
   patch is small.
3. The `metadata-update` / selective metadata loading direction is not just an implementation detail;
   it is central to making incremental relinks pay off.

## Codex as an exploratory scale check

There is also a larger one-off Codex benchmark snapshot in
`/private/tmp/wild-codex-results/codex-full-matrix.bench-results`. It reported:

- full sld about `2.09x` faster than mold and `81.23x` faster than GNU ld,
- changed-input incremental about `8.82x` faster than mold and `4.22x` faster than full sld.

That result is useful as an exploratory scale check because it points in the same direction as the
`ruff` / `ty` / `uv` measurements. It should not be treated as a headline claim yet: the saved report
contains single-run confidence intervals of `0.00`, so it is better read as a promising signal than as
stable benchmark evidence.

## How to use these results

For future performance work, the most useful questions are:

- Did the benchmark definitely take the expected incremental path?
- Did changed-input incremental improve relative to full sld, not only relative to GNU ld?
- Did the optimization help the metadata-heavy proof step, the patch application itself, or both?
- Did memory behavior move in the same direction, or did latency improve by expanding retained state?

That framing keeps the incremental performance conversation honest. The goal is not simply "beat ld";
it is to make a correctness-preserving changed-input update cheaper than the fast full linker sld
already is.
