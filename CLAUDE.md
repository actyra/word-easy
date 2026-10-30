# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

The project rules — never crash, the asset/clean-room prohibitions, the layer architecture,
typing parity, the roadmap docs, contributor credits and the CI policy — live in one place and
are imported here rather than copied, so the two files can't drift apart:

@AGENTS.md

Everything below is the command surface those rules assume you already know.

## Commands

`cargo xtask` is a `.cargo/config.toml` alias for `cargo run --package xtask --`; `cargo xtask`
with no argument prints the full list.

| Task | Command |
|---|---|
| Everything, in order, stopping at the first failure | `cargo xtask ci` |
| Tests, then the interface audit, then doctests | `cargo xtask test` |
| One test | `cargo test -p <crate> --lib -- --exact <module>::<test>` |
| Doctests alone (nextest does not run them) | `cargo test --workspace --doc` |
| Format | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Wasm check | `cargo xtask wasm` |
| Asset attribution, crate layering | `cargo xtask assets`, `cargo xtask layers` |
| Interface audit + missing-translation report | `cargo xtask i18n-report` |
| Word parity summary → `docs/parity-checklist.md` | `cargo xtask parity` |
| DOCX fidelity run over the local corpus | `cargo xtask corpus docx [--save-baseline]` |
| Test and line counts per crate | `cargo xtask stats [--exact]` |
| Compare two PDFs page by page (needs poppler) | `cargo xtask visual-diff <ref.pdf> <test.pdf> <outdir>` |
| Run the app with the control channel | `cargo run --release -p wordcraft -- --sample --control 7981` |

`cargo xtask ci` is fmt → clippy `-D warnings` → test → i18n-report → doctest → assets → layers →
wasm. Run it before every commit.

### What the test runner actually does

- `cargo xtask test` prefers `cargo nextest run`, falling back to `cargo test` with a note when
  nextest isn't installed (`cargo install cargo-nextest --locked`).
- `.config/nextest.toml` pins a **minimum** nextest of 0.9.150 (GHSA-vc25-qg58-w57c). An older one
  refuses to run and exits 92 (`REQUIRED_VERSION_NOT_MET`) — upgrade rather than work around it.
- Targets are `--workspace --lib --bins --tests`. Examples are deliberately excluded so they aren't
  linked for a test run; `clippy --all-targets` still compiles them.
- `cargo xtask test` is three runs: the suite with
  `i18n::audit::untranslated_interface_text` filtered out, then that audit on its own and
  uncaptured so its report reaches the log, then the doctests. Each reuses the first build.

### Prerequisites

- `cargo xtask wasm` needs the target: `rustup target add wasm32-unknown-unknown`.
- `cargo xtask visual-diff` and `cargo xtask corpus docx` need poppler (`pdftoppm`, `pdftotext`)
  on `PATH`; `corpus docx` also needs `cargo build --release -p wordcraft-cli` first and prints
  setup instructions instead of failing when `corpus/docx/` is absent.
- `CRAFT_FONTS_DIR` and `CRAFT_I18N_DIR` are **optional** build inputs. Unset, the build warns and
  continues (English-only interface, no bundled craft fonts). Only a release sets
  `CRAFT_I18N_REQUIRED=1`, which turns the missing catalog into an error.

### On Windows, a plain checkout does not build

Every item below was hit on a clean Windows machine on 2026-10-30; without them `cargo xtask ci`
cannot run at all.

| Need | Why |
|---|---|
| rustc ≥ 1.95 | `craft-pdf` needs 1.95, `krilla` 1.92. Below that nothing in the workspace compiles. |
| NASM on `PATH` | `aws-lc-sys` is a C dependency reached through `apps/wordcraft` → `craft-print` → `winprint` → `reqwest`. Because `cargo xtask test` builds `--workspace --bins`, a missing NASM blocks **every** test, not just the desktop app. |
| `rustup target add wasm32-unknown-unknown` | The `wasm` step. Needs a rustup-managed toolchain: the standalone Rust MSVC installer cannot add targets. |
| poppler's `pdftotext` **before** Git's on `PATH` | Git for Windows ships Xpdf's in `mingw64/bin`. It has no `-bbox-layout`, so `visual-diff` and `corpus docx` get a usage screen instead of a result. |
| `CARGO_BUILD_JOBS=8` (or similar) | On a 32-core machine the default parallelism exhausts the Windows commit limit mid-build: `os error 1455` (paging file too small) and rustc dying with `STATUS_STACK_BUFFER_OVERRUN`, which then cascade into nonsense type errors. |

## Gotchas

- **`egui::ComboBox` is banned** by `clippy.toml` — use `craft_ui::ComboBox`. egui's combo box menu
  can't grow back once open (egui#5225).
- **Interface text must go through the translation catalog.** `crates/ui-egui/src/i18n/audit.rs`
  lays the app out in a pseudo-language and reports any visible word not wrapped in `⟦…⟧` marks.
  Two different outcomes: text that *skips the catalog* fails the audit and so fails CI; a catalog
  string that some language has *no translation for* is only listed in the report, never a failure.
- **The control-channel key path on Windows** is `%APPDATA%\WordCraft\control-key.local-<port>`
  (AGENTS.md lists only the Linux and macOS paths). See `docs/control-protocol.md` (Keys).
- `profile.dev` builds workspace crates at `opt-level = 1` and dependencies at 2, so a cold
  `cargo xtask ci` takes a long time. Give it room rather than assuming it hung.
