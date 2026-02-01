//! CodexPotter-specific helpers for recovering from transient streaming errors.
//!
//! `codex-potter` runs multi-round workflows. When Codex emits certain transient network/streaming
//! errors mid-turn (e.g. response stream disconnected), we want to keep the current round alive
//! and let the agent recover by issuing a follow-up `continue` prompt.

use crate::protocol::CodexErrorInfo;
use crate::protocol::ErrorEvent;
use crate::protocol::EventMsg;

/// Returns `true` when `event` represents a transient streaming/network failure.
///
/// These errors are typically recoverable by retrying the turn via a follow-up `continue`
/// prompt, instead of ending the round and starting a new one.
pub fn is_retryable_stream_error(event: &ErrorEvent) -> bool {
    match event.codex_error_info {
        Some(CodexErrorInfo::HttpConnectionFailed { .. })
        | Some(CodexErrorInfo::ResponseStreamConnectionFailed { .. })
        | Some(CodexErrorInfo::ResponseStreamDisconnected { .. })
        | Some(CodexErrorInfo::ResponseTooManyFailedAttempts { .. }) => true,
        _ => {
            // Best-effort fallback for older/partial servers that do not populate `codex_error_info`.
            //
            // Keep the checks tight to avoid accidentally treating unrelated errors as retryable.
            let message = event.message.as_str();
            message.contains("stream disconnected before completion")
                || message.contains("error sending request for url")
        }
    }
}

/// Returns `true` when `msg` counts as "activity" for CodexPotter stream recovery.
///
/// Activity is defined by the workflow spec as receiving any valid:
/// - agent message
/// - tool call result
/// - reasoning output
///
/// Observing any activity resets the exponential backoff and the retry limit for future
/// streaming/network errors.
pub fn is_activity_event(msg: &EventMsg) -> bool {
    matches!(
        msg,
        EventMsg::AgentMessage(_)
            | EventMsg::AgentMessageDelta(_)
            | EventMsg::AgentReasoning(_)
            | EventMsg::AgentReasoningDelta(_)
            | EventMsg::AgentReasoningRawContent(_)
            | EventMsg::AgentReasoningRawContentDelta(_)
            | EventMsg::AgentReasoningSectionBreak(_)
            | EventMsg::ExecCommandEnd(_)
            | EventMsg::PatchApplyEnd(_)
            | EventMsg::PlanUpdate(_)
            | EventMsg::ViewImageToolCall(_)
            | EventMsg::WebSearchEnd(_)
    )
}
