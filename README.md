# FreeMix

FreeMix is a cross-platform, GPU-first, Rust live production system
targeting the complete public vMix 29 Pro/Max feature surface.

The repository now includes a runnable deterministic MVP and its dependency-light
Phase 1 foundation:

- `fm-types`: portable media, time, format, and identity types;
- `fm-model`: versioned project configuration and preflight validation;
- `fm-command`: idempotent command, revision, transaction, and lifecycle state;
- `fm-capabilities`: discovery records, requirements, and compatibility reports;
- `fm-clock`, `fm-graph`, and `fm-scheduler`: deterministic media planning;
- `fm-switcher` and `fm-engine`: preview/program command acceptance and
  frame-boundary realization;
- `fm-video` and `fm-sim`: dependency-free CPU reference rendering; and
- `fm-persistence`: strict, atomically saved `.freemix/project.json` projects.

Run a complete simulated show, restart it, and render Program to a PPM image:

```sh
cargo run -p freemix-cli -- demo demo.freemix program.ppm
cargo run -p freemix-cli -- status demo.freemix
```

Use `cargo run -p freemix-cli -- help` for the individual `new`, `preview`,
`cut`, `fade`, `status`, and `render` commands.

Run all checks with:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The product specification remains the contract for subsequent phases:

- [Product and architecture specification](docs/spec/README.md)
- [Feature parity ledger](docs/spec/feature-parity.md)
- [Rust crate map](docs/spec/crates.md)
- [Step-by-step implementation roadmap](docs/spec/implementation-roadmap.md)

The specification is intentionally split into focused chapters. Start with the
index, then treat the parity ledger and phase exit criteria as the product
contract.
