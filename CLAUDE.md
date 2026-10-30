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
| Tests (workspace lib/bins/tests, minus the i18n audit) | `cargo xtask test` |
| One test | `cargo test -p <crate> --lib -- --exact <module>::<test>` |
| Doctests (nextest does not run them) | `cargo test --workspace --doc` |
| Format / lint | `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` |
| Wasm check | `cargo xtask wasm` |
| Asset attribution, crate layering | `cargo xtask assets`, `cargo xtask layers` |
| Missing-translation report (never fails) | `cargo xtask i18n-report` |
| Word parity summary → `docs/parity-checklist.md` | `cargo xtask parity` |
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
- `cargo xtask test` skips `i18n::audit::untranslated_interface_text` and `cargo xtask i18n-report`
  runs only that one, uncaptured, so its report reaches the log.

### Prerequisites

- `cargo xtask wasm` needs the target: `rustup target add wasm32-unknown-unknown`.
- `cargo xtask visual-diff` needs poppler (`pdftoppm`, `pdftotext`) on `PATH`.
- `CRAFT_FONTS_DIR` and `CRAFT_I18N_DIR` are **optional** build inputs. Unset, the build warns and
  continues (English-only interface, no bundled craft fonts) — a plain checkout builds. Only a
  release sets `CRAFT_I18N_REQUIRED=1`, which turns the missing catalog into an error.

## Gotchas

- **`egui::ComboBox` is banned** by `clippy.toml` — use `craft_ui::ComboBox`. egui's combo box menu
  can't grow back once open (egui#5225).
- **Interface text must go through the translation catalog.** `crates/ui-egui/src/i18n/audit.rs`
  lays the app out in a pseudo-language and reports any visible word not wrapped in `⟦…⟧` marks;
  untranslated text fails CI.
- **The control-channel key path on Windows** is `%APPDATA%\WordCraft\control-key.local-<port>`
  (AGENTS.md lists only the Linux and macOS paths). See `docs/control-protocol.md` (Keys).
- `profile.dev` builds at `opt-level = 1` with dependencies at 2, so a first build is slow but
  tests run at near-release speed. Give a cold `cargo xtask ci` plenty of time.
