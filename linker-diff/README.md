# linker-diff

Linker-diff is a command-line utility that diffs two ELF binaries (shared objects or executables).
At least one of the binaries being diffed needs layout information as can optionally be produced by
the sld linker.

## Usage

The easiest way to use linker-diff is to first make sure it's installed into the same directory as
the sld linker, then build with the environment variable `SLD_REFERENCE_LINKER` set to the name of
another linker. e.g.

```sh
SLD_REFERENCE_LINKER=ld cargo test
```

When this variable is set, each time the sld linker is invoked, it'll call the specified linker
then run linker-diff on the result.
