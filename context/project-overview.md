# sld Project Overview

sld is a linker focused on making iterative development faster. The long-term direction is
fine-grained incremental linking: preserve enough state from one link to make the next correct link
substantially cheaper when only a narrow part of the program changed.

That goal matters even though sld is already intended to be fast for ordinary full links. The
project direction is not merely "be another fast linker"; it is to make repeated developer-loop
links cheaper than starting over from scratch every time.

## Why This Project Exists

Existing high-performance linkers are excellent at full relinks, but incremental linking remains a
different problem with different correctness risks. sld exists to pursue that incremental path
directly while still remaining competitive for non-incremental work.

The implementation is written in Rust partly because the incremental design needs a large amount of
state bookkeeping, validation, and conservative fallback behavior. Correctness has priority over a
fast path that cannot be justified from persisted state.

## Current Shape

The Linux ELF path is the mature center of the repository today. It covers:

- x86-64, ARM64, RISC-V, and LoongArch64 Linux targets,
- static, static-PIE, dynamically linked, and shared-object outputs,
- Rust proc-macro links,
- debug info,
- GNU jobserver support,
- partial linker-script support.

Mach-O support also exists in-tree and is an active correctness and parity surface, especially for
Darwin saved-link replays and cross-linker comparison work. It should be described as partial rather
than absent. Windows support and fully general incremental updates remain outside the current
finished surface, and linker-plugin LTO is still experimental.

## Incremental Linking Direction

The incremental path is intentionally conservative:

1. Seed reusable state during an initial incremental link.
2. Reuse the previous output when the relevant state and inputs still match.
3. Patch a changed input in place when the saved metadata proves that it is safe.
4. Fall back to a full relink when the patch boundary is not justified.

The repository already has dedicated context for this:

- [`context/incremental-design.md`](incremental-design.md) for the design constraints,
- [`context/incremental-linking.md`](incremental-linking.md) for the current behavior ladder,
- [`context/verifying-correctness.md`](verifying-correctness.md) for validation expectations,
- [`context/incremental-performance.md`](incremental-performance.md) for lessons from recent runs.

Those notes are the durable project-memory layer for understanding what sld is trying to become and
how the implementation is expected to prove it.

## Benchmarking Philosophy

Benchmarks are part of the project direction, not just release decoration. We care about three
separate questions:

1. How fast is a normal full sld link?
2. How cheap is incremental no-change reuse?
3. How cheap is a real changed-input incremental update compared with both full sld and other
   production linkers?

The third comparison is the most important strategic test. Incremental work is only compelling when
it beats a full sld relink for the same workload, not merely when it beats a slower baseline linker.

For serious benchmark claims:

- capture reproducible saved-link inputs,
- use the benchmark runner instead of timing entire builds,
- keep outputs on tmpfs where practical,
- pass `--no-fork` when measuring the linker process itself,
- assert on incremental logs so a fallback relink cannot be mistaken for a fast path,
- require output-change checks when the benchmark is supposed to prove a changed-input update.

The benchmark corpus spans larger real-world workloads and multiple environments, including:

- Linux saved-link workloads such as Chromium, `librustc-driver`, `ruff`, `ty`, and `uv`,
- machine-specific reports for a Ryzen 9 9955HX system, an older Intel laptop, and Raspberry Pi 5,
- native macOS saved-link benchmarking for Darwin replay work.

The detailed benchmarking references live here:

- [`BENCHMARKING.md`](../BENCHMARKING.md) for the benchmark runner and saved-link workflow,
- [`context/benchmarking-incremental-performance.md`](benchmarking-incremental-performance.md),
- [`context/benchmarking-memory-consumption.md`](benchmarking-memory-consumption.md),
- [`context/benchmarking-macos.md`](benchmarking-macos.md),
- [`benchmarks/`](../benchmarks/) for generated reports, configs, and checked-in chart outputs.

## Related Project Context

These files round out the repository-level picture:

- [`DESIGN.md`](../DESIGN.md) for the high-level linker pipeline,
- [`context/output-comparison-findings.md`](output-comparison-findings.md) for cross-linker comparison
  work and known parity gaps,
- [`context/testing-in-apple-containers.md`](testing-in-apple-containers.md) for the Linux-validation
  workflow used from macOS hosts.
