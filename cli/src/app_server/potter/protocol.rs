//! JSON-RPC protocol for `codex-potter app-server`.
//!
//! This module defines the request/response types for CodexPotter's project-level app-server.
//! The wire format intentionally mirrors upstream Codex app-server JSON-RPC:
//!
//! - Request/notification envelopes use `method` + optional `id` + `params`.
//! - The `"jsonrpc": "2.0"` field is omitted (see `app_server_protocol::jsonrpc_lite`).
//!
//! Keeping the shapes close to upstream reduces mental overhead and makes it easier to share
//! tooling across `codex` and `codex-potter`.

use std::path::PathBuf;
use std::time::Duration;

use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::PotterRoundOutcome;
use serde::Deserialize;
use serde::Serialize;

use crate::app_server::upstream_protocol::InitializeParams;
use crate::app_server::upstream_protocol::JSONRPCNotification;
use crate::app_server::upstream_protocol::JSONRPCRequest;
use crate::app_server::upstream_protocol::RequestId;

pub const POTTER_EVENT_NOTIFICATION_METHOD: &str = "codex/event/potter";

/// Requests from a Potter app-server client.
///
/// The wire format intentionally mirrors upstream Codex app-server JSON-RPC requests:
/// - Uses `method` + `id` + `params`.
/// - Omits the `"jsonrpc": "2.0"` field (see `app_server_protocol::jsonrpc_lite`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method")]
pub enum PotterAppServerClientRequest {
    #[serde(rename = "initialize")]
    Initialize {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: InitializeParams,
    },

    #[serde(rename = "project/list")]
    ProjectList {
        #[serde(rename = "id")]
        request_id: RequestId,
        #[serde(default)]
        params: ProjectListParams,
    },

    #[serde(rename = "project/start")]
    ProjectStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ProjectStartParams,
    },

    /// Resume an existing CodexPotter project for replay-only (no new rounds start).
    #[serde(rename = "project/resume")]
    ProjectResume {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ProjectResumeParams,
    },

    /// Start iterating additional rounds after a successful `project/resume` call.
    #[serde(rename = "project/start_rounds")]
    ProjectStartRounds {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ProjectStartRoundsParams,
    },

    /// Interrupt the active project.
    ///
    /// The server first attempts a graceful interrupt (forward `Op::Interrupt` to the active
    /// round backend and allow `PotterRoundFinished` / `PotterProjectCompleted` markers to be
    /// emitted). If an interrupt was already requested and the project is still running, the
    /// server may force-abort it.
    #[serde(rename = "project/interrupt")]
    ProjectInterrupt {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ProjectInterruptParams,
    },

    /// Resolve an interrupted project (stop or continue iterating).
    #[serde(rename = "project/resolve_interrupt")]
    ProjectResolveInterrupt {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ProjectResolveInterruptParams,
    },
}

/// Notifications from a Potter app-server client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method")]
pub enum PotterAppServerClientNotification {
    #[serde(rename = "initialized")]
    Initialized,
}

impl TryFrom<JSONRPCRequest> for PotterAppServerClientRequest {
    type Error = serde_json::Error;

    fn try_from(value: JSONRPCRequest) -> Result<Self, Self::Error> {
        serde_json::from_value(serde_json::to_value(value)?)
    }
}

impl TryFrom<JSONRPCNotification> for PotterAppServerClientNotification {
    type Error = serde_json::Error;

    fn try_from(value: JSONRPCNotification) -> Result<Self, Self::Error> {
        serde_json::from_value(serde_json::to_value(value)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PotterEventMode {
    /// Optimized for interactive rendering: suppresses UI-irrelevant events (for example rollback
    /// lifecycle notifications and empty turn completions during stream recovery).
    #[default]
    Interactive,
    /// Optimized for `exec --json`: forwards the raw event stream so the JSONL translator can
    /// enforce closure invariants (`turn.*` / `potter.round.*`) without depending on interactive
    /// suppression rules.
    ExecJson,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectListParams {
    /// Optional working directory to search for `.codexpotter/projects`.
    ///
    /// When omitted, the server default workdir is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectListResponse {
    pub projects: Vec<ProjectListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectListEntry {
    /// Path passed back to `project/resume`.
    pub project_path: PathBuf,
    pub user_request: String,
    pub created_at_unix_secs: u64,
    pub updated_at_unix_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStartParams {
    pub user_message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_mode: Option<PotterEventMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStartResponse {
    /// Unique identifier for the active project within the server process.
    pub project_id: String,
    pub working_dir: PathBuf,
    pub project_dir: PathBuf,
    pub progress_file_rel: PathBuf,
    pub progress_file: PathBuf,
    pub git_commit_start: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub rounds_total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectResumeParams {
    /// Same semantics as the existing `codex-potter resume PROJECT_PATH`.
    pub project_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_mode: Option<PotterEventMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectResumeResponse {
    pub project_id: String,
    pub working_dir: PathBuf,
    pub project_dir: PathBuf,
    pub progress_file_rel: PathBuf,
    pub progress_file: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub replay: ProjectResumeReplay,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unfinished_round: Option<ProjectResumeUnfinishedRound>,
}

/// Replay payload for `project/resume`.
///
/// This is "history-only": it never re-runs tools, and it never starts a new round.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectResumeReplay {
    pub completed_rounds: Vec<ProjectResumeReplayRound>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectResumeReplayRound {
    pub outcome: PotterRoundOutcome,
    pub events: Vec<EventMsg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectResumeUnfinishedRound {
    pub round_current: u32,
    pub round_total: u32,
    /// Minimal boundary events that should be rendered before prompting for a follow-up action.
    pub pre_action_events: Vec<EventMsg>,
    /// Number of rounds remaining if the user chooses "Continue & iterate".
    pub remaining_rounds_including_current: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumePolicy {
    #[default]
    ContinueUnfinishedRound,
    StartNewRound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStartRoundsParams {
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_policy: Option<ResumePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_mode: Option<PotterEventMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStartRoundsResponse {
    pub rounds_total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInterruptParams {
    pub project_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveInterruptAction {
    Stop,
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectResolveInterruptParams {
    pub project_id: String,
    pub action: ResolveInterruptAction,
    /// Optional prompt override for the next turn.
    ///
    /// Required when `action` is [`ResolveInterruptAction::Continue`]. The server uses this as the
    /// first turn prompt when retrying the interrupted round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_prompt_override: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterruptedProjectSummary {
    pub rounds: u32,
    pub duration: Duration,
    pub user_prompt_file: PathBuf,
    pub git_commit_start: String,
    pub git_commit_end: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectResolveInterruptResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<InterruptedProjectSummary>,
}
