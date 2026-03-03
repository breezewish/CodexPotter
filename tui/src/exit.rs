use codex_protocol::ThreadId;
use codex_protocol::protocol::TokenUsage;

/// Summary information produced when the CodexPotter TUI exits.
#[derive(Debug, Clone)]
pub struct AppExitInfo {
    /// Total token usage reported by the backend for the session/turn.
    pub token_usage: TokenUsage,
    /// The active thread ID, if known.
    pub thread_id: Option<ThreadId>,
    /// Why the TUI exited.
    pub exit_reason: ExitReason,
}

/// Reason why the CodexPotter TUI terminated.
#[derive(Debug, Clone)]
pub enum ExitReason {
    /// The run completed normally.
    Completed,
    /// The user interrupted or requested exit.
    UserRequested,
    /// The current task failed.
    TaskFailed(String),
    /// A fatal error occurred and the run cannot continue.
    Fatal(String),
}
