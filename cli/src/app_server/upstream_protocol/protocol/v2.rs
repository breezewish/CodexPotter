//! Upstream app-server protocol v2 payloads.
//!
//! This module contains request/response structs for v2 JSON-RPC methods (for example
//! `thread/start`, `thread/resume`, `thread/rollback`, `turn/start`) and the configuration types
//! they depend on.
//!
//! The shapes here intentionally mirror upstream Codex so the CLI can drive the `codex app-server`
//! subprocess without depending on its internal Rust types.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use codex_protocol::AbsolutePathBuf;
use codex_protocol::models::MessagePhase;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::plan_tool::PlanItemArg as CorePlanItemArg;
use codex_protocol::plan_tool::StepStatus as CorePlanStepStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::PlanType;
use codex_protocol::protocol::ServiceTier;
use codex_protocol::user_input::ByteRange as CoreByteRange;
use codex_protocol::user_input::TextElement as CoreTextElement;
use codex_protocol::user_input::UserInput as CoreUserInput;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value as JsonValue;

fn deserialize_upstream_codex_error_info_opt<'de, D>(
    deserializer: D,
) -> Result<Option<CodexErrorInfo>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<JsonValue>::deserialize(deserializer)?;
    Ok(value.and_then(upstream_codex_error_info_from_value))
}

fn upstream_codex_error_info_from_value(value: JsonValue) -> Option<CodexErrorInfo> {
    serde_json::from_value::<CodexErrorInfo>(value.clone())
        .ok()
        .or_else(|| upstream_codex_error_info_from_camel_case_value(value))
}

