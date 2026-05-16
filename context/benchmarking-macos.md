# Benchmarking Native macOS Links

This note covers native Mach-O saved-link benchmarks on macOS. It is separate from
`context/testing-in-apple-containers.md`, which is about validating Linux behavior from a macOS
workstation.

Use this flow when you want to compare:

- `sld` replaying a Darwin saved link.
- `/usr/bin/clang`, which drives Apple's system linker for the same replay.

## Capture A Darwin Save-Dir

Capture the link by invoking sld directly as the Darwin linker:

```sh
SLD_SAVE_BASE=/tmp/sld/uv \
  cargo --config 'build.rustflags=["-C","linker=/path/to/sld","-C","link-arg=-flavor","-C","link-arg=darwin"]' \
  build --manifest-path /path/to/uv/Cargo.toml -p uv --bin uv
```

The build will create numbered save-dirs under `/tmp/sld/uv`. Inspect the trailing comment in each
`run-with` script to identify the link you want to benchmark:

```sh
tail -n 1 /tmp/sld/uv/*/run-with
```

For `uv`, the useful entry is the final binary link, not one of the many build-script or proc-macro
links produced during the build.

## Arrange The Benchmark Inputs

The benchmark runner expects a directory whose child names are benchmark names. Put the chosen
numbered save-dir under a named path such as:

```text
/tmp/sld/macos-saves/uv
```

That path may be a copy or a symlink to the numbered save-dir.

## Run The Benchmark Runner

Run the checked-in macOS benchmark config against sld and Apple clang:

```sh
cargo run -p benchmark-runner -- bench \
  --config benchmarks/macos.toml \
  --saves /tmp/sld/macos-saves \
  --tmp /tmp/linker-benchmark-out \
  target/release/sld /usr/bin/clang
```

The Darwin `run-with` script contains the sld-only `-flavor darwin` arguments. The benchmark runner
uses a small wrapper when it invokes Apple clang so that:

- `-flavor darwin` is removed before reaching `/usr/bin/clang`.
- `--no-fork` is removed before reaching `/usr/bin/clang`.
- the same saved-link input can still be measured through the existing runner and report pipeline.

The checked-in `benchmarks/macos.toml` config passes `--no-fork` to sld so memory measurements stay
attached to the actual linker process, matching the existing Linux benchmark discipline.

## Report Output

Benchmark result blobs such as `benchmarks/macOS linker benchmarks.bench-results` are generated
intermediates and are ignored by Git. The report command turns them into the checked-in markdown and
SVG artifacts under `benchmarks/` and `benchmarks/images/`.
