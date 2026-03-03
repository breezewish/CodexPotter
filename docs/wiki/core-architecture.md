# Core Architecture

`codex-potter` is a **multi-round** runner that repeatedly invokes a fresh `codex app-server`
process and uses the filesystem (not conversation history) as durable memory.

The key idea is:

- each round is a single "turn" in a fresh Codex thread
- the agent is instructed to read and update a progress file under `.codexpotter/`
- `codex-potter` renders the streamed events with a legacy Codex TUI formatting pipeline

This document focuses on the cross-crate architecture and the end-to-end runtime flow.

## Major components

### `codex-potter` CLI (crate: `codex-potter-cli`)

Module: `cli/src/main.rs`.

Responsibilities:

- Resolve the `codex` binary (`cli/src/startup.rs`).
- Optionally prompt to add `.codexpotter/` to the user's global gitignore
  (`cli/src/global_gitignore.rs` + TUI prompt helpers).
- Prompt for the initial project goal (via `codex_tui::CodexPotterTui`).
- Create the per-project progress file and knowledge base directory under the *current working
  directory* (`cli/src/project.rs`).
- Run a bounded number of rounds (`--rounds`), where each round:
  - starts a fresh external `codex app-server`
  - sends a fixed user prompt (`cli/prompts/prompt.md`)
  - injects a developer prompt that points at the progress file (`cli/prompts/developer_prompt.md`)
  - renders until the round finishes (signaled via `EventMsg::PotterRoundFinished`)
- Collect additional prompts that the user queues during a running turn; after a project ends, each
  queued prompt becomes a **new project** with a new `.codexpotter/projects/...` directory.

### External `codex app-server` process (upstream Codex CLI)

`codex-potter` does not run "codex core" in-process. Instead it spawns the upstream `codex` CLI in
app-server mode and communicates via JSON-RPC over stdin/stdout:

- spawn: `codex [--sandbox ...] [--dangerously-bypass-approvals-and-sandbox] app-server`
  (`cli/src/app_server_backend.rs`)
- protocol: a local copy of the app-server schema in `cli/src/app_server_protocol/` (v1/v2)

Within each round, `codex-potter` creates a new thread (`thread/start`) and then starts a turn
(`turn/start`). On retryable stream/network failures it may issue follow-up `continue` turns
(additional `turn/start`) within the same round.

Deep dive: `docs/wiki/app-server-bridge.md`.

### Protocol crate (crate: `codex-protocol`)

Module: `protocol/src/protocol.rs`.

Key types:

- `Op`: UI -> backend messages (e.g. `Op::UserInput`).
- `Event` / `EventMsg`: backend -> UI messages (events emitted by the app-server).

Potter-specific additions:

- `EventMsg::PotterProjectStarted`
- `EventMsg::PotterRoundStarted`
- `EventMsg::PotterRoundFinished`
- `EventMsg::PotterProjectSucceeded`

These markers are synthesized by the control plane (`cli/src/main.rs` and
`cli/src/app_server_backend.rs`, not emitted by the upstream app-server) so the TUI can render
project/round boundaries and successful project completion as normal history cells.

### TUI renderer (crate: `codex-tui`)

`codex-potter` uses a simplified subset of the upstream Codex TUI.

Key modules:

- `tui/src/potter_tui.rs`: `CodexPotterTui` wrapper that owns terminal lifetime and exposes:
  - `prompt_user(...)` for the initial goal
  - `render_round(...)` for a single round
  - queued prompts + composer draft persistence across rounds
- `tui/src/app_server_render.rs`: the round renderer that:
  - sends `Op::UserInput` to start the turn
  - consumes `EventMsg` and inserts `HistoryCell`s
  - renders the history viewport + bottom pane until the round finishes

## Persistent artifacts (`.codexpotter/`)

`codex-potter` relies on two kinds of persistence:

1. Per-project state under the *workdir* (the directory where `codex-potter` is launched):
   - `.codexpotter/projects/YYYY/MM/DD/N/MAIN.md` (progress file)
   - a gitignored knowledge base directory (scratchpad for intermediate findings)
2. Per-user state under the user's home directory:
- `~/.codexpotter/config.toml`
- `~/.codexpotter/history.jsonl` (prompt history for the composer)
- `~/.codexpotter/codex-compat/` (a "Codex home" shim; symlinks to `$CODEX_HOME/*` when set, otherwise `~/.codex/*`)

Everything under `.codexpotter/` is intended to be gitignored.

## Project + round model

Terminology used by `codex-potter`:

- **Project**: one user goal (one progress file). Created once per user prompt.
- **Round**: one `codex app-server` process invocation. A project runs up to `--rounds` rounds.
- **Turn**: in upstream app-server terms, one `turn/start` call. `codex-potter` typically runs one
  turn per round; on retryable stream/network errors it may issue a follow-up `continue` (another
  `turn/start`) within the same round.

Important implication: a multi-round project does *not* keep a Codex conversation thread alive.
Durable memory is the progress file and the repository state on disk.

## End-to-end flow

### 1) Startup

1. Resolve the `codex` binary (`cli/src/startup.rs`).
2. (Best-effort) configure `~/.codexpotter/codex-compat/` and pass it to the app-server by setting
   the `CODEX_HOME` environment variable when spawning the process
   (`cli/src/codex_compat.rs` + `cli/src/app_server_backend.rs`).
3. Initialize the terminal UI (`codex_tui::CodexPotterTui::new()`).
4. Optionally show a global gitignore recommendation prompt.
5. Prompt the user for the initial goal (`CodexPotterTui::prompt_user(...)`).

### 2) Project initialization

For each project goal:

1. Create `.codexpotter/projects/YYYY/MM/DD/N/MAIN.md` from `cli/prompts/project_main.md`.
2. Ensure the gitignored knowledge base directory exists.
3. Render a developer prompt that points to the progress file (`cli/prompts/developer_prompt.md`).

### 3) One round (one app-server process; typically one turn)

For each round:

1. CLI sends potter-only marker events to the UI stream:
   - `PotterProjectStarted` (only for the first round of a project)
   - `PotterRoundStarted` (for every round)
   - `PotterProjectSucceeded` (only when the project finishes successfully, i.e. `finite_incantatem: true`)
2. CLI spawns `codex app-server` and starts the JSON-RPC bridge task
   (`cli/src/app_server_backend.rs`).
3. Backend performs:
   - `initialize`
   - `thread/start` (approval policy is `never`; sandbox is derived from CLI flags)
   - synthesize and send `EventMsg::SessionConfigured` to the UI
4. UI starts the round renderer (`tui/src/app_server_render.rs`), which sends `Op::UserInput`
   to request `turn/start` with the fixed user prompt:
   `Continue working according to the WORKFLOW_INSTRUCTIONS`.
   (The injected developer instructions that point at the progress file are configured earlier via
   `thread/start`.)
5. Backend forwards `codex/event/*` notifications as `Event` values to the UI. The UI converts them
   into `HistoryCell`s and renders them.
6. When the control plane emits `EventMsg::PotterRoundFinished`, the UI exits the round
   runner. The CLI checks the progress file front matter for `finite_incantatem: true` and decides
   whether to stop the project early (`cli/src/project.rs`).

### 4) Queued prompts during a turn

While a turn is running, the bottom composer can queue additional prompts. These are stored by
`CodexPotterTui` and surfaced to the CLI via `CodexPotterTui::pop_queued_user_prompt()`.

Each queued prompt becomes a new project (a new progress file) after the current project finishes.
The prompts intentionally do **not** share a conversation context.

## Ownership notes

- The multi-round/project model and progress file conventions are potter-specific (`cli/` +
  `tui/src/potter_tui.rs`).
- The rendering pipeline is upstream-derived (`tui/`), but simplified to only the parts needed for
  prompt + round rendering operation.
