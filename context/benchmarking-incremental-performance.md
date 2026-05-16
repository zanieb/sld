# Benchmarking Incremental Performance

Incremental benchmarks are easy to get wrong. A benchmark that merely runs `--incremental` is not
enough. It must prove that the measured run was actually incremental.

## The Three Comparisons That Matter

Use separate measurements for:

1. Full sld relink.
2. Incremental no-change reuse.
3. Incremental changed-input patching.

For changed-input work, also compare against:

- The system/default linker.
- Mold.
- Full non-incremental sld.

The last comparison is the most important one for project direction. Incrementality should beat a
full sld relink, not merely beat a much slower baseline linker.

## Built-In Benchmarking Support

`BENCHMARKING.md` and `benchmarks/incremental-linux.toml` already provide the right shape.

The key config features are:

- `sld_extra_flags = ["--incremental"]`
- `mutate_files = [...]`
- `expect_sld_log = [...]`
- `expect_output_change = true`

Example shape:

```toml
[bench.example-incremental-changed]
save = "example"
extra_flags = ["--no-fork"]
sld_extra_flags = ["--incremental"]
mutate_files = [
    { path = "target/debug/deps/example.rcgu.o", section = ".text.some_symbol" },
]
expect_sld_log = ["patched ", "changed input", "before loading inputs"]
expect_output_change = true
```

The `expect_sld_log` assertion is non-negotiable for serious claims. It prevents a benchmark from
silently measuring a full relink.

## Recommended Flow

1. Capture or refresh saved-link directories.
2. Run the benchmark runner against a tmpfs-backed output directory when possible.
3. Include `--no-fork` for sld and Mold when measuring the actual linker process.
4. Generate reports with stats, not only charts.
5. Treat large confidence intervals as a benchmark result that needs explanation.

Example report generation:

```sh
cargo run -q -p benchmark-runner -- report \
  --config benchmarks/incremental-linux.toml \
  --dir /tmp/sld-benchmark-report \
  --input /tmp/sld-benchmark-results/incremental-linux.bench-results \
  --print-stats
```

## May 12, 2026 Evidence Snapshot

The latest saved-link performance data is useful because it shows both the baseline linker picture
and the payoff from the metadata-only incremental path.

### Full sld vs Mold and GNU ld

For ordinary full links, sld looked strong:

| Project | GNU ld | Mold | sld | sld vs GNU ld | sld vs Mold |
| --- | ---: | ---: | ---: | ---: | ---: |
| `ruff` | 5536.59 ms | 706.35 ms | 457.36 ms | 12.11x | 1.54x |
| `ty` | 5957.20 ms | 703.12 ms | 481.59 ms | 12.37x | 1.46x |
| `uv` | 10857.09 ms | 1323.03 ms | 855.20 ms | 12.70x | 1.55x |

Source artifact:

- `/private/tmp/wild-benchmark-results/incremental-linux-metadata-only.bench-results`

### Changed-Input Incremental Runs

The metadata-only snapshot flips the changed-input story. The incremental patch path is now
meaningfully faster than both full sld and Mold on the checked-in `ruff` / `ty` / `uv` workloads:

| Project | Incremental changed | Full sld | Incremental vs full sld | Incremental vs Mold |
| --- | ---: | ---: | ---: | ---: |
| `ruff` | 125.38 ms | 457.36 ms | 3.65x | 5.63x |
| `ty` | 75.24 ms | 481.59 ms | 6.40x | 9.34x |
| `uv` | 200.19 ms | 855.20 ms | 4.27x | 6.61x |

The `ruff` and `uv` confidence intervals are still wide enough that they deserve another repeated
run before being promoted as stable headline numbers. The direction is nevertheless clear: reducing
metadata hydration cost changed the outcome materially.

That result says:

- The benchmark harness is catching real changed-input incremental work.
- The metadata-heavy proof step was previously erasing the benefit.
- Future work should continue to compare incremental changed relinks against full sld, not only
  against slower external linkers.

### Codex As A Positive Changed-Input Case

The Codex saved-link run showed the upside of the design:

| Case | Time |
| --- | ---: |
| Full sld | 1469.92 ms |
| Mold | 3035.02 ms |
| Incremental changed sld | 348.45 ms |

That corresponds to:

- 4.22x faster than full sld.
- 8.82x faster than Mold.

This Codex report came from a single-run benchmark matrix, so it is directional evidence rather than
a tight distribution. It is still useful because the effect size is large and the fast path was
verified separately through incremental logs.

Source artifact:

- `/private/tmp/wild-codex-results/codex-full-matrix.bench-results`

## Common Benchmarking Mistakes

Avoid these:

- Measuring a full relink and calling it incremental because `--incremental` was present.
- Mutating bytes outside the patchable subset, then interpreting fallback time as patch time.
- Comparing an incremental patch only to GNU ld while ignoring full sld.
- Measuring parent-process RSS or CPU for forked linkers.
- Reusing a prior incremental state directory with a changed command shape, which can trigger
  `full relink: linker arguments changed`.

## What Counts As Success

A meaningful incremental performance win should show:

1. The log proves the patch path ran.
2. The output mutation was semantically relevant.
3. The run is faster than a full sld relink for the same project.
4. The speedup is large enough to matter relative to measurement noise.
5. The result survives multiple runs or clearly states when it is only a directional probe.
