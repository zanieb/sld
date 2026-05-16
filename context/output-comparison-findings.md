# sld output comparison findings

Date: 2026-05-12
Host used for live runs: Darwin arm64, Apple `ld` 1266.8, Apple clang 21.0.0.

This note compares sld output against the closest available references in this checkout:

- Apple `ld`, with live Mach-O links on the current macOS host.
- GNU ld, through a live freestanding ELF comparison plus the repository's ELF comparison harness.
- mold, through a live freestanding ELF comparison plus the initialized mold submodule and skip list.

The live host did not have `mold` or GNU binutils initially. Homebrew packages `mold` 2.41.0 and
`x86_64-linux-gnu-binutils` 2.46.0 were installed to run the small live ELF comparisons below. The
host still lacks QEMU, Apple Container, and a running Docker daemon, so this note does not claim a
full Linux integration-test run.

## Existing comparison machinery

The main ELF correctness surface is `sld/tests/integration_tests.rs`.

- On Linux, `available_linkers_for_linux` enables GNU ld by default and then sld. LLD, gold, and
  mold are detected but disabled by default unless a fixture uses `EnableLinker`.
- When `run_all_diffs = true`, the harness links the same fixture with the enabled linkers, runs
  `linker-diff`, and then executes the outputs if the platform can run them.
- `linker-diff` is ELF-only. It needs at least one sld output with layout information.
- On macOS, `available_linkers_for_mac` now exposes Apple `ld` as an opt-in linker and still uses
  `ld.lld` if present, plus sld. In this environment `ld.lld` was not installed, so the original
  21 Mach-O integration tests were sld-only run/assertion checks before the opt-in Apple `ld`
  fixture coverage was added.
- The external-test surface is currently the mold shell test suite. The harness can also rerun those
  scripts through `SLD_EXTERNAL_LINKER=ld|lld|mold|gold|path` to distinguish sld-specific failures
  from failures shared with another linker.

## Live Mach-O comparison against Apple ld

Commands run:

- `cargo test -p sld-linker --test integration_tests -- --list`
- `cargo test -p sld-linker --test integration_tests -- macho/aarch64`
- Manual Apple `ld` and sld links for the `trivial`, `dynamic-import`, and `tlv` Mach-O fixtures.
- `file`, direct execution, `otool -L`, `otool -hV`, `otool -l`, `vtool -show-build`, `dyld_info`,
  and `codesign --verify --verbose=2` on the manual outputs.

Results:

- The Mach-O integration suite listed 21 tests and all 21 passed with sld.
- For the three manual fixtures, both Apple `ld` and sld produced arm64 Mach-O executables.
- All six manual outputs executed and exited with the expected code 42.
- All six outputs passed `codesign --verify --verbose=2`.

Structural differences observed in the three manual fixtures:

| Fixture | Apple ld size | sld size | Notable sld differences |
| --- | ---: | ---: | --- |
| `trivial` | 16,896 B | 59,600 B | sld emits fewer load commands but much larger `__LINKEDIT` / code-signature data. |
| `dynamic-import` | 49,976 B | 59,520 B | sld uses `LC_DYLD_CHAINED_FIXUPS`; Apple ld uses `LC_DYLD_INFO_ONLY`, `LC_DYSYMTAB`, lazy bind info, function starts, data-in-code, source version. |
| `tlv` | 33,432 B | 59,584 B | Both set `MH_HAS_TLV_DESCRIPTORS`; sld is still substantially larger. |

Other observed differences:

- sld records `/usr/lib/libSystem.B.dylib` with current version `0.0.0`; Apple `ld` records
  `1356.0.0` on this host.
- sld's `LC_BUILD_VERSION` has no tool entry; Apple `ld` records tool `LD` version `1266.8`.
- sld uses a fixed-looking UUID value in the observed output:
  `57494C44-2D4D-4143-484F-2D5555494421`.
