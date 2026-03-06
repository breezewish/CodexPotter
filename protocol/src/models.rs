//! Helpers shared between codex-potter and the single-turn TUI runner.
//!
//! This crate intentionally keeps only the small subset of the upstream Codex
//! protocol/model helpers that are required by the renderer.

use serde::Deserialize;
use serde::Serialize;

/// Classifies an assistant message as interim commentary or final answer text.
///
/// Providers do not emit this consistently, so callers must treat `None` as
/// "phase unknown" and keep compatibility behavior for legacy models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    /// Mid-turn assistant text (for example preamble/progress narration).
    ///
    /// Additional tool calls or assistant output may follow before turn
    /// completion.
    Commentary,
    /// The assistant's terminal answer text for the current turn.
    FinalAnswer,
}

/// Placeholder label inserted into user text when attaching a local image.
///
/// This must remain byte-for-byte compatible with the legacy `codex-tui` UI so
/// prompt rendering and placeholder replacement stay unchanged.
pub fn local_image_label_text(label_number: usize) -> String {
    format!("[Image #{label_number}]")
}
