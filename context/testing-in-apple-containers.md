# Testing In Apple Containers

Apple Container is a useful way to validate sld's Linux behavior from a macOS workstation without
pretending that macOS-native execution is the same thing.

Use it when the question is materially Linux-specific:

- ELF linker correctness.
- Linux-targeted integration tests.
- Saved-link benchmark runs for Rust projects captured from Linux builds.
- Cross-linker comparisons against GNU ld and Mold.
- Memory measurements that should describe the Linux linker process itself.

Do not use it as a substitute for native macOS / Mach-O testing.

## Sanity Check The Runtime First

Before starting a longer validation run, make sure Apple Container itself works:

```sh
container run --rm docker.io/library/alpine:latest echo hello-from-container
```

If that fails, fix the container runtime before debugging sld.

Useful inspection commands:

```sh
container image list
container ls
```

The exact image can vary by task, but recent sld work used Bookworm-based Linux images with arm64
tooling available locally.

## What To Mount

For substantial sld work, keep host and container paths explicit. The common pattern is:

- The sld source checkout.
- A Linux build output directory for `sld`.
- Saved-link directories for downstream projects.
- A result directory under `/private/tmp` for logs, reports, and metrics.

In recent runs, those commonly appeared inside the container as:

```text
/target-sld
/saves
/results
```

The important rule is not the exact spelling. The important rule is to keep the command shape stable
across seed, patch, and rerun phases.

## Build And Test sld On Linux

Apple Container is a good fit for Linux-focused correctness checks when the host machine is macOS.

Typical uses include:

- Unit tests for the incremental core.
- Integration tests that exercise ELF behavior.
- Broader Linux-only validation after state-format or patching changes.

The repo-side commands stay ordinary:

```sh
cargo test -p libsld --lib
cargo test -p sld-linker --test integration_tests
```

The value of the container is that those commands run in a Linux userspace and kernel-facing model,
which matters for the linker behavior we actually ship there.

## Saved-Link Performance Runs

For linker timing, prefer the benchmark runner and saved `run-with` directories rather than timing
whole builds.

The key correctness conditions do not change inside Apple Container:

1. Use the same saved-link input for comparable runs.
2. Use `--no-fork` for sld and Mold when measuring the actual linker process.
3. Assert that incremental logs prove the expected fast path ran.
4. Mutate a known patchable object section for changed-input experiments.

That is how the checked-in `ruff`, `ty`, and `uv` benchmark shape should be interpreted.

## Incremental Seed Versus Patch Runs

For large workloads such as Codex, measure these separately:

1. Initial incremental seed link.
2. Changed-input incremental patch.

They answer different questions:

- Seed link: "What does it cost to opt into incrementality?"
- Patch link: "How cheap is the developer loop once state exists?"

A trustworthy patch-path benchmark should:

1. Seed the `.incr` directory.
2. Mutate a specific input known to be patchable.
3. Re-run the exact same linker invocation.
4. Check the incremental log.

The log should say something like:

```text
full relink: no previous incremental state
patched 1 changed input file before loading inputs
patched 1 changed input sections before loading inputs
```

Without that, the benchmark might be measuring a fallback relink rather than incremental patching.

## Measuring RSS

When you need process RSS rather than just elapsed time:

- Keep `--no-fork`.
- Measure the actual linker child process.
- Record wall time beside RSS.
- Save the logs and metrics together.

Recent work used a small Linux helper around `wait4` to record:

```text
wall_seconds=...
max_rss_kib=...
user_seconds=...
sys_seconds=...
exit_status=...
```

That helper made it practical to separate Codex seed memory from Codex patch memory without relying
on parent-process numbers from a forked linker.

## Memory Limits Matter

Apple Container memory defaults can be too small for large sld seed-link experiments.

One recent Codex seed probe was accidentally run in a 1 GiB container and died with `SIGKILL`. That
looked like a linker regression until the container budget was inspected.

For large measurements, set the memory budget explicitly, for example:

```sh
container run --rm --memory 24g ...
```

Then confirm the live container really has the intended budget:

```sh
container ls
```

Treat unexplained `signal=9`, empty metrics, or partial artifacts as suspicious until the container
memory limit has been ruled out.

## Command Shape Must Stay Stable

Incremental state includes link-option identity. If the command shape changes between the seed run
and the patch run, sld can correctly reject reuse with:

```text
full relink: linker arguments changed
```

That is expected linker behavior, but it invalidates a benchmark intended to measure patching.

Keep these stable:

- Output path.
- Linker arguments.
- `run-with` script.
- Incremental flags.
- Mount-relative paths that become visible inside the saved invocation.

If you want to test fallback behavior, change one deliberately. If you want a patch-path benchmark,
change only the intended input bytes.

## Mutate Patchable Bytes, Not Just Any Bytes

A changed-input experiment must mutate bytes sld can legally patch.

If the mutation lands outside the patchable subset, the log may say something like:

```text
metadata-only changed-input patch unavailable before loading inputs: changed bytes outside patchable sections ...
```

That is a useful fallback test, but it is not a patch-path benchmark.

For performance work:

- Prefer benchmark configs with explicit object paths and sections.
- Verify the log after every new saved-link workload.
- Treat byte-twiddling at the end of an object file as suspect unless you have already proven it
  hits the intended fast path.

## Artifacts Worth Keeping

For each meaningful Apple Container run, keep:

1. The saved benchmark result or `time` / RSS metric file.
2. The incremental log.
3. The exact benchmark config or shell command shape.
4. The output path / result directory.
5. A short note on whether the result is repeated evidence or a directional one-off.

Recent examples from the Codex incremental memory work:

- `/private/tmp/wild-codex-mem-interned-clean-20260512d/seed.metrics`
- `/private/tmp/wild-codex-mem-seedpatch-localfs-rerun-20260512l/patch.time`
- `/private/tmp/wild-codex-mem-seedpatch-localfs-rerun-20260512l/codex.incr/log`

## Common Failure Modes

### OOM masquerading as linker failure

Symptoms:

- `signal=9`
- Container exits without complete metrics.
- Large output artifacts appear, but the run never finishes cleanly.

Check:

- container memory limit,
- host disk availability,
- whether multiple large containers are overlapping.

### Full relink masquerading as incremental patching

Symptoms:

- Benchmark looks suspiciously slow.
- Log lacks `patched ... before loading inputs`.

Check:

- command-shape drift,
- mutated bytes outside patchable sections,
- missing previous state,
- mismatched output paths.

### Reused artifact masquerading as fresh verification

Symptoms:

- A result directory exists, but the command or state does not match the run you think it describes.

Check:

- timestamps,
- metrics/log pairing,
- whether the seed and patch came from the same back-to-back harness.

## Where This Fits In The Repo

Use this guide together with:

- `context/verifying-correctness.md`
- `context/benchmarking-incremental-performance.md`
- `context/benchmarking-memory-consumption.md`

Those documents explain what to prove. This document explains how to prove it faithfully from macOS
using Apple Container.
