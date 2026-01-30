![codex-potter](./etc/banner.svg)

Designing philosophy:

- File system as memory
- Your prompt is always better than us -- potter only helps you run ralph loop, nothing more

## Getting Started

```sh
cargo build
```

Then, run codex-potter CLI (available in `target/debug/codex-potter`) in your project directory:

```sh
codex-potter --yolo
```

Your prompt will become a task assigned to CodexPotter, and CodexPotter will help you run ralph loop to complete it.

Note: During running, you can send more prompts, and all of these prompts will become a **new** task assigned to CodexPotter. Unlike codex, they will not share context.

## CI / Local checks

Our GitHub Actions CI runs the following checks on every PR and on pushes to `main`.
You can run the same commands locally:

```sh
# Formatting
cargo fmt --all -- --check

# Lints
cargo clippy --workspace --all-targets --locked -- -D warnings

# Tests (uses the repo's `ci-test` profile for faster CI-style builds)
cargo test --workspace --locked --profile ci-test

# Build
cargo build --workspace --all-targets --locked
```