- sld's `dynamic-import` symtab did not expose undefined `_printf` / `dyld_stub_binder` the same
  way as Apple `ld`; `dyld_info -imports` still showed `_printf` imported from libSystem.
- `dyld_info -fixups` succeeded on the Apple `ld` output. It exited unsuccessfully with no output on
  the sld chained-fixups output, even though the binary executed correctly.
- sld placed the `dynamic-import` string section under `__DATA,__cstring`, while Apple `ld` placed
  it under `__TEXT,__cstring`.
- Apple `ld` refused the direct `-nostdlib` trivial link without libSystem, while sld produced a
  runnable binary and still added libSystem metadata.

Confirmed Mach-O shortcoming from a normal clang-driver path:

- `clang --ld-path=target/debug/sld dynamic-import.c` failed with `sld: error: Couldn't identify
  file type`.
- Reproducing the clang link by hand showed that removing
  `/Library/Developer/CommandLineTools/usr/lib/clang/21/lib/darwin/libclang_rt.osx.a` made the sld
  link succeed.
- That runtime archive is a Mach-O universal binary containing per-architecture ar archives. Apple
  `ld` handles it; sld currently does not in this path.

Visible Mach-O future-work areas from source inspection:

- Repository-level overview material should describe Mach-O support as partial rather than absent;
  the implementation and test surface already exist in-tree.
- `libsld/src/macho_aarch64.rs` still has many `todo!()` architecture hooks.
- `libsld/src/macho.rs` has TODOs around optional segments, section kind derivation, sorting, and
  moved data types.
- `libsld/src/macho_writer.rs` has explicit unsupported cases for some subtractor relocations,
  `__eh_frame` 64-bit lengths, compact-unwind subtractor relocations, and dynamic-library comments.
- `libsld/src/args/macho.rs` supports only a small set of common Apple linker options and library
  names; unknown `-l` values warn as unsupported depending on `SLD_UNSUPPORTED`.

## Live ELF comparison against mold and GNU ld

Commands run:

- Installed `mold` 2.41.0 and `x86_64-linux-gnu-binutils` 2.46.0 with Homebrew.
- Compiled existing freestanding x86_64 Linux objects with Zig:
  - `sld/tests/sources/elf/trivial/trivial.c`
  - `sld/tests/sources/elf/data/data.c`
  - `sld/tests/sources/elf/common/runtime.c`
- Linked both fixtures with:
  - `target/debug/sld -flavor gnu -m elf_x86_64 --gc-sections -static`
  - `mold -m elf_x86_64 --gc-sections -static`
  - `x86_64-linux-gnu-ld -m elf_x86_64 --gc-sections -static`
- Compared with `file`, `x86_64-linux-gnu-readelf`, `x86_64-linux-gnu-objdump`, and
  `target/debug/linker-diff --sld-defaults`.

Results:

- Both fixtures linked successfully with all three linkers.
- All outputs were ELF64 x86-64 static executable files.
- The linked text for `trivial` was semantically the same across the three outputs; address bases
  and padding differed. Mold padded `_start` with `int3`, GNU ld padded with NOPs, and sld did not
  add the same visible post-function padding in that spot.

Size and layout snapshot:

| Fixture | sld | mold | GNU ld | Notable differences |
| --- | ---: | ---: | ---: | --- |
| `trivial` | 2,971 B | 4,576 B | 10,376 B | sld is smallest; mold adds `.eh_frame_hdr`, `.got`, `.got.plt`, and `PT_GNU_EH_FRAME`; GNU ld uses page-spaced file offsets. |
| `data` | 3,761 B | 5,232 B | 14,912 B | Same mold-vs-sld unwind/GOT differences; sld and mold both emit a `GNU_RELRO` segment, GNU ld does not for this static link. |

`linker-diff` findings:

