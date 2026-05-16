# Verifying incremental correctness

Incremental linking is only valuable if it is boringly correct. A fast patch that leaves stale
metadata behind, reuses an output from the wrong input order, or silently benchmarks a fallback as a
fast path is not a win.

This note collects the verification strategy that exists in the tree today:

- deterministic integration scenarios,
- unit tests around persisted state and rejection logic,
- log and sidecar assertions that prove which path actually ran,
- benchmark-runner guardrails for incremental timing,
- guidance for turning fuzz findings into permanent regression tests.

## Correctness model

sld's incremental tests are built around four invariants:

1. An unchanged relink must preserve the exact output bytes and reuse the previous output.
2. A rewritten-but-byte-identical input must refresh saved metadata without changing the output.
3. A changed-input incremental update must either:
   - produce the same bytes as a fresh full relink of the mutated inputs, or
   - intentionally preserve a padded incremental layout when that is the scenario under test.
4. Any case outside the currently patchable subset must log a clear fallback and complete as a
   correct full relink.

`sld/tests/integration_tests.rs` encodes that model directly in `run_incremental_test`.

## The integration-test harness

The test metadata format supports a substantial incremental matrix. The relevant knobs include:

- `TestIncremental`
- `TestIncrementalCompareFull`
- `TestIncrementalChanged`
- `TestIncrementalInterrupted`
- `TestIncrementalAddedInput`
- `TestIncrementalRemovedInput`
- `TestIncrementalReorderedInputs`
- `TestIncrementalChangedExpectPatch`
- `TestIncrementalChangedFallbackReason`
- `TestIncrementalChangedExpectReuse`
- `TestIncrementalChangedInput`
- `TestIncrementalChangedCompArgs`
- `TestIncrementalChangedSection`
- `TestIncrementalChangedSectionOffset`
- `TestIncrementalChangedGrowSection`
- `TestIncrementalChangedAppendArchiveMember`
- `TestIncrementalChangedCompareFull`
- `TestIncrementalChangedRestore`
- `TestIncrementalChangedSectionPrefix`
- `TestIncrementalChangedSymbolBytes`
- `TestIncrementalStateContains`

That DSL lets fixtures describe not only "link twice," but also the exact kind of mutation, the
expected fast path or fallback, the byte-level output evidence to inspect, and the persisted state
records that should remain afterward.

## Baseline sequence for every incremental fixture

For a normal incremental fixture, the harness performs this sequence:

1. Run a full sld link that serves as the comparison baseline.
2. Run the first incremental link.
3. Byte-compare that initial incremental output with the baseline unless the fixture intentionally
   reserves extra incremental capacity.
4. Run the second incremental link with no input changes.
5. Require byte-for-byte output stability.
6. Require the incremental log to contain:

```text
reused existing output
```

On Unix, the harness further requires:

```text
before loading inputs
```

That last condition proves the metadata fast path ran, rather than a slower post-load reuse path.

## Rewritten but unchanged inputs

Every incremental fixture also exercises a realistic nuisance case: a linker input is rewritten on
disk with the exact same bytes.

The harness:

1. rewrites one non-archive input with the same contents,
2. links again,
3. checks that the output bytes are unchanged,
4. requires the log to say:

```text
updated 1 rewritten input file before loading inputs
```

This protects the identity-refresh path in `libsld/src/incremental.rs`. It catches bugs where file
metadata changes would either force an unnecessary relink or leave stale file identities persisted
for the next update.

## Changed-input patch tests

When `TestIncrementalChanged:true` is present, the harness mutates one or more inputs and proves that
sld responds correctly.

For patchable cases, it checks:

- the output binary actually changes,
- any configured section-prefix or symbol-byte expectation matches,
- a repeated incremental link reuses the updated state and keeps the changed output stable,
- the changed incremental output matches a fresh full relink when the fixture says that comparison is
  valid,
- the log contains:

```text
patched N changed input file(s) before loading inputs
patched M changed input sections before loading inputs
```

Those checks appear in `sld/tests/integration_tests.rs`, and the test fixtures under
`sld/tests/sources/elf/incremental-*` cover a broad changed-input surface.

### Patch coverage already represented in-tree

| Area | Representative fixture families |
| --- | --- |
| Basic scalar data changes | `incremental`, `incremental-rodata`, `incremental-text` |
| Multiple mutated inputs / sections | `incremental-multiple`, `incremental-section-list` |
| Compiler-produced replacement objects | `incremental-source` |
| Growth with reserved capacity | `incremental-grow` |
| Anonymous and merged-string data | `incremental-anonymous`, `incremental-merge-string` |
| Relocation metadata | `incremental-relocated-data`, `incremental-relocation-target-moved` |
| Dynamic relocations | `incremental-dynamic-relocation`, `incremental-dynamic-relocation-added`, `incremental-dynamic-relocation-removed` |
| Frame metadata | `incremental-eh-frame-added`, `incremental-eh-frame-removed`, `incremental-text` variants |
| Archive input shapes | `incremental-archive`, `incremental-thin-archive` |
| Reversibility | fixtures using `TestIncrementalChangedRestore:true` |

