![codex-potter](./etc/banner.svg)

## 💡 Why CodexPotter

CodexPotter CLI takes your instructions and continuously **reconciles** the codebase toward your desired state (the [Ralph Wiggum](https://ghuntley.com/ralph/) pattern).

- 🤖 **Codex-first** — Codex subscription is all you need; no extra LLM needed; Local skills just works™

- 🚀 **Never worse than Codex** — Only drives Codex, nothing more; no business prompts which may not suit your project.

- 🧠 **File system as memory** — Stores your instructions in files to resist compaction and preserve all details.

- 📚 **Built-in knowledge base** - Keeps a local KB as an index / cache so that Codex learns project faster in clean contexts.

## ⚡️ Getting Started

```sh
cargo build
```

Then, run codex-potter CLI (available in `target/debug/codex-potter`) in your project directory:

```sh
codex-potter --yolo
```

Your prompt will become a task assigned to CodexPotter, and CodexPotter will help you run ralph loop to complete it.

⚠️ **Note:** Unlike codex, follow up prompts will become a **new** task assigned to CodexPotter, **without sharing contexts**.

## Development

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