- sld vs mold, both `trivial` and `data`:
  - sld is missing `.eh_frame_hdr`.
  - sld is missing `.got`.
  - sld is missing `.llvm_addrsig`.
  - sld is missing `PT_GNU_EH_FRAME` / GNU EH frame header metadata.
  - For `trivial`, mold also emitted `.got.plt`.
- sld vs GNU ld:
  - The clearest low-level metadata difference is `.eh_frame` section type, but the live Linux
    rerun showed this is input-sensitive rather than a simple "GNU is always X" rule. GNU ld
    preserves the input type: `SHT_PROGBITS` input produces `SHT_PROGBITS` output, while
    `SHT_X86_64_UNWIND` input produces `SHT_X86_64_UNWIND` output.
  - sld currently emits `SHT_PROGBITS` for both of those x86_64 `.eh_frame` shapes. That matches
    mold's documented policy of canonicalizing `SHT_X86_64_UNWIND` back to `SHT_PROGBITS`, but it
    differs from GNU ld when the input section already used `SHT_X86_64_UNWIND`.

Program-header differences:

- sld emits `PHDR`, read-only `LOAD`, executable `LOAD`, writable `LOAD`, `GNU_STACK`, and
  `GNU_RELRO` for `trivial`; `data` adds a separate writable data `LOAD`.
- A Linux/amd64 Rosetta run exposed a stricter runtime compatibility issue here: sld could emit
  all-zero writable `PT_LOAD` segments when the only kept sections in that segment were zero-sized
  `.data` / `.bss` style inputs, and it could also keep RELRO padding without real RELRO content.
  Rosetta rejects that shape with `bss_size overflow`.
- mold emits `PHDR`, read-only `LOAD`, executable `LOAD`, writable/RELRO `LOAD`, writable GOT/data
  `LOAD`, `GNU_EH_FRAME`, `GNU_STACK`, and `GNU_RELRO`.
- GNU ld emits fewer program headers for these static links: read-only `LOAD`, executable `LOAD`,
  unwind/read-only `LOAD`, optional data `LOAD`, and `GNU_STACK`.

Initial ELF takeaways:

- sld's static freestanding output is compact and has matching executable code for the simple
  fixtures, but it lacks mold's generated EH frame header surface.
- Compared with GNU ld, the most concrete low-level metadata difference in these fixtures is the
  input-sensitive `.eh_frame` section type policy. This should be treated as a deliberate
  GNU-vs-mold compatibility decision, not as an obvious correctness bug.
- Compared with mold, sld intentionally or accidentally omits GOT/GOT.PLT scaffolding in these
  static cases. That is probably fine for these inputs, but it is an observable output-policy
  difference and should be checked against dynamic and relocation-heavy fixtures in a Linux run.

## mold comparison from corpus and skip list

The mold submodule was initialized at `17956fdfa1ea18171587d02a7439e7dab6732ea1`.

Counts from the current checkout:

- Upstream mold shell tests present: 464.
- Tests named in `sld/tests/external_tests/mold_skip_tests.toml`: 186.
- Skip-list names missing from the checked-out mold corpus: 0.
- On this arm64 host, the harness collects 373 mold tests: 222 expected-pass tests and 151
  `expect_failure` tests. The difference from 464 is mostly architecture filtering.

Skip-list groups:

| Group | Count | Interpretation |
| --- | ---: | --- |
| `unsupported_options` | 44 | Command-line compatibility gaps such as `--icf`, `--repro`, `-Map`, tracing, retain-symbols, image-base, and assorted diagnostics options. |
| `gdb_index` | 7 | Missing GDB index support. Tracked in issue 811. |
| `version_script` | 1 | A remaining version-script case. |
| `z_options` | 8 | `-z` option semantics such as CET, nodefaultlib, nodump, rodynamic, sectionheader, and start-stop visibility. |
| `arch_x86_64` | 21 | x86_64-specific relocations, relaxations, GNU linkonce, textrel warnings, CET/IBT/SHSTK, and note layout differences. |
| `arch_aarch64` | 2 | Long/range-extension thunk cases. |
| `arch_riscv64` | 2 | RISC-V GOT relaxation and relocation-overflow cases. |
| `arch_loongarch64` | 4 | LoongArch relaxation cases. |
| `tls` | 2 | TLS common and local-exec error behavior. |
| `ignore` | 47 | Intentional differences, mold-specific behavior, message-format differences, output-section layout differences, and tests whose assertions assume mold branding. |
| `uncommon` | 12 | Lower-priority uncommon features such as section order/shuffle, separate debug file, start-stop, and zero-to-bss. |
| `misc` | 36 | Ungrouped failures that need triage into specific feature gaps or intentional differences. |

