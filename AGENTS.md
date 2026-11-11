# Repository Guidelines

## Project Structure & Module Organization

AgentEar is a macOS-first Rust application. `src/main.rs` is the entry point; focused `src/` modules cover audio, ASR, hotkeys, indexing, sessions, and the tray UI. Rust integration tests live in `tests/`; unit tests are colocated with modules. The Python TTS sidecar and its tests are in `services/tts/`. Put repeatable tooling in `scripts/`, architecture and release material in `docs/`, experiments in `spike/`, and bundled icons or voice metadata in `assets/`.

## Build, Test, and Development Commands

- `cargo build --locked`: build the debug binary from the lockfile.
- `cargo build --release`: produce the optimized binary.
- `cargo test --locked`: run the default Rust suite.
- `cargo clippy --all-targets`: run the CI lint pass. Do not add `-D warnings`; known warnings remain.
- `cargo fmt --all -- --check`: check formatting; omit `--check` to apply it.
- `python3 -m unittest discover -s services/tts -p 'test_*.py' -v`: test the TTS sidecar. Loudness cases require NumPy.
- `scripts/bundle.sh`: create the macOS bundle in `dist/`.
- `scripts/talk-e2e.sh --text '今天天气怎么样' --lang zh`: exercise the configured voice pipeline.

Development and CI require macOS because the project uses CoreAudio, AppKit, and Carbon. Models and ASR binaries under `vendor/` are uncommitted runtime assets.

## Coding Style & Naming Conventions

Use Rust 2021 and rustfmt defaults. Name modules, functions, and tests `snake_case`, types and traits `UpperCamelCase`, and constants `SCREAMING_SNAKE_CASE`. Python uses four spaces, `snake_case`, `unittest`, and `test_*.py` filenames. Run `scripts/lint-shell.sh` after shell changes. Repository scripts must use the intended virtual-environment interpreter, not bare `python3`.

## Testing Guidelines

Add regressions beside Rust logic or in `tests/` for cross-module behavior. Use `#[ignore]` only for networking, audio, or live-sidecar requirements, and document invocation. TTS changes require corresponding `services/tts/test_*.py` coverage and both test suites. During iteration, use targets such as `cargo test session::tests`.

## Commit & Pull Request Guidelines

Follow the Conventional Commit history: `feat(tts): ...`, `fix(index): ...`, `docs: ...`, or `chore(release): ...`. Keep subjects imperative and scoped. PRs should explain behavior, link issues, list verification, and include screenshots or recordings for UX changes. CI must pass, review goes through PR-Daemon, and merges are squashed. Release PRs update `Cargo.toml`, current-status documentation, `docs/releases/vX.Y.Z-notes.md`, and `docs/agent/progress.md` together.
