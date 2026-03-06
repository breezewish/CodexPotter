//! Upstream app-server protocol v2 payloads.
//!
//! This module contains request/response structs for v2 JSON-RPC methods (for example
//! `thread/start`, `thread/resume`, `thread/rollback`, `turn/start`) and the configuration types
//! they depend on.
//!
//! The shapes here intentionally mirror upstream Codex so the CLI can drive the `codex app-server`
//! subprocess without depending on its internal Rust types.

use std::collections::HashMap;
use std::path::PathBuf;

use codex_protocol::AbsolutePathBuf;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::user_input::ByteRange as CoreByteRange;
use codex_protocol::user_input::TextElement as CoreTextElement;
use codex_protocol::user_input::UserInput as CoreUserInput;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

/// Upstream approval policy for agent tool executions.
///
/// CodexPotter typically sets this to [`AskForApproval::Never`] and handles any "approval"-like
/// UX at a higher level.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AskForApproval {
    #[serde(rename = "untrusted")]
    UnlessTrusted,
    OnFailure,
    OnRequest,
    Never,
}

/// CLI-selected sandbox mode hint sent to the upstream app-server.
///
/// The app-server resolves this into a concrete [`SandboxPolicy`] and echoes the result back in
/// `thread/start` / `thread/resume` responses.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionApprovalDecision {
    Accept,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeApprovalDecision {
    Accept,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionRequestApprovalResponse {
    pub decision: CommandExecutionApprovalDecision,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRequestApprovalResponse {
    pub decision: FileChangeApprovalDecision,
}

/// Network access configuration for external sandbox policies.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NetworkAccess {
    #[default]
    Restricted,
    Enabled,
}

/// Concrete sandbox policy resolved by the upstream app-server.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SandboxPolicy {
    DangerFullAccess,
    ReadOnly,
    #[serde(rename_all = "camelCase")]
    ExternalSandbox {
        #[serde(default)]
        network_access: NetworkAccess,
    },
    #[serde(rename_all = "camelCase")]
    WorkspaceWrite {
        #[serde(default)]
        writable_roots: Vec<AbsolutePathBuf>,
        #[serde(default)]
        network_access: bool,
        #[serde(default)]
        exclude_tmpdir_env_var: bool,
        #[serde(default)]
        exclude_slash_tmp: bool,
    },
}

/// Parameters for the `thread/start` JSON-RPC method.
///
/// Note: optional fields are intentionally serialized as `null` when unset to match upstream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub sandbox: Option<SandboxMode>,
    pub config: Option<HashMap<String, JsonValue>>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    #[serde(default)]
    pub experimental_raw_events: bool,
}

/// Response payload for `thread/start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
    pub thread: Thread,
    pub model: String,
    pub model_provider: String,
    pub cwd: PathBuf,
    pub approval_policy: AskForApproval,
    pub sandbox: SandboxPolicy,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Parameters for the `thread/resume` JSON-RPC method.
///
/// Note: optional fields are intentionally serialized as `null` when unset to match upstream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: String,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub cwd: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub sandbox: Option<SandboxMode>,
    pub config: Option<HashMap<String, JsonValue>>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
}

/// Response payload for `thread/resume`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeResponse {
    pub thread: Thread,
    pub model: String,
    pub model_provider: String,
    pub cwd: PathBuf,
    pub approval_policy: AskForApproval,
    pub sandbox: SandboxPolicy,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Upstream thread metadata returned by `thread/*` methods.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    pub path: PathBuf,
}

/// Parameters for the `thread/rollback` JSON-RPC method.
///
/// Rollback only affects the thread history and does **not** revert any local file changes made by
/// the agent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRollbackParams {
    pub thread_id: String,
    /// The number of turns to drop from the end of the thread. Must be >= 1.
    pub num_turns: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRollbackResponse {
    pub thread: Thread,
}

/// Parameters for the `turn/start` JSON-RPC method.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    pub cwd: Option<PathBuf>,
    pub approval_policy: Option<AskForApproval>,
    pub sandbox_policy: Option<SandboxPolicy>,
    pub model: Option<String>,
    pub effort: Option<JsonValue>,
    pub summary: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
    pub collaboration_mode: Option<JsonValue>,
}

/// Upstream turn metadata returned by `turn/*` methods.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
}

/// Response payload for `turn/start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    pub turn: Turn,
}

/// Parameters for the `turn/interrupt` JSON-RPC method.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

/// Response payload for `turn/interrupt`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptResponse {}

/// Byte range into the prompt string, used to map UI placeholders.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

impl From<CoreByteRange> for ByteRange {
    fn from(value: CoreByteRange) -> Self {
        Self {
            start: value.start,
            end: value.end,
        }
    }
}

/// Prompt metadata for UI placeholders (for example mentions).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TextElement {
    pub byte_range: ByteRange,
    pub placeholder: Option<String>,
}

impl From<CoreTextElement> for TextElement {
    fn from(value: CoreTextElement) -> Self {
        Self {
            byte_range: value.byte_range.into(),
            placeholder: value.placeholder,
        }
    }
}

/// User input items passed to `turn/start`.
///
/// This is a JSON-RPC-friendly subset of [`codex_protocol::user_input::UserInput`]. Unknown
/// variants are treated as a programmer error to surface protocol drift early.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    Text {
        text: String,
        #[serde(default)]
        text_elements: Vec<TextElement>,
    },
    Image {
        url: String,
    },
    LocalImage {
        path: PathBuf,
    },
    Skill {
        name: String,
        path: PathBuf,
    },
    Mention {
        name: String,
        path: String,
    },
}

impl From<CoreUserInput> for UserInput {
    fn from(value: CoreUserInput) -> Self {
        match value {
            CoreUserInput::Text {
                text,
                text_elements,
            } => UserInput::Text {
                text,
                text_elements: text_elements.into_iter().map(Into::into).collect(),
            },
            CoreUserInput::Image { image_url } => UserInput::Image { url: image_url },
            CoreUserInput::LocalImage { path } => UserInput::LocalImage { path },
            CoreUserInput::Skill { name, path } => UserInput::Skill { name, path },
            CoreUserInput::Mention { name, path } => UserInput::Mention { name, path },
            _ => unreachable!("unsupported user input variant"),
        }
    }
}