The highest-signal mold gaps are:

- Command-line compatibility breadth.
- Debug-index generation.
- Architecture-specific relaxation/relocation correctness.
- TLS edge cases.
- Versioning and symbol-table edge cases.
- Copy relocation / PLT / GOT layout differences that may be valid but still need deliberate policy.
- The `misc` bucket is too large; turning it into named categories is probably the fastest way to
  expose actionable future work.

Local execution check:

- Running `cargo test -p sld-linker --features mold_tests --test integration_tests --
  external_test_suites/mold/test/hello-dynamic.sh --exact` failed on macOS before it could serve as
  meaningful sld-vs-mold evidence. The failure included Bash 3 syntax incompatibility with `|&`,
  Apple clang target behavior, and `sld: error: -m llvm is not yet supported`.
- The external mold harness now runs correctly in an Apple Linux container when the sld checkout is
  mounted read-only, `/workspace/fakes-debug` is overlaid with a writable tmp directory whose
  `mold`, `ld`, and `ld.lld` entries point at `/target-sld/debug/sld`, and Cargo builds offline
  from the mounted host cache.
- With that setup, after removing `global-offset-table.sh` from the skip list, the Linux external
  mold run collected 373 tests and reported 373 passed / 0 failed.
- The last two positive failures, `range-extension-thunk.sh` and `section-start.sh`, were relevant
  AArch64 range / `--section-start` correctness tests. They now pass after teaching sld to place
  non-primary range-extension thunks in the source part and to keep ordinary `.text` before fixed
  custom executable sections.
- The removed `global-offset-table.sh` skip was verified as an enabled positive test in the Linux
  container, and the remaining host-side skip-list sentinels still pass: 151 `expect_failure` tests.

## GNU ld comparison from repo evidence

The ELF integration harness is designed around GNU ld as the default reference linker on Linux.
With `run_all_diffs = true`, it is the main binary-output comparator for sld. The live freestanding
ELF runs above cover only tiny static x86_64 fixtures; the broader GNU ld comparison still belongs
in the integration harness on Linux.

Important current limitations and signals:

- This macOS host can now run direct `x86_64-linux-gnu-ld` for freestanding static links, but still
  cannot run the full GNU ld comparison suite: no Linux sysroot, no QEMU, and no running Docker
  daemon.
- Many ELF fixtures include `DiffIgnore` directives. These are useful because they document known
  acceptable output differences from reference linkers, but they also mean "diff green" is not
  byte-for-byte identity.
- Some fixtures explicitly `SkipLinker:ld`, showing places where GNU ld cannot serve as the
  reference for that scenario or lacks the relevant behavior.
- LLD is enabled in many ELF fixtures as an additional reference. Mold is now enabled in a couple of
  low-noise ELF fixtures, while broader mold comparison still lives mostly in the external mold
  suite.
- `LINKER_SCRIPT_SUPPORT.md` is a good future-work map for GNU ld compatibility. Remaining gaps
  include richer `MEMORY` placement, `PHDRS`, `AT`, `FILL`, `SORT_BY_*`, `EXCLUDE_FILE`,
  `SIZEOF_HEADERS`, `DEFINED`, ternary expressions, `OVERLAY`, and several output-section controls.

## Suggested next work

Harness updates made after the initial comparison:

