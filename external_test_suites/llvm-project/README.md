# Curated LLVM LLD corpus

This directory contains a small, repo-local curation of LLVM LLD tests that are useful for
exercising sld as an external linker corpus on Linux and macOS.

The scripts are adapted from upstream LLVM LLD tests rather than copied verbatim from LLVM's lit
suite. The adaptation keeps the inputs compact and converts the original lit/FileCheck recipes into
standalone shell scripts that fit sld's existing external-test harness.

- Upstream project: [`llvm/llvm-project`](https://github.com/llvm/llvm-project)
- Upstream snapshot used for this curation: `992df0aed5dda0a644c8939daa81b028f198651b`
- License of the upstream material: `Apache-2.0 WITH LLVM-exception`
- Upstream license text: [`LICENSE.TXT`](https://github.com/llvm/llvm-project/blob/992df0aed5dda0a644c8939daa81b028f198651b/LICENSE.TXT)

See [`curation.toml`](./curation.toml) for the per-test provenance records. When adding tests, keep
that manifest in sync with the shell script, include the original LLVM test path, and briefly note
what changed during adaptation.
