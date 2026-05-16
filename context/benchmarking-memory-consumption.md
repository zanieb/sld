# Benchmarking Incremental Memory Consumption

Incremental linking changes the memory story in two different ways:

1. The seed link may use more memory because it builds and persists reusable state.
2. The patch link should use much less memory because it avoids a full relink.

Measure those paths separately. Collapsing them into one "incremental memory" number hides the
tradeoff.

## The Metrics To Track

For each workload, keep separate RSS measurements for:

- Full non-incremental sld.
- Full Mold, when comparing against an established fast linker.
- Initial incremental seed.
- Incremental changed-input patch.

Always record:

- The exact command shape.
- The output path and state directory.
- Whether the measured linker process used `--no-fork`.
- The incremental log lines that prove the intended path ran.
- The container or host environment.

## Why `--no-fork` Matters

The benchmark runner already documents this, but it is easy to forget:

- sld and Mold may fork.
- Parent-process RSS is not representative of the actual linker work.
- Use `--no-fork` when measuring linker memory.

That is true for both benchmark-runner results and ad hoc memory probes.

## Recommended Approaches

### Benchmark runner

Use the normal benchmark runner when you want repeatable cross-linker memory reports.

For example, the Codex full-link memory comparison produced:

| Linker | Peak memory |
| --- | ---: |
| Mold | 5167.96 MiB |
| Full sld | 4475.03 MiB |

Source artifact:

- `/private/tmp/wild-codex-results/codex-full-memory.bench-results`

That is a useful full-link baseline, but it does not describe incremental seed or patch memory.

### Ad hoc incremental RSS probe

For large saved-link workloads, a small Linux helper that wraps the linker and reads `wait4`
resource usage is useful. Recent work used that pattern inside Apple Container to separate:

- Initial incremental seed RSS.
- Changed-input patch RSS.

The exact wrapper is less important than the invariants:

1. Run seed and patch with the same command shape.
2. Mutate a known patchable input section between the two runs.
3. Verify the incremental log after the patch.
4. Record peak RSS and wall time separately.

## May 12, 2026 Codex RSS Snapshot

The Codex saved-link measurements are a good reference point because they exposed both a serious seed
memory problem and a very lightweight patch path.

### Before the seed-memory reduction

One same-harness measurement recorded:

| Phase | Peak RSS | Wall time |
| --- | ---: | ---: |
| Incremental seed | 19,797,104 KiB | 202.486 s |
| Incremental patch | 822,740 KiB | 0.349 s |

Source artifact:

- `/private/tmp/wild-codex-mem-reloc2-20260512e/same-container.metrics`

### After the seed-memory reduction

The newer seed run recorded:

| Phase | Peak RSS | Wall time |
| --- | ---: | ---: |
| Incremental seed | 9,263,396 KiB | 203.416 s |

Source artifact:

- `/private/tmp/wild-codex-mem-interned-clean-20260512d/seed.metrics`

That is roughly a 2.14x reduction in seed RSS at essentially unchanged wall time for this case.

The changed-input patch path was then reverified separately:

| Phase | Peak RSS | Wall time |
| --- | ---: | ---: |
| Incremental patch | 834,716 KiB | 0.75 s |

Source artifacts:

- `/private/tmp/wild-codex-mem-seedpatch-localfs-rerun-20260512l/patch.time`
- `/private/tmp/wild-codex-mem-seedpatch-localfs-rerun-20260512l/codex.incr/log`

The log proved this was a real patch path:

```text
full relink: no previous incremental state
patched 1 changed input file before loading inputs
patched 1 changed input sections before loading inputs
```

## Apple Container Details

The practical Apple Container workflow for this style of measurement is collected in
`context/testing-in-apple-containers.md`. The short version is:

- set explicit memory budgets for large seed-link experiments,
- keep seed and patch command shapes stable,
- validate the incremental log before trusting the metric,
- treat mutations outside patchable sections as fallback tests, not patch benchmarks.

## How To Interpret Results

Good signs:

- Seed RSS falls without wall-time regression.
- Patch RSS stays much lower than full-link RSS.
- Patch logs show the expected incremental path.

Concerning signs:

- Patch RSS approaches seed RSS.
- Seed memory grows without a corresponding durability or capability gain.
- Memory improvements rely on a benchmark run that later turns out to have fallen back.

## What To Preserve In Future Reports

When documenting a memory improvement, include:

1. Before and after RSS.
2. Wall time alongside RSS.
3. The exact benchmark harness or artifact path.
4. The log proof that the intended incremental mode ran.
5. Whether the result is a repeated measurement or a one-off directional probe.