- Added an opt-in `apple-ld` Mach-O linker in the integration harness, using Apple `ld` plus the
  active macOS SDK path. It is enabled for the Mach-O `trivial` and `tlv` fixtures.
- Left `dynamic-import` sld-only for now: Apple `ld` currently produces different `__got` bytes
  from the existing fixture assertion. Covering that comparison cleanly needs per-linker assertions
  or a split fixture config.
- Enabled mold as an opt-in ELF reference for `custom-note` and `non-alloc`, with explicit ignores
  for mold's extra GOT / EH-frame-header surface.
- Fixed the external mold-test runner so it uses a Bash with `|&` support, invokes scripts through
  that Bash instead of the script shebang, and treats upstream `skip` exits as still skipped when
  checking skip-list regressions.
- Taught rooted filters such as `external_test_suites/mold`, `elf/...`, and `macho/...` to prune
  unrelated source-test collection even without `--exact`. That matters on minimal Linux containers
  where unrelated ELF fixtures may name optional linkers such as `ld.lld`.
- Removed `global-offset-table.sh` from the mold skip list after the Linux Apple Container run proved
  it now passes under sld.
- Added section-type assertions to the ELF integration harness. The new `.eh_frame` fixtures cover
  both ordinary `SHT_PROGBITS` inputs, which GNU ld and sld both emit as `SHT_PROGBITS`, and
  `SHT_X86_64_UNWIND` inputs, where GNU ld preserves `SHT_X86_64_UNWIND` but sld follows the
  mold-style `SHT_PROGBITS` policy.
- Added a `NoEmptyLoadSegment` ELF assertion and applied it to the `eh-frame` fixture that
  reproduced the Rosetta `bss_size overflow`; sld now keeps zero-sized sections without letting
  them alone keep an empty `PT_LOAD` segment.

1. Broaden Apple `ld` comparison for Mach-O.
   The integration path now has opt-in Apple `ld` execution for a couple of fixtures, but there is
   still no Mach-O structural comparator. Even a small comparator would catch the libSystem
   version/tool metadata, UUID, chained-fixups tooling compatibility, section-placement, and
   output-size differences above.

2. Fix or explicitly scope normal clang-driver Mach-O linking.
   The universal `libclang_rt.osx.a` failure blocks a normal `clang --ld-path=sld file.c` path.
   Supporting universal Mach-O archives, or intentionally filtering the relevant architecture, looks
   like a concrete high-value bug.

3. Reduce Mach-O metadata deltas.
   Decide which differences are intentional, then fix or document: libSystem current version,
   `LC_BUILD_VERSION` tool entries, UUID generation, optional `LC_*` commands, function starts,
   data-in-code/source-version policy, and symbol-table exposure for imports.

4. Split the remaining mold skip list into actionable categories.
   The `misc` group and parts of `ignore` should become smaller categories: true bugs, intentional
   linker-policy differences, diagnostic-only differences, unsupported options, and environment-only
   tests. The Linux rerun already found and removed one stale skip: `global-offset-table.sh`.

5. Broaden the Linux Apple Container run.
   The full external mold run is now working and passes. The next concrete command set should
   cover the normal ELF integration harness and third-party-linker comparison modes:
   - `SLD_TEST_CONFIG=test-config-ci.toml cargo test -p sld-linker --test integration_tests`
   - selected `SLD_EXTERNAL_LINKER=ld|lld|mold cargo test -p sld-linker --features mold_tests --test integration_tests <test>`
   - `SLD_IGNORE_SKIP=mold cargo test -p sld-linker --features mold_tests --test integration_tests` for a controlled failure census.

6. Expand mold as an integration-test reference.
   Mold is now enabled for a couple of low-noise fixtures, but most mold comparison still lives in
   the external mold suite. More `EnableLinker:mold` fixtures would make output comparison against
   mold more direct once the current EH-frame/GOT policy differences are either fixed or explicitly
   scoped.