The tests do not merely check that the linker exits successfully. They pin symbol bytes, generated
section bytes, sidecar contents such as `reloc2`, `dynrel`, and `fde` records, and repeated reuse of
the refreshed state after a patch.

## Fallback tests

A large part of incremental correctness is declining to patch when the current proof is not strong
enough. The fixture matrix includes intentional fallback cases for:

- changed bytes outside patchable sections,
- grown sections that cannot be resolved without reserved capacity,
- archive membership changes,
- added linker inputs,
- removed linker inputs,
- reordered input lists.

Fallback fixtures require the absence of a patch log and the presence of both:

```text
changed-input patch unavailable before loading inputs
full relink: input file changed:
```

Some tests also require a reason substring such as:

```text
changed bytes outside patchable sections
could not resolve patchable sections
archive members changed
```

That combination proves two things at once:

1. the incremental checker recognized the unsupported case,
2. the linker still finished via a correct full relink instead of taking a stale fast path.

## Interrupted-update recovery

`TestIncrementalInterrupted:true` creates an `update-in-progress` marker in the `.incr` directory and
runs another link. The harness requires:

- output bytes remain correct,
- the log says:

```text
full relink: previous incremental update did not complete
```

- the stale marker is cleared.

The corresponding runtime logic lives in `libsld/src/incremental.rs` around update-marker creation,
clearance, and stale-marker classification. This is a critical crash-recovery property: a partially
updated output must never be treated as reusable state.

## Persisted-state and sidecar unit tests

`libsld/src/incremental.rs` also has a large unit-test surface around the data model behind those
integration cases. Examples include:

- `.incr` state-directory naming,
- global incremental log rendering,
- stable snapshot path generation,
- snapshot isolation,
- metadata-update round trips,
- state parsing across current and legacy versions,
- invalid sidecar file names,
- changed-file identity checks while the fast path is running,
- interrupted-update classification,
- patchability rules for section flags,
- output-range overlap and overflow rejection.

These tests matter because incremental bugs often arise in bookkeeping rather than final code
generation. The metadata-refresh work in this area is a good example: after a successful patch, the
next link must read refreshed identities and refreshed relocation or frame records, not a stale view
of the pre-patch world.

## Benchmark checks are correctness checks too

`BENCHMARKING.md`, `benchmarks/incremental-linux.toml`, and
`benchmarks/runner/src/benchmarking.rs` add another useful layer:

- mutate inputs between timed runs,
- verify the linked output changed when that is expected,
- require incremental log substrings such as `patched ` and `before loading inputs`,
- reject benchmark runs that silently fell back to a full relink.

This prevents a common measurement failure: reporting "incremental speedup" while accidentally
timing a different path than the one under discussion.

The checked-in Linux benchmark config covers `ruff`, `ty`, and `uv` changed-input relinks. Its
fixtures use explicit ELF object sections rather than automatic selection so the benchmark remains
stable and continues to test the intended patch path.

## Fuzzing and regression capture

Fuzzing is most valuable here as a discovery engine, not as the final artifact. The durable contract
should live in deterministic tests.

When a fuzz campaign finds an incremental issue:

1. minimize the reproducer,
2. decide whether the correct behavior is patch, reuse, or fallback,
3. encode that expectation as an integration fixture or unit test,
4. assert on both output correctness and the path-specific log message,
5. keep the minimized case in-tree so the bug stays fixed.

That workflow complements the existing fixture matrix well. The integration harness is already rich
enough to preserve many incremental fuzz findings without inventing a new test framework.

## Suggested local verification ladder

When changing incremental behavior, run the smallest useful stack first and widen only as the change
demands:

```sh
cargo test -p sld-linker --test integration_tests incremental
```

```sh
cargo test -p libsld incremental
```

```sh
cargo test -p sld-linker --test integration_tests
```

For benchmark-runner changes or changed-input performance claims, also exercise the incremental
benchmark plumbing documented in `BENCHMARKING.md` and the checked-in
`benchmarks/incremental-linux.toml` config.

Some sandboxed environments cannot read the shared Cargo registry cache. In those cases, using an
isolated cache is a practical workaround:

```sh
CARGO_HOME=/private/tmp/sld-cargo-home cargo test -p libsld incremental
```

That keeps the verification itself from becoming noisy while preserving the same source-level test
coverage.

## Reading the evidence

For an incremental change, a good proof bundle contains:

1. the fixture or unit test that captures the intended behavior,
2. byte or symbol evidence that the output is right,
3. a log assertion that proves the intended incremental path ran,
4. a repeated-link assertion that proves refreshed state is reusable,
5. a fallback assertion when the change is intentionally unsupported,
6. benchmark assertions when the claim is about performance rather than only behavior.

That is the practical standard this tree is converging on. It is strong enough to keep the feature
moving quickly, while still making "correct full relink" the default answer whenever the saved state
cannot prove more.
