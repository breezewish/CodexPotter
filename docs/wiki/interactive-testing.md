# Testing interactive flows (TUI + app-server)

Interactive behavior (keyboard shortcuts, cancellation, popups, stream recovery) is easy to break
because it spans multiple layers:

- TUI input handling (crossterm key events)
- TUI rendering pipeline (history insertion + transient viewport)
- Upstream `codex app-server` JSON-RPC bridge (turn lifecycle)
- Potter control-plane (`project/*` methods, multi-round runner)

This page describes a pragmatic strategy for testing these flows without relying on network access
or model behavior.

## Goals

- Keep tests deterministic and fast.
- Prefer behavioral assertions (what requests are sent / what is rendered) over internal state.
- Avoid depending on a real upstream `codex` binary, API keys, or network.

## Layer 1: Backend protocol tests with a dummy app-server

The `codex-potter-cli` crate contains integration-style tests for the upstream app-server driver in
`cli/src/app_server/codex_backend.rs`.

Pattern:

1. Create a temporary executable script (bash) that pretends to be `codex app-server`.
2. Assert requests by grepping JSON-RPC lines read from stdin.
3. Emit JSON-RPC responses and `codex/event/*` notifications on stdout.
4. Run `run_app_server_backend_inner` pointing `codex_bin` at the script.
5. Drive the backend with `Op`s and assert on emitted `EventMsg`s.

This is how `turn/interrupt` is tested:

- `app_server::codex_backend::tests::backend_turn_interrupt_requests_turn_interrupt`
- It verifies that an `Op::Interrupt` results in a JSON-RPC `turn/interrupt` request, and that a
  `turn_aborted` notification produces `EventMsg::PotterRoundFinished { outcome: Interrupted }`.

Notes:

- Gate these tests with `#[cfg(unix)]` because they rely on a bash script + `chmod`.
- Keep responses minimal but schema-correct; avoid `{}` when the driver expects structured fields.

## Layer 2: VT100 snapshots for TUI output

The `codex-tui` crate uses snapshot tests to validate rendered output without a real terminal
emulator.

Two useful building blocks:

- `VT100Backend` (a ratatui backend that feeds a `vt100` parser)
- `crate::custom_terminal::Terminal<VT100Backend>` + `insert_history_lines(...)` for terminal
  scrollback insertion

Example: Esc interrupt flow snapshot

- Test: `tui/src/app_server_render.rs` `round_renderer_esc_interrupt_flow_vt100`
- It simulates a key sequence (Esc) and injects a `TurnAborted(Interrupted)` event.
- The snapshot asserts that the transcript contains the user-facing interruption hint.

Snapshot workflow:

- Run `cargo test -p codex-tui` (or a filtered test).
- Inspect generated `*.snap.new` files.
- Accept a snapshot with `cargo insta accept --snapshot 'path/to/snapshot.snap'`.

## When you need "real" end-to-end tests

The most realistic approach is to run the `codex-potter` binary in a PTY and drive it with byte
sequences (Esc, arrows, etc.) while parsing screen output with `vt100`.

Trade-offs:

- More coverage (raw mode, crossterm decoding, alt-screen, timing)
- More engineering effort (PTY management, responding to terminal queries, flakiness)

If you add PTY tests, keep them `#[cfg(unix)]` and isolate terminal-protocol details in a helper
module.