fn upstream_codex_error_info_from_camel_case_value(value: JsonValue) -> Option<CodexErrorInfo> {
    match value {
        JsonValue::String(name) => match name.as_str() {
            "contextWindowExceeded" => Some(CodexErrorInfo::ContextWindowExceeded),
            "usageLimitExceeded" => Some(CodexErrorInfo::UsageLimitExceeded),
            "serverOverloaded" => Some(CodexErrorInfo::ServerOverloaded),
            "internalServerError" => Some(CodexErrorInfo::InternalServerError),
            "unauthorized" => Some(CodexErrorInfo::Unauthorized),
            "badRequest" => Some(CodexErrorInfo::BadRequest),
            "threadRollbackFailed" => Some(CodexErrorInfo::ThreadRollbackFailed),
            "sandboxError" => Some(CodexErrorInfo::SandboxError),
            "other" => Some(CodexErrorInfo::Other),
            _ => None,
        },
        JsonValue::Object(fields) => {
            let mut entries = fields.into_iter();
            let (name, payload) = entries.next()?;
            if entries.next().is_some() {
                return None;
            }

            match name.as_str() {
                "httpConnectionFailed" => Some(CodexErrorInfo::HttpConnectionFailed {
                    http_status_code: upstream_http_status_code(payload),
                }),
                "responseStreamConnectionFailed" => {
                    Some(CodexErrorInfo::ResponseStreamConnectionFailed {
                        http_status_code: upstream_http_status_code(payload),
                    })
                }
                "responseStreamDisconnected" => Some(CodexErrorInfo::ResponseStreamDisconnected {
                    http_status_code: upstream_http_status_code(payload),
                }),
                "responseTooManyFailedAttempts" => {
                    Some(CodexErrorInfo::ResponseTooManyFailedAttempts {
                        http_status_code: upstream_http_status_code(payload),
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn upstream_http_status_code(value: JsonValue) -> Option<u16> {
    let JsonValue::Object(mut fields) = value else {
        return None;
    };

    fields
        .remove("httpStatusCode")
        .or_else(|| fields.remove("http_status_code"))
        .and_then(|status| status.as_u64())
        .and_then(|status| u16::try_from(status).ok())
}

/// Decode an upstream response service tier while tolerating values Potter does not model
/// explicitly.
///
/// Current Codex builds can return `"default"` in thread metadata. Potter only has distinct UX
/// for `fast` and `flex`, so anything else should degrade to `None` instead of failing the whole
/// response decode.
pub fn service_tier_from_value(value: &JsonValue) -> Option<ServiceTier> {
    serde_json::from_value::<ServiceTier>(value.clone()).ok()
}

fn deserialize_response_service_tier_opt<'de, D>(
    deserializer: D,
) -> Result<Option<ServiceTier>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<JsonValue>::deserialize(deserializer)?;
    Ok(value.as_ref().and_then(service_tier_from_value))
}

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

/// Configures who approval requests are routed to for review.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalsReviewer {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "guardian_subagent", alias = "auto_review")]
    AutoReview,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchMode {
    Disabled,
    #[default]
    Cached,
    Live,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSummary {
    #[default]
    Auto,
    Concise,
    Detailed,
    None,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Personality {
    None,
    Friendly,
    Pragmatic,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: JsonValue,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub defer_loading: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DynamicToolSpecDe {
    name: String,
    description: String,
    input_schema: JsonValue,
    defer_loading: Option<bool>,
    expose_to_context: Option<bool>,
}

impl<'de> Deserialize<'de> for DynamicToolSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let DynamicToolSpecDe {
            name,
            description,
            input_schema,
            defer_loading,
            expose_to_context,
        } = DynamicToolSpecDe::deserialize(deserializer)?;

        Ok(Self {
            name,
            description,
            input_schema,
            defer_loading: defer_loading
                .unwrap_or_else(|| expose_to_context.map(|visible| !visible).unwrap_or(false)),
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct ExecPolicyAmendment {
    pub command: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicyRuleAction {
    Allow,
    Deny,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyAmendment {
    pub host: String,
    pub action: NetworkPolicyRuleAction,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionApprovalDecision {
    Accept,
    AcceptForSession,
    AcceptWithExecpolicyAmendment {
        execpolicy_amendment: ExecPolicyAmendment,
    },
    ApplyNetworkPolicyAmendment {
        network_policy_amendment: NetworkPolicyAmendment,
    },
    Decline,
    Cancel,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeApprovalDecision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigRequirements {
    pub allowed_approval_policies: Option<Vec<AskForApproval>>,
    pub allowed_sandbox_modes: Option<Vec<SandboxMode>>,
    pub allowed_web_search_modes: Option<Vec<WebSearchMode>>,
    pub feature_requirements: Option<BTreeMap<String, bool>>,
    pub enforce_residency: Option<ResidencyRequirement>,
    pub network: Option<NetworkRequirements>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRequirements {
    pub enabled: Option<bool>,
    pub http_port: Option<u16>,
    pub socks_port: Option<u16>,
    pub allow_upstream_proxy: Option<bool>,
    pub dangerously_allow_non_loopback_proxy: Option<bool>,
    pub dangerously_allow_all_unix_sockets: Option<bool>,
    pub domains: Option<BTreeMap<String, NetworkDomainPermission>>,
    pub managed_allowed_domains_only: Option<bool>,
    pub allowed_domains: Option<Vec<String>>,
    pub denied_domains: Option<Vec<String>>,
    pub unix_sockets: Option<BTreeMap<String, NetworkUnixSocketPermission>>,
    pub allow_unix_sockets: Option<Vec<String>>,
    pub allow_local_binding: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkDomainPermission {
    Allow,
    Deny,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkUnixSocketPermission {
    Allow,
    None,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ResidencyRequirement {
    Us,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigRequirementsReadResponse {
    pub requirements: Option<ConfigRequirements>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdditionalNetworkPermissions {
    pub enabled: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdditionalFileSystemPermissions {
    pub read: Option<Vec<AbsolutePathBuf>>,
    pub write: Option<Vec<AbsolutePathBuf>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob_scan_max_depth: Option<NonZeroUsize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<FileSystemSandboxEntry>>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionProfile {
    pub network: Option<AdditionalNetworkPermissions>,
    pub file_system: Option<AdditionalFileSystemPermissions>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileSystemAccessMode {
    Read,
    Write,
    None,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileSystemSpecialPath {
    Root,
    Minimal,
    CurrentWorkingDirectory,
    ProjectRoots {
        subpath: Option<PathBuf>,
    },
    Tmpdir,
    SlashTmp,
    Unknown {
        path: String,
        subpath: Option<PathBuf>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileSystemPath {
    Path { path: AbsolutePathBuf },
    GlobPattern { pattern: String },
    Special { value: FileSystemSpecialPath },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemSandboxEntry {
    pub path: FileSystemPath,
    pub access: FileSystemAccessMode,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionProfileFileSystemPermissions {
    pub entries: Vec<FileSystemSandboxEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob_scan_max_depth: Option<NonZeroUsize>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionProfile {
    pub network: Option<AdditionalNetworkPermissions>,
    pub file_system: Option<PermissionProfileFileSystemPermissions>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GrantedPermissionProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<AdditionalNetworkPermissions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_system: Option<AdditionalFileSystemPermissions>,
}

impl From<RequestPermissionProfile> for GrantedPermissionProfile {
    fn from(value: RequestPermissionProfile) -> Self {
        Self {
            network: value.network,
            file_system: value.file_system,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionGrantScope {
    #[default]
    Turn,
    Session,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsRequestApprovalParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<AbsolutePathBuf>,
    pub reason: Option<String>,
    pub permissions: RequestPermissionProfile,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsRequestApprovalResponse {
    pub permissions: GrantedPermissionProfile,
    #[serde(default)]
    pub scope: PermissionGrantScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_auto_review: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputOption {
    pub label: String,
    pub description: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub is_other: bool,
    #[serde(default)]
    pub is_secret: bool,
    pub options: Option<Vec<ToolRequestUserInputOption>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub questions: Vec<ToolRequestUserInputQuestion>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputAnswer {
    pub answers: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputResponse {
    pub answers: HashMap<String, ToolRequestUserInputAnswer>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum McpServerElicitationAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum McpServerElicitationRequest {
    #[serde(rename_all = "camelCase")]
    Form {
        #[serde(rename = "_meta")]
        meta: Option<JsonValue>,
        message: String,
        requested_schema: JsonValue,
    },
    #[serde(rename_all = "camelCase")]
    Url {
        #[serde(rename = "_meta")]
        meta: Option<JsonValue>,
        message: String,
        url: String,
        elicitation_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpServerElicitationRequestParams {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub server_name: String,
    #[serde(flatten)]
    pub request: McpServerElicitationRequest,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpServerElicitationRequestResponse {
    pub action: McpServerElicitationAction,
    pub content: Option<JsonValue>,
    #[serde(rename = "_meta")]
    pub meta: Option<JsonValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallParams {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub tool: String,
    pub arguments: JsonValue,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallResponse {
    pub content_items: Vec<DynamicToolCallOutputContentItem>,
    pub success: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DynamicToolCallOutputContentItem {
    #[serde(rename_all = "camelCase")]
    InputText { text: String },
    #[serde(rename_all = "camelCase")]
    InputImage { image_url: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Account {
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {},
    #[serde(rename = "chatgpt", rename_all = "camelCase")]
    Chatgpt { email: String, plan_type: PlanType },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type")]
pub enum LoginAccountParams {
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {
        #[serde(rename = "apiKey")]
        api_key: String,
    },
    #[serde(rename = "chatgpt")]
    Chatgpt,
    #[serde(rename = "chatgptDeviceCode")]
    ChatgptDeviceCode,
    #[serde(rename = "chatgptAuthTokens", rename_all = "camelCase")]
    ChatgptAuthTokens {
        access_token: String,
        chatgpt_account_id: String,
        chatgpt_plan_type: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum LoginAccountResponse {
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {},
    #[serde(rename = "chatgpt", rename_all = "camelCase")]
    Chatgpt { login_id: String, auth_url: String },
    #[serde(rename = "chatgptDeviceCode", rename_all = "camelCase")]
    ChatgptDeviceCode {
        login_id: String,
        verification_url: String,
        user_code: String,
    },
    #[serde(rename = "chatgptAuthTokens", rename_all = "camelCase")]
    ChatgptAuthTokens {},
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CancelLoginAccountParams {
    pub login_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CancelLoginAccountStatus {
    Canceled,
    NotFound,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CancelLoginAccountResponse {
    pub status: CancelLoginAccountStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GetAccountParams {
    #[serde(default)]
    pub refresh_token: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GetAccountResponse {
    pub account: Option<Account>,
    pub requires_openai_auth: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChatgptAuthTokensRefreshReason {
    Unauthorized,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatgptAuthTokensRefreshParams {
    pub reason: ChatgptAuthTokensRefreshReason,
    pub previous_account_id: Option<String>,
}

/// Network access configuration for external sandbox policies.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NetworkAccess {
    #[default]
    Restricted,
    Enabled,
}

fn default_include_platform_defaults() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ReadOnlyAccess {
    #[serde(rename_all = "camelCase")]
    Restricted {
        #[serde(default = "default_include_platform_defaults")]
        include_platform_defaults: bool,
        #[serde(default)]
        readable_roots: Vec<AbsolutePathBuf>,
    },
    #[default]
    FullAccess,
}

/// Concrete sandbox policy resolved by the upstream app-server.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SandboxPolicy {
    DangerFullAccess,
    #[serde(rename_all = "camelCase")]
    ReadOnly {
        #[serde(default)]
        access: ReadOnlyAccess,
        #[serde(default)]
        network_access: bool,
    },
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
        read_only_access: ReadOnlyAccess,
        #[serde(default)]
        network_access: bool,
        #[serde(default)]
        exclude_tmpdir_env_var: bool,
        #[serde(default)]
        exclude_slash_tmp: bool,
    },
}

/// Sticky or turn-scoped environment selected through the upstream app-server.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnEnvironmentParams {
    pub environment_id: String,
    pub cwd: AbsolutePathBuf,
}

/// Parameters for the `thread/start` JSON-RPC method.
///
/// Note: optional fields are intentionally serialized as `null` when unset to match upstream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub service_tier: Option<Option<ServiceTier>>,
    pub cwd: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox: Option<SandboxMode>,
    pub permission_profile: Option<PermissionProfile>,
    pub config: Option<HashMap<String, JsonValue>>,
    pub service_name: Option<String>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub personality: Option<Personality>,
    pub ephemeral: Option<bool>,
    pub environments: Option<Vec<TurnEnvironmentParams>>,
    pub dynamic_tools: Option<Vec<DynamicToolSpec>>,
    pub mock_experimental_field: Option<String>,
    #[serde(default)]
    pub experimental_raw_events: bool,
    #[serde(default)]
    pub persist_extended_history: bool,
}

/// Response payload for `thread/start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
    pub thread: Thread,
    pub model: String,
    pub model_provider: String,
    #[serde(default, deserialize_with = "deserialize_response_service_tier_opt")]
    pub service_tier: Option<ServiceTier>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub instruction_sources: Vec<AbsolutePathBuf>,
    pub approval_policy: AskForApproval,
    pub approvals_reviewer: ApprovalsReviewer,
    pub sandbox: SandboxPolicy,
    #[serde(default)]
    pub permission_profile: Option<PermissionProfile>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Parameters for the `thread/resume` JSON-RPC method.
///
/// Note: optional fields are intentionally serialized as `null` when unset to match upstream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: String,
    pub history: Option<Vec<JsonValue>>,
    pub path: Option<PathBuf>,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub service_tier: Option<Option<ServiceTier>>,
    pub cwd: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox: Option<SandboxMode>,
    pub permission_profile: Option<PermissionProfile>,
    pub config: Option<HashMap<String, JsonValue>>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub personality: Option<Personality>,
    #[serde(default)]
    pub persist_extended_history: bool,
}

/// Response payload for `thread/resume`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeResponse {
    pub thread: Thread,
    pub model: String,
    pub model_provider: String,
    #[serde(default, deserialize_with = "deserialize_response_service_tier_opt")]
    pub service_tier: Option<ServiceTier>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub instruction_sources: Vec<AbsolutePathBuf>,
    pub approval_policy: AskForApproval,
    pub approvals_reviewer: ApprovalsReviewer,
    pub sandbox: SandboxPolicy,
    #[serde(default)]
    pub permission_profile: Option<PermissionProfile>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Upstream thread metadata returned by `thread/*` methods.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub model_provider: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub status: JsonValue,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(default)]
    pub cli_version: String,
    #[serde(default)]
    pub source: JsonValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_info: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub turns: Vec<Turn>,
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

/// Collaboration mode kind applied to a `turn/start` request.
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CollaborationModeKind {
    Plan,
    #[default]
    Default,
}

/// Settings bundled with a collaboration mode.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CollaborationModeSettings {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub developer_instructions: Option<String>,
}

/// Collaboration mode preset applied to a turn.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub struct CollaborationMode {
    pub mode: CollaborationModeKind,
    pub settings: CollaborationModeSettings,
}

/// Parameters for the `turn/start` JSON-RPC method.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    pub environments: Option<Vec<TurnEnvironmentParams>>,
    pub cwd: Option<PathBuf>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox_policy: Option<SandboxPolicy>,
    pub permission_profile: Option<PermissionProfile>,
    pub model: Option<String>,
    pub service_tier: Option<Option<ServiceTier>>,
    pub effort: Option<ReasoningEffort>,
    pub summary: Option<ReasoningSummary>,
    pub personality: Option<Personality>,
    pub output_schema: Option<JsonValue>,
    pub collaboration_mode: Option<CollaborationMode>,
}

/// Upstream turn metadata returned by `turn/*` methods.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    #[serde(default)]
    pub items: Vec<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TurnStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TurnError>,
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

/// Parameters for the `thread/backgroundTerminals/clean` JSON-RPC method.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBackgroundTerminalsCleanParams {
    /// Thread whose background terminals should be stopped.
    pub thread_id: String,
}

/// Response payload for `thread/backgroundTerminals/clean`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBackgroundTerminalsCleanResponse {}

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

// === Server notifications (subset) ===
//
// Newer upstream Codex app-server versions translate internal `EventMsg` values into typed JSON-RPC
// notifications (and no longer forward legacy `codex/event/*` notifications over stdio/websocket
// transports). CodexPotter parses a minimal subset of these payloads and maps them back into the
// legacy `EventMsg` stream that the workflow/TUI expects.

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HookEventName {
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    SessionStart,
    UserPromptSubmit,
    Stop,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HookHandlerType {
    Command,
    Prompt,
    Agent,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HookExecutionMode {
    Sync,
    Async,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HookScope {
    Thread,
    Turn,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HookRunStatus {
    Running,
    Completed,
    Failed,
    Blocked,
    Stopped,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HookOutputEntryKind {
    Warning,
    Stop,
    Feedback,
    Context,
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookOutputEntry {
    pub kind: HookOutputEntryKind,
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookRunSummary {
    pub id: String,
    pub event_name: HookEventName,
    pub handler_type: HookHandlerType,
    pub execution_mode: HookExecutionMode,
    pub scope: HookScope,
    pub source_path: PathBuf,
    pub display_order: i64,
    pub status: HookRunStatus,
    pub status_message: Option<String>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub entries: Vec<HookOutputEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedNotification {
    pub thread_id: String,
    pub turn: ThreadTurn,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HookStartedNotification {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub run: HookRunSummary,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    pub thread_id: String,
    pub turn: ThreadTurn,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HookCompletedNotification {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub run: HookRunSummary,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTurn {
    pub id: String,
    #[serde(default)]
    pub items: Vec<JsonValue>,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TurnError>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    pub message: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_upstream_codex_error_info_opt"
    )]
    pub codex_error_info: Option<CodexErrorInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_details: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    pub error: TurnError,
    pub will_retry: bool,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsageUpdatedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub token_usage: ThreadTokenUsage,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsage {
    pub total: TokenUsageBreakdown,
    pub last: TokenUsageBreakdown,
    pub model_context_window: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageDeltaNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlanDeltaNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnPlanUpdatedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub explanation: Option<String>,
    pub plan: Vec<TurnPlanStep>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnPlanStep {
    pub step: String,
    pub status: TurnPlanStepStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TurnPlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

impl From<CorePlanItemArg> for TurnPlanStep {
    fn from(value: CorePlanItemArg) -> Self {
        Self {
            step: value.step,
            status: value.status.into(),
        }
    }
}

impl From<CorePlanStepStatus> for TurnPlanStepStatus {
    fn from(value: CorePlanStepStatus) -> Self {
        match value {
            CorePlanStepStatus::Pending => Self::Pending,
            CorePlanStepStatus::InProgress => Self::InProgress,
            CorePlanStepStatus::Completed => Self::Completed,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningSummaryTextDeltaNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
    pub summary_index: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningSummaryPartAddedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub summary_index: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningTextDeltaNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
    pub content_index: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInteractionNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub process_id: String,
    pub stdin: String,
}

/// Notification payload for streaming output from a turn-scoped command execution item.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionOutputDeltaNotification {
    /// Thread that owns the command execution item.
    pub thread_id: String,
    /// Turn that owns the command execution item.
    pub turn_id: String,
    /// Command execution item that produced this output delta.
    pub item_id: String,
    /// Output text delta from the command execution stream.
    pub delta: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemStartedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item: JsonValue,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GuardianApprovalReviewStatus {
    InProgress,
    Approved,
    Denied,
    Aborted,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GuardianRiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GuardianApprovalReview {
    pub status: GuardianApprovalReviewStatus,
    #[serde(alias = "risk_score")]
    pub risk_score: Option<u8>,
    #[serde(alias = "risk_level")]
    pub risk_level: Option<GuardianRiskLevel>,
    pub rationale: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemGuardianApprovalReviewStartedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub target_item_id: String,
    pub review: GuardianApprovalReview,
    pub action: Option<JsonValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemGuardianApprovalReviewCompletedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub target_item_id: String,
    pub review: GuardianApprovalReview,
    pub action: Option<JsonValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemCompletedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item: JsonValue,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextCompactedNotification {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageThreadItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub phase: Option<MessagePhase>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningThreadItem {
    pub id: String,
    #[serde(default)]
    pub summary: Vec<String>,
    #[serde(default)]
    pub content: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeThreadItem {
    pub id: String,
    #[serde(default)]
    pub changes: Vec<FileUpdateChange>,
    pub status: PatchApplyStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
    pub diff: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PatchChangeKind {
    Add,
    Delete,
    Update { move_path: Option<PathBuf> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PatchApplyStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionThreadItem {
    pub id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub process_id: Option<String>,
    pub status: CommandExecutionStatus,
    #[serde(default)]
    pub command_actions: Vec<CommandAction>,
    pub aggregated_output: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextCompactionThreadItem {
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CommandAction {
    Read {
        command: String,
        name: String,
        path: PathBuf,
    },
    ListFiles {
        command: String,
        path: Option<String>,
    },
    Search {
        command: String,
        query: Option<String>,
        path: Option<String>,
    },
    Unknown {
        command: String,
    },
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::path::PathBuf;

    use codex_protocol::protocol::CodexErrorInfo;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::ApprovalsReviewer;
    use super::DynamicToolSpec;
    use super::GrantedPermissionProfile;
    use super::PermissionGrantScope;
    use super::PermissionsRequestApprovalParams;
    use super::PermissionsRequestApprovalResponse;
    use super::Thread;
    use super::ThreadResumeResponse;
    use super::ThreadStartResponse;
    use super::TurnError;
    use super::TurnStartParams;
    use super::TurnStatus;

    #[test]
    fn turn_error_handles_supported_and_unknown_upstream_codex_error_info_shapes() {
        struct Case {
            message: &'static str,
            codex_error_info: serde_json::Value,
            expected_info: Option<CodexErrorInfo>,
        }

        for case in [
            Case {
                message: "exceeded retry limit, last status: 429 Too Many Requests",
                codex_error_info: json!({
                    "responseTooManyFailedAttempts": {
                        "httpStatusCode": 429
                    }
                }),
                expected_info: Some(CodexErrorInfo::ResponseTooManyFailedAttempts {
                    http_status_code: Some(429),
                }),
            },
            Case {
                message: "server overloaded",
                codex_error_info: json!("serverOverloaded"),
                expected_info: Some(CodexErrorInfo::ServerOverloaded),
            },
            Case {
                message: "fatal error",
                codex_error_info: json!({
                    "brandNewProblem": {
                        "httpStatusCode": 503
                    }
                }),
                expected_info: None,
            },
        ] {
            let error: TurnError = serde_json::from_value(json!({
                "message": case.message,
                "codexErrorInfo": case.codex_error_info,
            }))
            .expect("deserialize turn error");

            assert_eq!(error.message, case.message);
            assert_eq!(error.codex_error_info, case.expected_info);
        }
    }

    #[test]
    fn dynamic_tool_spec_deserializes_legacy_expose_to_context() {
        let spec: DynamicToolSpec = serde_json::from_value(json!({
            "name": "lookup_ticket",
            "description": "Fetch a ticket",
            "inputSchema": {
                "type": "object"
            },
            "exposeToContext": false
        }))
        .expect("deserialize dynamic tool spec");

        assert_eq!(
            spec,
            DynamicToolSpec {
                name: "lookup_ticket".to_string(),
                description: "Fetch a ticket".to_string(),
                input_schema: json!({
                    "type": "object"
                }),
                defer_loading: true,
            }
        );
    }

    #[test]
    fn approvals_reviewer_accepts_current_and_legacy_auto_review_values() {
        let current: ApprovalsReviewer =
            serde_json::from_value(json!("auto_review")).expect("deserialize auto_review");
        let legacy: ApprovalsReviewer = serde_json::from_value(json!("guardian_subagent"))
            .expect("deserialize guardian_subagent");

        assert_eq!(current, ApprovalsReviewer::AutoReview);
        assert_eq!(legacy, ApprovalsReviewer::AutoReview);
        assert_eq!(
            serde_json::to_value(ApprovalsReviewer::AutoReview).expect("serialize reviewer"),
            json!("guardian_subagent")
        );
    }

    #[test]
    fn thread_start_response_deserializes_current_permission_profile_shape() {
        let response: ThreadStartResponse = serde_json::from_value(json!({
            "thread": {
                "id": "thread-1",
                "path": "/tmp/thread.jsonl"
            },
            "model": "gpt-5.4",
            "modelProvider": "openai",
            "serviceTier": null,
            "cwd": "/tmp/worktree",
            "instructionSources": ["/tmp/worktree/AGENTS.md"],
            "approvalPolicy": "never",
            "approvalsReviewer": "auto_review",
            "sandbox": {
                "type": "workspaceWrite",
                "writableRoots": ["/tmp/worktree"]
            },
            "permissionProfile": {
                "network": {
                    "enabled": true
                },
                "fileSystem": {
                    "entries": [
                        {
                            "path": {
                                "type": "path",
                                "path": "/tmp/worktree"
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "glob_pattern",
                                "pattern": "**/*.rs"
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": {
                                    "kind": "current_working_directory"
                                }
                            },
                            "access": "read"
                        }
                    ],
                    "globScanMaxDepth": 4
                }
            },
            "reasoningEffort": null
        }))
        .expect("deserialize thread/start response");

        assert_eq!(response.approvals_reviewer, ApprovalsReviewer::AutoReview);
        assert_eq!(response.instruction_sources.len(), 1);
        let permission_profile = response
            .permission_profile
            .expect("permission profile should deserialize");
        assert_eq!(
            permission_profile.network.expect("network").enabled,
            Some(true)
        );
        let file_system = permission_profile.file_system.expect("file system");
        assert_eq!(file_system.entries.len(), 3);
        assert_eq!(file_system.glob_scan_max_depth, NonZeroUsize::new(4));
    }

    #[test]
    fn thread_resume_response_deserializes_legacy_auto_review_spelling() {
        let response: ThreadResumeResponse = serde_json::from_value(json!({
            "thread": {
                "id": "thread-1"
            },
            "model": "gpt-5.4",
            "modelProvider": "openai",
            "serviceTier": null,
            "cwd": "/tmp/worktree",
            "instructionSources": [],
            "approvalPolicy": "never",
            "approvalsReviewer": "guardian_subagent",
            "sandbox": {
                "type": "readOnly"
            },
            "permissionProfile": null,
            "reasoningEffort": null
        }))
        .expect("deserialize thread/resume response");

        assert_eq!(response.approvals_reviewer, ApprovalsReviewer::AutoReview);
        assert_eq!(response.permission_profile, None);
    }

    #[test]
    fn thread_start_response_tolerates_default_service_tier() {
        let response: ThreadStartResponse = serde_json::from_value(json!({
            "thread": {
                "id": "thread-1"
            },
            "model": "gpt-5.4",
            "modelProvider": "openai",
            "serviceTier": "default",
            "cwd": "/tmp/worktree",
            "instructionSources": [],
            "approvalPolicy": "never",
            "approvalsReviewer": "user",
            "sandbox": {
                "type": "dangerFullAccess"
            },
            "permissionProfile": null,
            "reasoningEffort": "high",
            "activePermissionProfile": null,
            "runtimeWorkspaceRoots": ["/tmp/worktree"]
        }))
        .expect("deserialize thread/start response with default service tier");

        assert_eq!(response.service_tier, None);
    }

    #[test]
    fn turn_start_params_round_trip_permission_profile_and_environments() {
        let params: TurnStartParams = serde_json::from_value(json!({
            "threadId": "thread-1",
            "input": [],
            "environments": [
                {
                    "environmentId": "local",
                    "cwd": "/tmp/worktree"
                }
            ],
            "permissionProfile": {
                "fileSystem": {
                    "entries": [
                        {
                            "path": {
                                "type": "path",
                                "path": "/tmp/worktree"
                            },
                            "access": "write"
                        }
                    ]
                }
            }
        }))
        .expect("deserialize turn/start params");

        assert_eq!(
            params.environments.expect("environments")[0].environment_id,
            "local"
        );
        assert_eq!(
            params
                .permission_profile
                .expect("permission profile")
                .file_system
                .expect("file system")
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn permissions_request_preserves_file_system_entries_in_grant_response() {
        let params: PermissionsRequestApprovalParams = serde_json::from_value(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "permissions-1",
            "cwd": "/tmp/worktree",
            "reason": "Need to write generated files",
            "permissions": {
                "fileSystem": {
                    "entries": [
                        {
                            "path": {
                                "type": "path",
                                "path": "/tmp/worktree/generated"
                            },
                            "access": "write"
                        }
                    ],
                    "globScanMaxDepth": 3
                }
            }
        }))
        .expect("deserialize permissions request");

        let response = PermissionsRequestApprovalResponse {
            permissions: GrantedPermissionProfile::from(params.permissions),
            scope: PermissionGrantScope::Session,
            strict_auto_review: Some(false),
        };

        assert_eq!(
            serde_json::to_value(response).expect("serialize response"),
            json!({
                "permissions": {
                    "fileSystem": {
                        "read": null,
                        "write": null,
                        "globScanMaxDepth": 3,
                        "entries": [
                            {
                                "path": {
                                    "type": "path",
                                    "path": "/tmp/worktree/generated"
                                },
                                "access": "write"
                            }
                        ]
                    }
                },
                "scope": "session",
                "strictAutoReview": false
            })
        );
    }

    #[test]
    fn thread_deserializes_current_upstream_shape() {
        let thread: Thread = serde_json::from_value(json!({
            "id": "thread-1",
            "preview": "Investigate protocol drift",
            "ephemeral": false,
            "modelProvider": "openai",
            "createdAt": 1,
            "updatedAt": 2,
            "status": {
                "type": "active",
                "activeFlags": ["waitingOnApproval"]
            },
            "path": "/tmp/thread.jsonl",
            "cwd": "/tmp/worktree",
            "cliVersion": "0.116.0",
            "source": "cli",
            "agentNickname": "reviewer",
            "agentRole": "worker",
            "gitInfo": {
                "branch": "main"
            },
            "name": "Protocol drift",
            "turns": [{
                "id": "turn-1",
                "items": [{
                    "type": "message"
                }],
                "status": "completed",
                "error": null
            }]
        }))
        .expect("deserialize thread");

        assert_eq!(thread.id, "thread-1");
        assert_eq!(thread.preview, "Investigate protocol drift");
        assert_eq!(thread.path, Some(PathBuf::from("/tmp/thread.jsonl")));
        assert_eq!(thread.turns.len(), 1);
        assert_eq!(thread.turns[0].status, Some(TurnStatus::Completed));
    }
}
