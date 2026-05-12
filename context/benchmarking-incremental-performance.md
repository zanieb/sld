# Benchmarking Incremental Performance

Incremental benchmarks are easy to get wrong. A benchmark that merely runs `--incremental` is not
enough. It must prove that the measured run was actually incremental.

## The Three Comparisons That Matter

Use separate measurements for:

1. Full Wild relink.
2. Incremental no-change reuse.
3. Incremental changed-input patching.

For changed-input work, also compare against:

- The system/default linker.
- Mold.
- Full non-incremental Wild.

The last comparison is the most important one for project direction. Incrementality should beat a
full Wild relink, not merely beat a much slower baseline linker.

## Built-In Benchmarking Support

`BENCHMARKING.md` and `benchmarks/incremental-linux.toml` already provide the right shape.

The key config features are:

- `wild_extra_flags = ["--incremental"]`
- `mutate_files = [...]`
- `expect_wild_log = [...]`
- `expect_output_change = true`

Example shape:

```toml
[bench.example-incremental-changed]
save = "example"
extra_flags = ["--no-fork"]
wild_extra_flags = ["--incremental"]
mutate_files = [
    { path = "target/debug/deps/example.rcgu.o", section = ".text.some_symbol" },
]
expect_wild_log = ["patched ", "changed input", "before loading inputs"]
expect_output_change = true
```

The `expect_wild_log` assertion is non-negotiable for serious claims. It prevents a benchmark from
silently measuring a full relink.

## Recommended Flow

1. Capture or refresh saved-link directories.
2. Run the benchmark runner against a tmpfs-backed output directory when possible.
3. Include `--no-fork` for Wild and Mold when measuring the actual linker process.
4. Generate reports with stats, not only charts.
5. Treat large confidence intervals as a benchmark result that needs explanation.

Example report generation:

```sh
cargo run -q -p benchmark-runner -- report \
  --config benchmarks/incremental-linux.toml \
  --dir /tmp/wild-benchmark-report \
  --input /tmp/wild-benchmark-results/incremental-linux.bench-results \
  --print-stats
```

## May 12, 2026 Evidence Snapshot

The current saved-link performance data is useful because it shows both a success case and a real
gap.

### Full Wild vs Mold and GNU ld

For ordinary full links, Wild looked strong:

| Project | GNU ld | Mold | Wild | Wild vs GNU ld | Wild vs Mold |
| --- | ---: | ---: | ---: | ---: | ---: |
| `ruff` | 5686.18 ms | 877.66 ms | 480.80 ms | 11.83x | 1.83x |
| `ty` | 5804.23 ms | 699.16 ms | 466.62 ms | 12.44x | 1.50x |
| `uv` | 10607.51 ms | 1268.19 ms | 822.95 ms | 12.89x | 1.54x |

Source artifact:

- `/private/tmp/wild-benchmark-results/incremental-linux-final.bench-results`

### Changed-Input Incremental Runs

The current `ruff` / `ty` / `uv` changed-input measurements do **not** yet prove the desired win over
full Wild:

| Project | Incremental changed | Full Wild | Incremental vs full Wild | Incremental vs Mold |
| --- | ---: | ---: | ---: | ---: |
| `ruff` | 2002.74 ms | 480.80 ms | 0.24x | 0.44x |
| `ty` | 1431.73 ms | 466.62 ms | 0.33x | 0.49x |
| `uv` | 2449.74 ms | 822.95 ms | 0.34x | 0.52x |

That is not documentation noise. It is the performance hole to attack next.

The result says:

- The benchmark harness is catching real changed-input incremental work.
- The current patch bookkeeping can dominate the win on these projects.
- Any future speedup claim should be rerun against these cases, not only against a friendlier
  workload.

### Codex As A Positive Changed-Input Case

The Codex saved-link run showed the upside of the design:

| Case | Time |
| --- | ---: |
| Full Wild | 1469.92 ms |
| Mold | 3035.02 ms |
| Incremental changed Wild | 348.45 ms |

That corresponds to:

- 4.22x faster than full Wild.
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
- Comparing an incremental patch only to GNU ld while ignoring full Wild.
- Measuring parent-process RSS or CPU for forked linkers.
- Reusing a prior incremental state directory with a changed command shape, which can trigger
  `full relink: linker arguments changed`.

## What Counts As Success

A meaningful incremental performance win should show:

1. The log proves the patch path ran.
2. The output mutation was semantically relevant.
3. The run is faster than a full Wild relink for the same project.
4. The speedup is large enough to matter relative to measurement noise.
5. The result survives multiple runs or clearly states when it is only a directional probe.
