# Repository Guide

## Toolchain And Checks

- CI pins Rust 1.97.0; the workspace uses edition 2024 and has no `rust-toolchain` file.
- A bare root Cargo command covers only the curated `default-members`. Use `-p <package>` while iterating and `--workspace` for repository-wide verification.
- Match CI with `cargo fmt --all --check`, `cargo check --workspace --all-targets --all-features`, `cargo test --workspace --all-targets --all-features`, and `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- Run policy gates with `cargo run -p xtask -- parity check` and `cargo run -p xtask -- deps check`.
- Focus an integration target with `cargo test -p <package> --test <target>`; append `<test-name> -- --exact` for one named test. Acceptance commands are recorded in `parity.toml`.

## Boundaries And Entrypoints

- `apps/` contains composition roots; reusable code is layered under `crates/{foundation,media,gpu,io,features,services,ui}`. `xtask deps check` is the executable dependency-direction policy, including special rules for `fm-sim` and `fm-client`.
- The runnable dependency-light path is `freemix-cli`: local commands restore/mutate/save the simulated engine. `cargo run -p freemix-cli -- demo demo.freemix program.ppm` exercises that flow.
- `freemixd` is the headless authority/server and `freemix-studio` supervises or connects to it. Native daemon execution requires both compilation with `--features native-media` and the runtime `--native-media` option.
- Treat `docs/spec/` as the draft product contract, not proof of implementation. Check source, tests, and `parity.toml` status before claiming a feature exists.

## Contracts And Test Quirks

- Keep `parity.toml` and `docs/spec/feature-parity.md` synchronized. `xtask parity check` validates ledger structure and local evidence but does not execute acceptance commands; `--phase N` additionally enforces phase completion.
- A `.freemix` project path is a directory bundle containing `project.json`, not a single extension file.
- Protocol `.wire` files in `crates/foundation/fm-protocol/tests/fixtures/` are byte-for-byte compatibility fixtures. Persistence JSON fixtures under `crates/services/fm-persistence/tests/fixtures/` cover schema migration; update either set only with the corresponding compatibility change.
- FFmpeg integration tests skip when `ffmpeg`/`ffprobe` are unavailable; set `FM_REQUIRE_FFMPEG=1` when their absence must fail verification.
- On macOS, full-workspace/all-feature builds compile `crates/io/fm-io-macos/native/CameraHelper.swift` through `xcrun`; Xcode Command Line Tools and a macOS 13+ deployment target are required.
