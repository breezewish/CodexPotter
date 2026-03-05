<!--

For agents: This file is carefully maintained and polished for better readability. Don't edit this file.

-->

<p align="center">
  <img src="./etc/banner.svg" alt="CodexPotter banner" />
</p>

<p align="center">
  <img src="./etc/screenshot.png" alt="CodexPotter screenshot" width="80%" />
</p>

&ensp;

## 💡 Why CodexPotter

**CodexPotter** continuously **reconciles** code base toward your instructed state ([Ralph Wiggum pattern](https://ghuntley.com/ralph/)):

- 🤖 **Codex-first** — Codex subscription is all you need; no extra LLM needed.
- 🧭 **Auto-review / reconcile** — Review and polish multi rounds until fully aligned with your instruction.
- 💦 **Clean-room** — Use clean context in each round, avoid context poisoning, maximize IQ.
- 🎯 **Attention is all you need** — Keep you focused on _crafting_ tasks, instead of _cleaning up_ unfinished work.
- 🚀 **Never worse than Codex** — Drive Codex, nothing more; no business prompts which may not suit you.
- 🧩 **Seamless integration** — AGENTS.md, skills & MCPs just work™ ; opt in to improve plan / review.
- 🧠 **File system as memory** — Store instructions in files to resist compaction and preserve all details.
- 🪶 **Tiny footprint** — Use [<1k tokens](./cli/prompts/developer_prompt.md), ensuring LLM context fully serves your business logic.
- 📚 **Built-in knowledge base** — Keep a local KB as index so Codex learns project fast in clean contexts.

&ensp;

## ⚡️ Getting started

**1. Prerequisites:** ensure you have [codex CLI](https://developers.openai.com/codex/quickstart?setup=cli) locally. CodexPotter drives your local codex to perform tasks.

**2. Install CodexPotter via npm or bun:**

```shell
# Install via npm
npm install -g codex-potter
```

```shell
# Install via bun
bun install -g codex-potter
```

**3. Run:** Start CodexPotter in your project directory, just like Codex:

```sh
# --yolo is recommended to be fully autonomous
codex-potter --yolo
```

⚠️ **Note:** Unlike Codex, every follow up prompt turns into a **new** task, **not sharing previous contexts**. Assign tasks to CodexPotter, instead of chat with it.

### Prompting tips

**✅ tasks with clear goals or scopes:**

- "port upstream codex's /resume into this project, keep code aligned"

**✅ persist results to review in later rounds:**

- "create a design doc for ... **in DESIGN.md**"

**❌ interactive tasks with human feedback loops:**

CodexPotter is not suitable for such tasks:

- Front-end development with human UI feedback

- Question-answering

- Brainstorming sessions

&ensp;

## Roadmap

- [x] Skill popup
- [x] Resume (history replay + continue iterating)
- [x] Better handling of stream disconnect / similar network issues
- [x] Agent-call friendly (non-interactive exec and resume)
- [ ] Better plan / user selection support
- [ ] Better sandbox support
- [ ] Interoperability with codex CLI sessions (for follow-up prompts)
- [ ] Allow opting out knowledge base
- [ ] Recommended skills for PRD and code review

&ensp;

## Development

```sh
# Formatting
cargo fmt

# Lints
cargo clippy

# Tests
cargo nextest run

# Build
cargo build
```

&ensp;

## License

This project is community-driven fork of [openai/codex](https://github.com/openai/codex) repository, licensed under the same Apache-2.0 License.
