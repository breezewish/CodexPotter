//! Upstream `codex app-server` backend driver.
//!
//! This module is the execution plane for CodexPotter rounds:
//!
//! - Spawns an external `codex app-server` process (one process per round).
//! - Drives the JSON-RPC request/response lifecycle (`thread/*`, `turn/start`, etc.).
//! - Translates upstream notifications into `codex_protocol::protocol::EventMsg`.
//! - Implements CodexPotter-specific stream recovery by injecting `PotterStreamRecovery*` markers
//!   and retrying with follow-up `Continue` turns when retryable transient errors occur.
//!
//! The backend emits a well-formed round boundary by synthesizing `EventMsg::PotterRoundFinished`,
//! and applies additional event filtering depending on [`AppServerEventMode`].

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::app_server::stream_recovery::ContinueRetryDecision;
use crate::app_server::stream_recovery::ContinueRetryPlan;
use crate::app_server::stream_recovery::PotterStreamRecovery;
use crate::app_server::upstream_protocol::AdditionalFileSystemPermissions as UpstreamAdditionalFileSystemPermissions;
use crate::app_server::upstream_protocol::AgentMessageDeltaNotification as UpstreamAgentMessageDeltaNotification;
use crate::app_server::upstream_protocol::AgentMessageThreadItem as UpstreamAgentMessageThreadItem;
use crate::app_server::upstream_protocol::ApplyPatchApprovalResponse;
use crate::app_server::upstream_protocol::ClientInfo;
use crate::app_server::upstream_protocol::ClientNotification;
use crate::app_server::upstream_protocol::ClientRequest;
use crate::app_server::upstream_protocol::ClientResponse;
use crate::app_server::upstream_protocol::CollaborationMode;
use crate::app_server::upstream_protocol::CollaborationModeKind;
use crate::app_server::upstream_protocol::CollaborationModeSettings;
use crate::app_server::upstream_protocol::CommandAction as UpstreamCommandAction;
use crate::app_server::upstream_protocol::CommandExecutionApprovalDecision;
use crate::app_server::upstream_protocol::CommandExecutionOutputDeltaNotification as UpstreamCommandExecutionOutputDeltaNotification;
use crate::app_server::upstream_protocol::CommandExecutionRequestApprovalResponse;
use crate::app_server::upstream_protocol::CommandExecutionThreadItem as UpstreamCommandExecutionThreadItem;
use crate::app_server::upstream_protocol::ContextCompactedNotification as UpstreamContextCompactedNotification;
use crate::app_server::upstream_protocol::ContextCompactionThreadItem as UpstreamContextCompactionThreadItem;
use crate::app_server::upstream_protocol::DynamicToolCallOutputContentItem;
use crate::app_server::upstream_protocol::DynamicToolCallResponse;
use crate::app_server::upstream_protocol::ErrorNotification as UpstreamErrorNotification;
use crate::app_server::upstream_protocol::ExecCommandApprovalResponse;
use crate::app_server::upstream_protocol::FileChangeApprovalDecision;
use crate::app_server::upstream_protocol::FileChangeRequestApprovalResponse;
use crate::app_server::upstream_protocol::FileChangeThreadItem as UpstreamFileChangeThreadItem;
use crate::app_server::upstream_protocol::FileSystemAccessMode as UpstreamFileSystemAccessMode;
use crate::app_server::upstream_protocol::FileSystemPath as UpstreamFileSystemPath;
use crate::app_server::upstream_protocol::FileUpdateChange as UpstreamFileUpdateChange;
use crate::app_server::upstream_protocol::GrantedPermissionProfile;
use crate::app_server::upstream_protocol::GuardianApprovalReviewStatus as UpstreamGuardianApprovalReviewStatus;
use crate::app_server::upstream_protocol::GuardianRiskLevel as UpstreamGuardianRiskLevel;
use crate::app_server::upstream_protocol::HookCompletedNotification as UpstreamHookCompletedNotification;
use crate::app_server::upstream_protocol::HookEventName as UpstreamHookEventName;
use crate::app_server::upstream_protocol::HookExecutionMode as UpstreamHookExecutionMode;
use crate::app_server::upstream_protocol::HookHandlerType as UpstreamHookHandlerType;
use crate::app_server::upstream_protocol::HookOutputEntry as UpstreamHookOutputEntry;
use crate::app_server::upstream_protocol::HookOutputEntryKind as UpstreamHookOutputEntryKind;
use crate::app_server::upstream_protocol::HookRunStatus as UpstreamHookRunStatus;
use crate::app_server::upstream_protocol::HookRunSummary as UpstreamHookRunSummary;
use crate::app_server::upstream_protocol::HookScope as UpstreamHookScope;
use crate::app_server::upstream_protocol::HookStartedNotification as UpstreamHookStartedNotification;
use crate::app_server::upstream_protocol::InitializeCapabilities;
use crate::app_server::upstream_protocol::InitializeParams;
use crate::app_server::upstream_protocol::ItemCompletedNotification as UpstreamItemCompletedNotification;
use crate::app_server::upstream_protocol::ItemGuardianApprovalReviewCompletedNotification as UpstreamItemGuardianApprovalReviewCompletedNotification;
use crate::app_server::upstream_protocol::ItemGuardianApprovalReviewStartedNotification as UpstreamItemGuardianApprovalReviewStartedNotification;
use crate::app_server::upstream_protocol::ItemStartedNotification as UpstreamItemStartedNotification;
use crate::app_server::upstream_protocol::JSONRPCError;
use crate::app_server::upstream_protocol::JSONRPCErrorError;
use crate::app_server::upstream_protocol::JSONRPCMessage;
use crate::app_server::upstream_protocol::JSONRPCNotification;
use crate::app_server::upstream_protocol::JSONRPCResponse;
use crate::app_server::upstream_protocol::McpServerElicitationAction;
use crate::app_server::upstream_protocol::McpServerElicitationRequestResponse;
use crate::app_server::upstream_protocol::PatchApplyStatus as UpstreamPatchApplyStatus;
use crate::app_server::upstream_protocol::PatchChangeKind as UpstreamPatchChangeKind;
use crate::app_server::upstream_protocol::PermissionGrantScope;
use crate::app_server::upstream_protocol::PermissionsRequestApprovalResponse;
use crate::app_server::upstream_protocol::PlanDeltaNotification as UpstreamPlanDeltaNotification;
use crate::app_server::upstream_protocol::ReasoningSummaryPartAddedNotification as UpstreamReasoningSummaryPartAddedNotification;
use crate::app_server::upstream_protocol::ReasoningSummaryTextDeltaNotification as UpstreamReasoningSummaryTextDeltaNotification;
use crate::app_server::upstream_protocol::ReasoningTextDeltaNotification as UpstreamReasoningTextDeltaNotification;
use crate::app_server::upstream_protocol::ReasoningThreadItem as UpstreamReasoningThreadItem;
use crate::app_server::upstream_protocol::RequestId;
use crate::app_server::upstream_protocol::ServerRequest;
use crate::app_server::upstream_protocol::TerminalInteractionNotification as UpstreamTerminalInteractionNotification;
use crate::app_server::upstream_protocol::ThreadResumeParams;
use crate::app_server::upstream_protocol::ThreadResumeResponse;
use crate::app_server::upstream_protocol::ThreadRollbackParams;
use crate::app_server::upstream_protocol::ThreadRollbackResponse;
use crate::app_server::upstream_protocol::ThreadStartParams;
use crate::app_server::upstream_protocol::ThreadStartResponse;
use crate::app_server::upstream_protocol::ThreadTokenUsageUpdatedNotification as UpstreamThreadTokenUsageUpdatedNotification;
use crate::app_server::upstream_protocol::ToolRequestUserInputResponse;
use crate::app_server::upstream_protocol::TurnCompletedNotification as UpstreamTurnCompletedNotification;
use crate::app_server::upstream_protocol::TurnPlanStepStatus as UpstreamTurnPlanStepStatus;
use crate::app_server::upstream_protocol::TurnPlanUpdatedNotification as UpstreamTurnPlanUpdatedNotification;
use crate::app_server::upstream_protocol::TurnStartParams;
use crate::app_server::upstream_protocol::TurnStartedNotification as UpstreamTurnStartedNotification;
use crate::app_server::upstream_protocol::TurnStatus as UpstreamTurnStatus;
use crate::app_server::upstream_protocol::UserInput as ApiUserInput;
use crate::app_server::upstream_protocol::upstream_response_service_tier_to_internal;
use anyhow::Context;
use codex_protocol::ThreadId;
use codex_protocol::approvals::ElicitationRequest as ProtocolElicitationRequest;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::approvals::GuardianAssessmentEvent;
use codex_protocol::approvals::GuardianAssessmentStatus;
use codex_protocol::approvals::GuardianRiskLevel as ProtocolGuardianRiskLevel;
use codex_protocol::mcp::RequestId as McpRequestId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::AgentMessageDeltaEvent;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AgentReasoningDeltaEvent;
use codex_protocol::protocol::AgentReasoningEvent;
use codex_protocol::protocol::AgentReasoningRawContentDeltaEvent;
use codex_protocol::protocol::AgentReasoningRawContentEvent;
use codex_protocol::protocol::AgentReasoningSectionBreakEvent;
use codex_protocol::protocol::ContextCompactedEvent;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecOutputStream;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookExecutionMode;
use codex_protocol::protocol::HookHandlerType;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_protocol::protocol::HookScope;
use codex_protocol::protocol::HookStartedEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::PatchApplyBeginEvent;
use codex_protocol::protocol::PatchApplyEndEvent;
use codex_protocol::protocol::PlanDeltaEvent;
use codex_protocol::protocol::PotterRoundOutcome;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ServiceTier;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::StreamErrorEvent;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::request_permissions::RequestPermissionProfile as ProtocolRequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsEvent;
use codex_protocol::request_user_input::RequestUserInputEvent;
use codex_protocol::request_user_input::RequestUserInputQuestion as ProtocolRequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputQuestionOption as ProtocolRequestUserInputQuestionOption;
use codex_protocol::user_input::UserInput as CodexUserInput;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStderr;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;

/// Upstream uses JSON-RPC's `INVALID_REQUEST_ERROR_CODE` when turn state preconditions fail
/// (e.g. interrupting a turn that already completed).
const JSONRPC_INVALID_REQUEST_ERROR_CODE: i64 = -32600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryAction {
    RetryContinue { attempt: u32 },
}

struct StreamRecoveryContext {
    stream_recovery: PotterStreamRecovery,
    recovery_action_tx: UnboundedSender<RecoveryAction>,
    pending_continue_retry: Option<ContinueRetryPlan>,
    active_turn_id: Option<String>,
    last_context_compaction_turn_id: Option<String>,
    has_sent_turn_start: bool,
    has_finished_round: bool,
    round_started_at: Instant,
    last_turn_start_was_recovery_continue: bool,
    event_mode: AppServerEventMode,
    bypass_approvals_and_sandbox: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppServerLaunchConfig {
    pub spawn_sandbox: Option<crate::app_server::upstream_protocol::SandboxMode>,
    pub thread_sandbox: Option<crate::app_server::upstream_protocol::SandboxMode>,
    pub bypass_approvals_and_sandbox: bool,
}

impl AppServerLaunchConfig {
    pub fn from_cli(sandbox: crate::CliSandbox, bypass: bool) -> Self {
        if bypass {
            return Self {
                spawn_sandbox: None,
                thread_sandbox: Some(
                    crate::app_server::upstream_protocol::SandboxMode::DangerFullAccess,
                ),
                bypass_approvals_and_sandbox: true,
            };
        }

        let mode = sandbox.as_protocol();
        Self {
            spawn_sandbox: mode,
            thread_sandbox: mode,
            bypass_approvals_and_sandbox: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppServerEventMode {
    /// Optimized for interactive rendering: suppresses UI-irrelevant events (for example rollback
    /// lifecycle notifications and empty turn completions during stream recovery).
    #[default]
    Interactive,
    /// Optimized for `exec --json`: forwards the raw event stream so the JSONL translator can
    /// enforce closure invariants (`turn.*` / `potter.round.*`) without depending on interactive
    /// suppression rules.
    ExecJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerBackendConfig {
    pub codex_bin: String,
    pub developer_instructions: Option<String>,
    pub launch: AppServerLaunchConfig,
    pub upstream_cli_args: crate::app_server::UpstreamCodexCliArgs,
    pub codex_home: Option<PathBuf>,
    pub thread_cwd: Option<PathBuf>,
    pub resume_thread_id: Option<ThreadId>,
    pub event_mode: AppServerEventMode,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpServerStartupStatusUpdatedNotification {
    name: String,
    status: String,
    error: Option<String>,
}

pub async fn run_app_server_backend(
    config: AppServerBackendConfig,
    mut op_rx: UnboundedReceiver<Op>,
    event_tx: UnboundedSender<Event>,
    _fatal_exit_tx: UnboundedSender<String>,
) -> anyhow::Result<()> {
    match run_app_server_backend_inner(config, &mut op_rx, &event_tx).await {
        Ok(()) => Ok(()),
        Err(err) => {
            let message = format!("Failed to run `codex app-server`: {err}");
            let _ = event_tx.send(Event {
                id: "".to_string(),
                msg: EventMsg::Error(ErrorEvent {
                    message: message.clone(),
                    codex_error_info: None,
                }),
            });
            let _ = event_tx.send(Event {
                id: "".to_string(),
                msg: EventMsg::PotterRoundFinished {
                    outcome: PotterRoundOutcome::TaskFailed { message },
                    duration_secs: 0,
                },
            });

            // Surface backend failures via the UI and exit reason, instead of bubbling up an
            // additional anyhow error that would get printed after the TUI exits.
            Ok(())
        }
    }
}

async fn run_app_server_backend_inner(
    config: AppServerBackendConfig,
    op_rx: &mut UnboundedReceiver<Op>,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let AppServerBackendConfig {
        codex_bin,
        developer_instructions,
        launch,
        upstream_cli_args,
        codex_home,
        thread_cwd,
        resume_thread_id,
        event_mode,
    } = config;

    let client_info = resolve_upstream_client_info(&codex_bin).await;
    let (mut child, stdin, stdout, stderr) = spawn_app_server(
        &codex_bin,
        launch,
        &upstream_cli_args,
        codex_home.as_deref(),
    )
    .await?;
    let stderr_capture = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_truncated = Arc::new(AtomicBool::new(false));
    let stderr_task = {
        let stderr_capture = stderr_capture.clone();
        let stderr_truncated = stderr_truncated.clone();
        tokio::spawn(async move {
            const LIMIT_BYTES: usize = 32 * 1024;
            let mut stderr = stderr;
            let mut buf = [0u8; 4096];

            loop {
                let n = stderr.read(&mut buf).await?;
                if n == 0 {
                    break;
                }

                let mut capture = match stderr_capture.lock() {
                    Ok(guard) => guard,
                    Err(err) => err.into_inner(),
                };
                let remaining = LIMIT_BYTES.saturating_sub(capture.len());
                if remaining == 0 {
                    stderr_truncated.store(true, Ordering::Relaxed);
                    continue;
                }

                let take = remaining.min(n);
                capture.extend_from_slice(&buf[..take]);
                if take < n {
                    stderr_truncated.store(true, Ordering::Relaxed);
                }
            }

            Ok::<(), std::io::Error>(())
        })
    };

    let mut stdin = Some(stdin);
    let mut lines = BufReader::new(stdout).lines();
    let mut next_id: i64 = 1;
    let mut shutdown_requested = false;
    let (recovery_action_tx, mut recovery_action_rx) = unbounded_channel::<RecoveryAction>();
    let mut recovery = StreamRecoveryContext {
        stream_recovery: PotterStreamRecovery::new(),
        recovery_action_tx,
        pending_continue_retry: None,
        active_turn_id: None,
        last_context_compaction_turn_id: None,
        has_sent_turn_start: false,
        has_finished_round: false,
        round_started_at: Instant::now(),
        last_turn_start_was_recovery_continue: false,
        event_mode,
        bypass_approvals_and_sandbox: launch.bypass_approvals_and_sandbox,
    };

    let result = async {
        initialize_app_server(
            stdin
                .as_mut()
                .context("codex app-server stdin unavailable")?,
            &mut lines,
            &mut next_id,
            client_info,
            &mut recovery,
            event_tx,
        )
        .await?;

        let thread_start_or_resume = match resume_thread_id {
            Some(thread_id) => ThreadStartOrResume::Resume(
                thread_resume(
                    stdin
                        .as_mut()
                        .context("codex app-server stdin unavailable")?,
                    &mut lines,
                    &mut next_id,
                    ThreadResumeSettings {
                        thread_id,
                        model: upstream_cli_args.model.clone(),
                        developer_instructions,
                        sandbox_mode: launch.thread_sandbox,
                        cwd: thread_cwd,
                    },
                    &mut recovery,
                    event_tx,
                )
                .await?,
            ),
            None => ThreadStartOrResume::Start(
                thread_start(
                    stdin
                        .as_mut()
                        .context("codex app-server stdin unavailable")?,
                    &mut lines,
                    &mut next_id,
                    ThreadStartSettings {
                        model: upstream_cli_args.model.clone(),
                        developer_instructions,
                        sandbox_mode: launch.thread_sandbox,
                        cwd: thread_cwd,
                    },
                    &mut recovery,
                    event_tx,
                )
                .await?,
            ),
        };

        let thread_id = thread_start_or_resume.thread_id().to_string();
        let session_model = thread_start_or_resume.model().to_string();
        let session_reasoning_effort = thread_start_or_resume.reasoning_effort();
        let turn_request = TurnRequestContext {
            thread_id: thread_id.as_str(),
            model: session_model.as_str(),
            reasoning_effort: session_reasoning_effort,
        };

        let session_configured = synthesize_session_configured(&thread_start_or_resume)?;
        let _ = event_tx.send(Event {
            id: "".to_string(),
            msg: EventMsg::SessionConfigured(session_configured),
        });

        loop {
            tokio::select! {
                maybe_op = op_rx.recv(), if !shutdown_requested => {
                    let Some(op) = maybe_op else {
                        shutdown_requested = true;
                        stdin.take();
                        continue;
                    };
                    if matches!(op, Op::UserInput { .. }) {
                        let was_in_retry_streak = recovery.stream_recovery.is_in_retry_streak();
                        recovery.has_sent_turn_start = true;
                        recovery.last_turn_start_was_recovery_continue = false;
                        recovery.pending_continue_retry = None;
                        recovery.stream_recovery = PotterStreamRecovery::new();
                        if was_in_retry_streak {
                            let _ = event_tx.send(Event {
                                id: "".to_string(),
                                msg: EventMsg::PotterStreamRecoveryRecovered,
                            });
                        }
                    }
                    let mut io = AppServerIo {
                        stdin: stdin.as_mut().context("codex app-server stdin unavailable")?,
                        lines: &mut lines,
                        next_id: &mut next_id,
                        recovery: &mut recovery,
                        event_tx,
                        op_rx,
                    };
                    handle_op(turn_request, op, &mut io).await?;
                }
                maybe_action = recovery_action_rx.recv(), if !shutdown_requested => {
                    let Some(action) = maybe_action else {
                        continue;
                    };

                    if !recovery.stream_recovery.is_in_retry_streak() {
                        continue;
                    }

                    match action {
                        RecoveryAction::RetryContinue { attempt } => {
                            recovery.has_sent_turn_start = true;
                            if attempt >= 2 && recovery.last_turn_start_was_recovery_continue {
                                // Remove the previous automatic `Continue` turn from the thread
                                // history so retries do not accumulate redundant `Continue`
                                // prompts in the model context.
                                //
                                // This is expected to succeed because stream recovery retries are
                                // scheduled only after the previous turn has ended.
                                thread_rollback(
                                    &thread_id,
                                    stdin.as_mut().context("codex app-server stdin unavailable")?,
                                    &mut lines,
                                    &mut next_id,
                                    &mut recovery,
                                    event_tx,
                                )
                                .await?;
                            }
                            recovery.last_turn_start_was_recovery_continue = true;
                            let mut io = AppServerIo {
                                stdin: stdin.as_mut().context("codex app-server stdin unavailable")?,
                                lines: &mut lines,
                                next_id: &mut next_id,
                                recovery: &mut recovery,
                                event_tx,
                                op_rx,
                            };
                            handle_op(
                                turn_request,
                                Op::UserInput {
                                    items: vec![CodexUserInput::Text {
                                        text: String::from("Continue"),
                                        text_elements: Vec::new(),
                                    }],
                                    final_output_json_schema: None,
                                },
                                &mut io,
                            )
                            .await?;
                        }
                    }
                }
                maybe_line = lines.next_line() => {
                    let Some(line) = maybe_line? else {
                        break;
                    };
                    let msg: JSONRPCMessage = serde_json::from_str(&line)
                        .with_context(|| format!("failed to decode app-server message: {line}"))?;
                    handle_app_server_message(
                        msg,
                        &mut stdin,
                        &mut recovery,
                        event_tx,
                    )
                    .await?;
                }
            }
        }

        let _ = child.wait().await;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if result.is_err() {
        // Do not await the drain task on failure: the child might keep running and we'd hang while
        // waiting for stderr to close. We already captured enough to provide context.
        stderr_task.abort();
    } else {
        let _ = stderr_task.await;
    }

    let result = result.map_err(|err| {
        let stderr = {
            let capture = match stderr_capture.lock() {
                Ok(guard) => guard,
                Err(err) => err.into_inner(),
            };
            String::from_utf8_lossy(&capture).to_string()
        };

        let stderr = stderr.trim_end_matches(['\n', '\r']).to_string();
        if stderr.is_empty() {
            return err;
        }

        let mut message = String::new();
        message.push_str(&err.to_string());
        message.push_str("\n\n");
        message.push_str("app-server stderr:");
        message.push('\n');
        message.push_str(&stderr);
        if stderr_truncated.load(Ordering::Relaxed) {
            message.push('\n');
            message.push_str("[stderr truncated]");
        }
        anyhow::Error::msg(message)
    });

    match result {
        Ok(()) => {}
        Err(err) => {
            // Avoid turning shutdown errors into a second round failure: once we emitted the
            // round outcome, the UI is already leaving the round renderer.
            if recovery.has_finished_round {
                return Ok(());
            }
            return Err(err);
        }
    }

    // If the backend finishes while the UI still expects it to be alive, ensure the UI can exit.
    if !shutdown_requested && !recovery.has_finished_round {
        anyhow::bail!("codex app-server exited unexpectedly");
    }

    Ok(())
}

async fn spawn_app_server(
    codex_bin: &str,
    launch: AppServerLaunchConfig,
    upstream_cli_args: &crate::app_server::UpstreamCodexCliArgs,
    codex_home: Option<&Path>,
) -> anyhow::Result<(Child, ChildStdin, ChildStdout, ChildStderr)> {
    let mut cmd = Command::new(codex_bin);
    cmd.kill_on_drop(true);

    if let Some(codex_home) = codex_home {
        cmd.env("CODEX_HOME", codex_home);
    }

    for arg in upstream_cli_args.to_upstream_codex_args() {
        cmd.arg(arg);
    }

    if launch.bypass_approvals_and_sandbox {
        cmd.arg("--dangerously-bypass-approvals-and-sandbox");
    }
    if let Some(mode) = launch.spawn_sandbox {
        cmd.arg("--sandbox");
        cmd.arg(super::sandbox_mode_cli_arg(mode));
    }

    cmd.arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut attempts = 0usize;
    let mut child = loop {
        match cmd.spawn() {
            Ok(child) => break child,
            Err(err) if err.kind() == ErrorKind::ExecutableFileBusy && attempts < 5 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to start `{codex_bin}` app-server"));
            }
        }
    };

    let stdin = child
        .stdin
        .take()
        .context("codex app-server stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("codex app-server stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("codex app-server stderr unavailable")?;
    Ok((child, stdin, stdout, stderr))
}

async fn initialize_app_server(
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: &mut i64,
    client_info: ClientInfo,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let request_id = next_request_id(next_id);
    let request = ClientRequest::Initialize {
        request_id: request_id.clone(),
        params: InitializeParams {
            client_info,
            capabilities: Some(InitializeCapabilities {
                experimental_api: true,
                opt_out_notification_methods: None,
            }),
        },
    };
    let _response = send_client_request(stdin, lines, request, recovery, event_tx).await?;

    send_message(stdin, &ClientNotification::Initialized).await?;
    Ok(())
}

/// Cache successfully resolved upstream client identity per configured `codex_bin` so multi-round
/// sessions do not pay an extra `codex --version` spawn on every round.
fn upstream_client_info_cache() -> &'static Mutex<HashMap<String, ClientInfo>> {
    static CACHE: OnceLock<Mutex<HashMap<String, ClientInfo>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_upstream_client_info_cache() -> std::sync::MutexGuard<'static, HashMap<String, ClientInfo>>
{
    match upstream_client_info_cache().lock() {
        Ok(guard) => guard,
        Err(err) => {
            tracing::warn!(
                "Upstream client info cache mutex poisoned; continuing with inner state."
            );
            err.into_inner()
        }
    }
}

async fn resolve_upstream_client_info(codex_bin: &str) -> ClientInfo {
    if let Some(client_info) = lock_upstream_client_info_cache().get(codex_bin).cloned() {
        return client_info;
    }

    let client_info = match read_codex_cli_version(codex_bin).await {
        Ok(version) => ClientInfo {
            name: "codex-tui".to_string(),
            title: None,
            version,
        },
        Err(err) => {
            tracing::warn!(
                error = %err,
                codex_bin,
                "Failed to resolve upstream codex version; falling back to CodexPotter build version."
            );
            return ClientInfo {
                name: "codex-tui".to_string(),
                title: None,
                version: codex_tui::CODEX_POTTER_VERSION.to_string(),
            };
        }
    };
    lock_upstream_client_info_cache().insert(codex_bin.to_string(), client_info.clone());
    client_info
}

async fn read_codex_cli_version(codex_bin: &str) -> anyhow::Result<String> {
    const MAX_ATTEMPTS: usize = 3;

    let mut attempt = 1;
    let output = loop {
        match Command::new(codex_bin).arg("--version").output().await {
            Ok(output) => break output,
            Err(err) if err.kind() == ErrorKind::ExecutableFileBusy => {
                if attempt == MAX_ATTEMPTS {
                    return Err(err).with_context(|| format!("run `{codex_bin} --version`"));
                }
                // Freshly replaced executables can briefly return ETXTBSY on Linux.
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("run `{codex_bin} --version`"));
            }
        }
    };

    anyhow::ensure!(
        output.status.success(),
        "`{codex_bin} --version` failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );

    parse_codex_cli_version(&String::from_utf8_lossy(&output.stdout))
}

fn parse_codex_cli_version(output: &str) -> anyhow::Result<String> {
    let trimmed = output.trim();
    anyhow::ensure!(!trimmed.is_empty(), "empty stdout from `codex --version`");

    for token in trimmed.split_whitespace() {
        let token = token.trim_matches(|ch: char| matches!(ch, ',' | ';' | ')' | '('));
        let token = token.strip_prefix('v').unwrap_or(token);
        if looks_like_semver(token) {
            return Ok(token.to_string());
        }
    }

    anyhow::bail!("failed to find a version token in `codex --version` output: {trimmed:?}")
}

fn looks_like_semver(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut idx = 0usize;

    let start = idx;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == start || idx >= bytes.len() || bytes[idx] != b'.' {
        return false;
    }
    idx += 1;

    let start = idx;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == start || idx >= bytes.len() || bytes[idx] != b'.' {
        return false;
    }
    idx += 1;

    let start = idx;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == start {
        return false;
    }
    if idx == bytes.len() {
        return true;
    }

    match bytes[idx] {
        b'-' | b'+' => {}
        _ => return false,
    }

    bytes[idx + 1..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

async fn send_client_request(
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    request: ClientRequest,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<ClientResponse> {
    let request_id = request.id().clone();
    let method = request.method();
    send_message(stdin, &request).await?;
    let response = read_until_response(stdin, lines, request_id, recovery, event_tx).await?;
    let response_summary = summarize_response_result(&response.result);
    request
        .decode_response(response)
        .with_context(|| format!("decode {method} response (payload: {response_summary})"))
}

fn summarize_response_result(result: &serde_json::Value) -> String {
    const MAX_CHARS: usize = 400;

    let rendered = serde_json::to_string(result)
        .unwrap_or_else(|err| format!("<failed to serialize response payload: {err}>"));
    let mut chars = rendered.chars();
    let preview = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...[truncated]")
    } else {
        preview
    }
}

struct ThreadStartSettings {
    model: Option<String>,
    developer_instructions: Option<String>,
    sandbox_mode: Option<crate::app_server::upstream_protocol::SandboxMode>,
    cwd: Option<PathBuf>,
}

struct ThreadResumeSettings {
    thread_id: ThreadId,
    model: Option<String>,
    developer_instructions: Option<String>,
    sandbox_mode: Option<crate::app_server::upstream_protocol::SandboxMode>,
    cwd: Option<PathBuf>,
}

impl ThreadStartSettings {
    fn into_params(self) -> ThreadStartParams {
        ThreadStartParams {
            model: self.model,
            model_provider: None,
            service_tier: None,
            cwd: self.cwd.map(|cwd| cwd.to_string_lossy().to_string()),
            approval_policy: Some(crate::app_server::upstream_protocol::AskForApproval::Never),
            approvals_reviewer: None,
            sandbox: self.sandbox_mode,
            permission_profile: None,
            config: None,
            service_name: None,
            base_instructions: None,
            developer_instructions: self.developer_instructions,
            personality: None,
            ephemeral: None,
            environments: None,
            dynamic_tools: None,
            mock_experimental_field: None,
            experimental_raw_events: false,
            persist_extended_history: false,
        }
    }
}

impl ThreadResumeSettings {
    fn into_params(self) -> ThreadResumeParams {
        ThreadResumeParams {
            thread_id: self.thread_id.to_string(),
            history: None,
            path: None,
            model: self.model,
            model_provider: None,
            service_tier: None,
            cwd: self.cwd.map(|cwd| cwd.to_string_lossy().to_string()),
            approval_policy: Some(crate::app_server::upstream_protocol::AskForApproval::Never),
            approvals_reviewer: None,
            sandbox: self.sandbox_mode,
            permission_profile: None,
            config: None,
            base_instructions: None,
            developer_instructions: self.developer_instructions,
            personality: None,
            persist_extended_history: false,
        }
    }
}

async fn thread_start(
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: &mut i64,
    settings: ThreadStartSettings,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<ThreadStartResponse> {
    let request = ClientRequest::ThreadStart {
        request_id: next_request_id(next_id),
        params: settings.into_params(),
    };
    let response = send_client_request(stdin, lines, request, recovery, event_tx).await?;
    let ClientResponse::ThreadStart { response, .. } = response else {
        unreachable!("thread/start must decode into thread/start response");
    };
    Ok(response)
}

async fn thread_resume(
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: &mut i64,
    settings: ThreadResumeSettings,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<ThreadResumeResponse> {
    let request = ClientRequest::ThreadResume {
        request_id: next_request_id(next_id),
        params: settings.into_params(),
    };
    let response = send_client_request(stdin, lines, request, recovery, event_tx).await?;
    let ClientResponse::ThreadResume { response, .. } = response else {
        unreachable!("thread/resume must decode into thread/resume response");
    };
    Ok(response)
}

async fn thread_rollback(
    thread_id: &str,
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: &mut i64,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let request = ClientRequest::ThreadRollback {
        request_id: next_request_id(next_id),
        params: ThreadRollbackParams {
            thread_id: thread_id.to_string(),
            num_turns: 1,
        },
    };
    let response = send_client_request(stdin, lines, request, recovery, event_tx)
        .await
        .with_context(|| format!("thread/rollback thread_id={thread_id}"))?;
    let ClientResponse::ThreadRollback { response, .. } = response else {
        unreachable!("thread/rollback must decode into thread/rollback response");
    };
    let _parsed: ThreadRollbackResponse = response;
    Ok(())
}

fn default_collaboration_mode(
    model: &str,
    reasoning_effort: Option<ReasoningEffort>,
) -> CollaborationMode {
    CollaborationMode {
        mode: CollaborationModeKind::Default,
        settings: CollaborationModeSettings {
            model: model.to_string(),
            reasoning_effort,
            developer_instructions: None,
        },
    }
}

#[derive(Clone, Copy)]
struct TurnRequestContext<'a> {
    thread_id: &'a str,
    model: &'a str,
    reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Default)]
struct PendingTurnStartInterrupt {
    requested: bool,
    sent: bool,
}

struct AppServerIo<'a> {
    stdin: &'a mut ChildStdin,
    lines: &'a mut tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: &'a mut i64,
    recovery: &'a mut StreamRecoveryContext,
    event_tx: &'a UnboundedSender<Event>,
    op_rx: &'a mut UnboundedReceiver<Op>,
}

async fn handle_op(
    ctx: TurnRequestContext<'_>,
    op: Op,
    io: &mut AppServerIo<'_>,
) -> anyhow::Result<()> {
    match op {
        Op::UserInput {
            items,
            final_output_json_schema,
        } => {
            let input = items.into_iter().map(ApiUserInput::from).collect();
            let request = ClientRequest::TurnStart {
                request_id: next_request_id(io.next_id),
                params: TurnStartParams {
                    thread_id: ctx.thread_id.to_string(),
                    input,
                    environments: None,
                    cwd: None,
                    approval_policy: None,
                    approvals_reviewer: None,
                    sandbox_policy: None,
                    permission_profile: None,
                    model: None,
                    service_tier: None,
                    effort: None,
                    summary: None,
                    personality: None,
                    output_schema: final_output_json_schema,
                    collaboration_mode: Some(default_collaboration_mode(
                        ctx.model,
                        ctx.reasoning_effort,
                    )),
                },
            };
            let request_id = request.id().clone();
            let method = request.method();
            send_message(io.stdin, &request).await?;
            let mut pending_interrupt = PendingTurnStartInterrupt::default();
            let response = match read_turn_start_response_or_error(
                ctx,
                &request_id,
                io,
                &mut pending_interrupt,
            )
            .await?
            {
                Ok(response) => response,
                Err(error) => {
                    anyhow::bail!("app-server returned error for {request_id:?}: {error:?}");
                }
            };
            let response_summary = summarize_response_result(&response.result);
            let response = request.decode_response(response).with_context(|| {
                format!("decode {method} response (payload: {response_summary})")
            })?;
            let ClientResponse::TurnStart {
                response: parsed, ..
            } = response
            else {
                unreachable!("turn/start must decode into turn/start response");
            };
            io.recovery.active_turn_id = Some(parsed.turn.id);
            if pending_interrupt.requested && !pending_interrupt.sent {
                send_turn_interrupt_for_current_state(ctx, io).await?;
            }
            Ok(())
        }
        Op::CleanBackgroundTerminals => {
            let request = build_background_terminals_clean_request(io.next_id, ctx.thread_id);
            let request_id = request.id().clone();
            send_message(io.stdin, &request).await?;

            match read_until_response_or_error(
                io.stdin,
                io.lines,
                &request_id,
                io.recovery,
                io.event_tx,
            )
            .await?
            {
                Ok(response) => {
                    let response = request
                        .decode_response(response)
                        .context("decode thread/backgroundTerminals/clean response")?;
                    let ClientResponse::ThreadBackgroundTerminalsClean {
                        response: _parsed, ..
                    } = response
                    else {
                        unreachable!(
                            "thread/backgroundTerminals/clean must decode into its response type"
                        );
                    };
                    Ok(())
                }
                Err(error) if error.code == JSONRPC_INVALID_REQUEST_ERROR_CODE => Ok(()),
                Err(error) => {
                    anyhow::bail!("app-server returned error for {request_id:?}: {error:?}");
                }
            }
        }
        Op::Interrupt => {
            let Some(turn_id) = io.recovery.active_turn_id.as_deref() else {
                return Ok(());
            };
            let request = build_turn_interrupt_request(io.next_id, ctx.thread_id, turn_id);
            let request_id = request.id().clone();
            send_message(io.stdin, &request).await?;

            match read_until_response_or_error(
                io.stdin,
                io.lines,
                &request_id,
                io.recovery,
                io.event_tx,
            )
            .await?
            {
                Ok(response) => {
                    let response = request
                        .decode_response(response)
                        .context("decode turn/interrupt response")?;
                    let ClientResponse::TurnInterrupt {
                        response: _parsed, ..
                    } = response
                    else {
                        unreachable!("turn/interrupt must decode into turn/interrupt response");
                    };
                    Ok(())
                }
                Err(error) if error.code == JSONRPC_INVALID_REQUEST_ERROR_CODE => Ok(()),
                Err(error) => {
                    anyhow::bail!("app-server returned error for {request_id:?}: {error:?}");
                }
            }
        }
        Op::GetHistoryEntryRequest { .. } => {
            // The prompt screen does not support fetching persisted prompt history from the
            // backend. Ignore the request so the UI can stay simple.
            Ok(())
        }
    }
}

async fn read_turn_start_response_or_error(
    ctx: TurnRequestContext<'_>,
    request_id: &RequestId,
    io: &mut AppServerIo<'_>,
    pending_interrupt: &mut PendingTurnStartInterrupt,
) -> anyhow::Result<Result<JSONRPCResponse, JSONRPCErrorError>> {
    let mut op_rx_closed = false;

    loop {
        tokio::select! {
                maybe_op = io.op_rx.recv(), if !op_rx_closed => {
                    match maybe_op {
                        Some(Op::CleanBackgroundTerminals) => {
                            let request = build_background_terminals_clean_request(
                                io.next_id,
                                ctx.thread_id,
                            );
                            send_message(io.stdin, &request).await?;
                        }
                        Some(Op::Interrupt) => {
                            pending_interrupt.requested = true;
                        }
                        Some(Op::UserInput { .. }) | Some(Op::GetHistoryEntryRequest { .. }) => {}
                        None => {
                        op_rx_closed = true;
                    }
                }
            }
            maybe_line = io.lines.next_line() => {
                let Some(line) = maybe_line? else {
                    anyhow::bail!(
                        "app-server stdout closed while waiting for response {request_id:?}"
                    );
                };
                let msg: JSONRPCMessage = serde_json::from_str(&line)
                    .with_context(|| format!("decode json-rpc: {line}"))?;

                match msg {
                    JSONRPCMessage::Response(response) if &response.id == request_id => {
                        return Ok(Ok(response));
                    }
                    JSONRPCMessage::Error(err) if &err.id == request_id => {
                        return Ok(Err(err.error));
                    }
                    JSONRPCMessage::Notification(notification) => {
                        if notification.method.starts_with("codex/event/") {
                            handle_codex_event_notification(
                                &notification.method,
                                notification.params,
                                io.recovery,
                                io.event_tx,
                            )?;
                        } else {
                            handle_typed_notification(notification, io.recovery, io.event_tx)?;
                        }
                    }
                    JSONRPCMessage::Request(request) => {
                        handle_server_request(io.stdin, request, io.recovery, io.event_tx).await?;
                    }
                    _ => {}
                }
            }
        }

        if pending_interrupt.requested && !pending_interrupt.sent {
            send_turn_interrupt_for_current_state(ctx, io).await?;
            pending_interrupt.sent = true;
        }
    }
}

async fn send_turn_interrupt_for_current_state(
    ctx: TurnRequestContext<'_>,
    io: &mut AppServerIo<'_>,
) -> anyhow::Result<()> {
    let turn_id = io.recovery.active_turn_id.as_deref().unwrap_or("");
    let request = build_turn_interrupt_request(io.next_id, ctx.thread_id, turn_id);
    send_message(io.stdin, &request).await?;
    Ok(())
}

fn build_background_terminals_clean_request(next_id: &mut i64, thread_id: &str) -> ClientRequest {
    ClientRequest::ThreadBackgroundTerminalsClean {
        request_id: next_request_id(next_id),
        params: crate::app_server::upstream_protocol::ThreadBackgroundTerminalsCleanParams {
            thread_id: thread_id.to_string(),
        },
    }
}

fn build_turn_interrupt_request(
    next_id: &mut i64,
    thread_id: &str,
    turn_id: &str,
) -> ClientRequest {
    ClientRequest::TurnInterrupt {
        request_id: next_request_id(next_id),
        params: crate::app_server::upstream_protocol::TurnInterruptParams {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
        },
    }
}

async fn handle_app_server_message(
    msg: JSONRPCMessage,
    stdin: &mut Option<ChildStdin>,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    match msg {
        JSONRPCMessage::Notification(notification) => {
            if notification.method.starts_with("codex/event/") {
                handle_codex_event_notification(
                    &notification.method,
                    notification.params,
                    recovery,
                    event_tx,
                )?;
            } else {
                handle_typed_notification(notification, recovery, event_tx)?;
            }
        }
        JSONRPCMessage::Request(request) => {
            if let Some(stdin) = stdin.as_mut() {
                handle_server_request(stdin, request, recovery, event_tx).await?;
            }
        }
        JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {}
    }

    Ok(())
}

fn handle_typed_notification(
    notification: JSONRPCNotification,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let JSONRPCNotification { method, params } = notification;
    let Some(params) = params else {
        return Ok(());
    };

    match method.as_str() {
        "turn/started" => {
            let ev: UpstreamTurnStartedNotification =
                serde_json::from_value(params).context("decode turn/started notification")?;
            let turn_id = ev.turn.id;
            handle_codex_event(
                Event {
                    id: turn_id.clone(),
                    msg: EventMsg::TurnStarted(TurnStartedEvent {
                        turn_id,
                        model_context_window: None,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "turn/completed" => {
            let ev: UpstreamTurnCompletedNotification =
                serde_json::from_value(params).context("decode turn/completed notification")?;
            let turn_id = ev.turn.id;
            match ev.turn.status {
                UpstreamTurnStatus::Completed => {
                    handle_codex_event(
                        Event {
                            id: turn_id.clone(),
                            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                turn_id,
                                last_agent_message: None,
                            }),
                        },
                        recovery,
                        event_tx,
                    );
                }
                UpstreamTurnStatus::Interrupted => {
                    handle_codex_event(
                        Event {
                            id: turn_id.clone(),
                            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                                turn_id: Some(turn_id),
                                reason: TurnAbortReason::Interrupted,
                            }),
                        },
                        recovery,
                        event_tx,
                    );
                }
                UpstreamTurnStatus::Failed => {
                    // Newer upstream transports represent turn failures via `turn/completed` with
                    // a `Failed` status. CodexPotter's stream recovery needs an error signal plus
                    // a follow-up empty TurnComplete to schedule the retrying `Continue` turn.
                    if recovery.pending_continue_retry.is_some() {
                        handle_codex_event(
                            Event {
                                id: turn_id.clone(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    turn_id,
                                    last_agent_message: None,
                                }),
                            },
                            recovery,
                            event_tx,
                        );
                        return Ok(());
                    }

                    let message = ev
                        .turn
                        .error
                        .as_ref()
                        .map(|error| error.message.clone())
                        .unwrap_or_else(|| "turn failed".to_string());
                    let codex_error_info = ev
                        .turn
                        .error
                        .as_ref()
                        .and_then(|error| error.codex_error_info.clone());
                    let error_event = ErrorEvent {
                        message,
                        codex_error_info,
                    };
                    let retryable =
                        codex_protocol::potter_stream_recovery::is_retryable_stream_error(
                            &error_event,
                        );

                    handle_codex_event(
                        Event {
                            id: turn_id.clone(),
                            msg: EventMsg::Error(error_event),
                        },
                        recovery,
                        event_tx,
                    );

                    if retryable {
                        handle_codex_event(
                            Event {
                                id: turn_id.clone(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    turn_id,
                                    last_agent_message: None,
                                }),
                            },
                            recovery,
                            event_tx,
                        );
                    }
                }
                UpstreamTurnStatus::InProgress => {}
            }
        }
        "hook/started" => {
            let ev: UpstreamHookStartedNotification =
                serde_json::from_value(params).context("decode hook/started notification")?;
            let run = hook_run_summary_from_upstream(ev.run);
            handle_codex_event(
                Event {
                    id: run.id.clone(),
                    msg: EventMsg::HookStarted(HookStartedEvent {
                        turn_id: ev.turn_id,
                        run,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "hook/completed" => {
            let ev: UpstreamHookCompletedNotification =
                serde_json::from_value(params).context("decode hook/completed notification")?;
            let run = hook_run_summary_from_upstream(ev.run);
            handle_codex_event(
                Event {
                    id: run.id.clone(),
                    msg: EventMsg::HookCompleted(HookCompletedEvent {
                        turn_id: ev.turn_id,
                        run,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "thread/tokenUsage/updated" => {
            let ev: UpstreamThreadTokenUsageUpdatedNotification = serde_json::from_value(params)
                .context("decode thread/tokenUsage/updated notification")?;
            let info = TokenUsageInfo {
                total_token_usage: TokenUsage {
                    input_tokens: ev.token_usage.total.input_tokens,
                    cached_input_tokens: ev.token_usage.total.cached_input_tokens,
                    output_tokens: ev.token_usage.total.output_tokens,
                    reasoning_output_tokens: ev.token_usage.total.reasoning_output_tokens,
                    total_tokens: ev.token_usage.total.total_tokens,
                },
                last_token_usage: TokenUsage {
                    input_tokens: ev.token_usage.last.input_tokens,
                    cached_input_tokens: ev.token_usage.last.cached_input_tokens,
                    output_tokens: ev.token_usage.last.output_tokens,
                    reasoning_output_tokens: ev.token_usage.last.reasoning_output_tokens,
                    total_tokens: ev.token_usage.last.total_tokens,
                },
                model_context_window: ev.token_usage.model_context_window,
            };
            handle_codex_event(
                Event {
                    id: ev.turn_id,
                    msg: EventMsg::TokenCount(TokenCountEvent {
                        info: Some(info),
                        rate_limits: None,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "thread/compacted" => {
            let ev: UpstreamContextCompactedNotification =
                serde_json::from_value(params).context("decode thread/compacted notification")?;
            emit_context_compacted_event(ev.turn_id.clone(), ev.turn_id, recovery, event_tx);
        }
        "turn/plan/updated" => {
            let ev: UpstreamTurnPlanUpdatedNotification =
                serde_json::from_value(params).context("decode turn/plan/updated notification")?;
            handle_codex_event(
                Event {
                    id: ev.turn_id.clone(),
                    msg: EventMsg::PlanUpdate(UpdatePlanArgs {
                        explanation: ev.explanation,
                        plan: ev
                            .plan
                            .into_iter()
                            .map(|step| PlanItemArg {
                                step: step.step,
                                status: step_status_from_upstream(step.status),
                            })
                            .collect(),
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "item/agentMessage/delta" => {
            let ev: UpstreamAgentMessageDeltaNotification = serde_json::from_value(params)
                .context("decode item/agentMessage/delta notification")?;
            handle_codex_event(
                Event {
                    id: ev.turn_id,
                    msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta: ev.delta }),
                },
                recovery,
                event_tx,
            );
        }
        "item/plan/delta" => {
            let ev: UpstreamPlanDeltaNotification =
                serde_json::from_value(params).context("decode item/plan/delta notification")?;
            handle_codex_event(
                Event {
                    id: ev.turn_id,
                    msg: EventMsg::PlanDelta(PlanDeltaEvent { delta: ev.delta }),
                },
                recovery,
                event_tx,
            );
        }
        "item/commandExecution/outputDelta" => {
            let ev: UpstreamCommandExecutionOutputDeltaNotification =
                serde_json::from_value(params)
                    .context("decode item/commandExecution/outputDelta notification")?;
            handle_codex_event(
                Event {
                    id: ev.turn_id,
                    msg: EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
                        call_id: ev.item_id,
                        stream: ExecOutputStream::Stdout,
                        chunk: ev.delta.into_bytes(),
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "item/reasoning/summaryTextDelta" => {
            let ev: UpstreamReasoningSummaryTextDeltaNotification = serde_json::from_value(params)
                .context("decode item/reasoning/summaryTextDelta notification")?;
            handle_codex_event(
                Event {
                    id: ev.turn_id,
                    msg: EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                        delta: ev.delta,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "item/reasoning/summaryPartAdded" => {
            let ev: UpstreamReasoningSummaryPartAddedNotification = serde_json::from_value(params)
                .context("decode item/reasoning/summaryPartAdded notification")?;
            handle_codex_event(
                Event {
                    id: ev.turn_id,
                    msg: EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                        item_id: ev.item_id,
                        summary_index: ev.summary_index,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "item/reasoning/textDelta" => {
            let ev: UpstreamReasoningTextDeltaNotification = serde_json::from_value(params)
                .context("decode item/reasoning/textDelta notification")?;
            handle_codex_event(
                Event {
                    id: ev.turn_id,
                    msg: EventMsg::AgentReasoningRawContentDelta(
                        AgentReasoningRawContentDeltaEvent { delta: ev.delta },
                    ),
                },
                recovery,
                event_tx,
            );
        }
        "item/commandExecution/terminalInteraction" => {
            let ev: UpstreamTerminalInteractionNotification = serde_json::from_value(params)
                .context("decode item/commandExecution/terminalInteraction notification")?;
            handle_codex_event(
                Event {
                    id: ev.turn_id,
                    msg: EventMsg::TerminalInteraction(TerminalInteractionEvent {
                        call_id: ev.item_id,
                        process_id: ev.process_id,
                        stdin: ev.stdin,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "item/started" => {
            let ev: UpstreamItemStartedNotification =
                serde_json::from_value(params).context("decode item/started notification")?;
            handle_typed_item_started(ev, recovery, event_tx)?;
        }
        "item/autoApprovalReview/started" => {
            let ev: UpstreamItemGuardianApprovalReviewStartedNotification =
                serde_json::from_value(params)
                    .context("decode item/autoApprovalReview/started notification")?;
            let assessment = guardian_assessment_from_upstream_auto_approval_review(
                ev.turn_id,
                ev.target_item_id,
                ev.review,
                ev.action,
            );
            handle_codex_event(
                Event {
                    id: assessment.id.clone(),
                    msg: EventMsg::GuardianAssessment(assessment),
                },
                recovery,
                event_tx,
            );
        }
        "item/autoApprovalReview/completed" => {
            let ev: UpstreamItemGuardianApprovalReviewCompletedNotification =
                serde_json::from_value(params)
                    .context("decode item/autoApprovalReview/completed notification")?;
            let assessment = guardian_assessment_from_upstream_auto_approval_review(
                ev.turn_id,
                ev.target_item_id,
                ev.review,
                ev.action,
            );
            handle_codex_event(
                Event {
                    id: assessment.id.clone(),
                    msg: EventMsg::GuardianAssessment(assessment),
                },
                recovery,
                event_tx,
            );
        }
        "item/completed" => {
            let ev: UpstreamItemCompletedNotification =
                serde_json::from_value(params).context("decode item/completed notification")?;
            handle_typed_item_completed(ev, recovery, event_tx)?;
        }
        "mcpServer/startupStatus/updated" => {
            let ev: McpServerStartupStatusUpdatedNotification = serde_json::from_value(params)
                .context("decode mcpServer/startupStatus/updated notification")?;
            if ev.status == "failed" {
                handle_codex_event(
                    Event {
                        id: String::new(),
                        msg: EventMsg::Warning(WarningEvent {
                            message: ev.error.unwrap_or_else(|| {
                                format!("MCP server `{}` failed to start", ev.name)
                            }),
                        }),
                    },
                    recovery,
                    event_tx,
                );
            }
        }
        "error" => {
            let extracted_message = match &params {
                serde_json::Value::String(message) => Some(message.clone()),
                serde_json::Value::Object(obj) => obj
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
                    .or_else(|| {
                        obj.get("message")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string)
                    }),
                _ => None,
            };
            let params_repr =
                serde_json::to_string(&params).unwrap_or_else(|_| "<unserializable>".to_string());
            let ev: UpstreamErrorNotification = match serde_json::from_value(params) {
                Ok(ev) => ev,
                Err(err) => {
                    let extracted_message = extracted_message
                        .as_deref()
                        .unwrap_or("<unknown upstream error message>");
                    let id = recovery.active_turn_id.clone().unwrap_or_default();
                    handle_codex_event(
                        Event {
                            id,
                            msg: EventMsg::Warning(WarningEvent {
                                message: format!(
                                    "Failed to decode upstream `error` notification (ignored): {err}; extracted_message={extracted_message}; params={params_repr}"
                                ),
                            }),
                        },
                        recovery,
                        event_tx,
                    );
                    return Ok(());
                }
            };

            if ev.turn_id.is_empty()
                || ev
                    .error
                    .message
                    .contains("failed to refresh available models")
            {
                handle_codex_event(
                    Event {
                        id: ev.turn_id,
                        msg: EventMsg::Warning(WarningEvent {
                            message: format!("Upstream warning: {}", ev.error.message),
                        }),
                    },
                    recovery,
                    event_tx,
                );
                return Ok(());
            }

            if ev.will_retry {
                handle_codex_event(
                    Event {
                        id: ev.turn_id,
                        msg: EventMsg::StreamError(StreamErrorEvent {
                            message: ev.error.message,
                            codex_error_info: ev.error.codex_error_info,
                            additional_details: ev.error.additional_details,
                        }),
                    },
                    recovery,
                    event_tx,
                );
            } else {
                handle_codex_event(
                    Event {
                        id: ev.turn_id,
                        msg: EventMsg::Error(ErrorEvent {
                            message: ev.error.message,
                            codex_error_info: ev.error.codex_error_info,
                        }),
                    },
                    recovery,
                    event_tx,
                );
            }
        }
        _ => {}
    }

    Ok(())
}

fn step_status_from_upstream(status: UpstreamTurnPlanStepStatus) -> StepStatus {
    match status {
        UpstreamTurnPlanStepStatus::Pending => StepStatus::Pending,
        UpstreamTurnPlanStepStatus::InProgress => StepStatus::InProgress,
        UpstreamTurnPlanStepStatus::Completed => StepStatus::Completed,
    }
}

fn handle_typed_item_started(
    ev: UpstreamItemStartedNotification,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let Some(item_type) = ev.item.get("type").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };

    match item_type {
        "fileChange" => {
            let item: UpstreamFileChangeThreadItem =
                serde_json::from_value(ev.item).context("decode item/started fileChange item")?;
            let changes = file_changes_from_update_changes(&item.changes);
            handle_codex_event(
                Event {
                    id: item.id.clone(),
                    msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                        call_id: item.id,
                        turn_id: ev.turn_id,
                        auto_approved: false,
                        changes,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "commandExecution" => {
            let item: UpstreamCommandExecutionThreadItem = serde_json::from_value(ev.item)
                .context("decode item/started commandExecution item")?;
            handle_codex_event(
                Event {
                    id: item.id.clone(),
                    msg: EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                        call_id: item.id,
                        process_id: item.process_id,
                        turn_id: ev.turn_id,
                        command: command_tokens_from_string(&item.command),
                        cwd: item.cwd,
                        parsed_cmd: parsed_commands_from_actions(&item.command_actions),
                        source: ExecCommandSource::Agent,
                        interaction_input: None,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        _ => {}
    }

    Ok(())
}

fn handle_typed_item_completed(
    ev: UpstreamItemCompletedNotification,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let Some(item_type) = ev.item.get("type").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };

    match item_type {
        "agentMessage" => {
            let item: UpstreamAgentMessageThreadItem = serde_json::from_value(ev.item)
                .context("decode item/completed agentMessage item")?;
            handle_codex_event(
                Event {
                    id: item.id.clone(),
                    msg: EventMsg::AgentMessage(AgentMessageEvent {
                        message: item.text,
                        phase: item.phase,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "reasoning" => {
            let item: UpstreamReasoningThreadItem =
                serde_json::from_value(ev.item).context("decode item/completed reasoning item")?;
            let UpstreamReasoningThreadItem {
                id,
                summary,
                content,
            } = item;
            for (index, summary) in summary.into_iter().enumerate() {
                handle_codex_event(
                    Event {
                        id: format!("{id}:summary:{index}"),
                        msg: EventMsg::AgentReasoning(AgentReasoningEvent { text: summary }),
                    },
                    recovery,
                    event_tx,
                );
            }
            for (index, content) in content.into_iter().enumerate() {
                handle_codex_event(
                    Event {
                        id: format!("{id}:content:{index}"),
                        msg: EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent {
                            text: content,
                        }),
                    },
                    recovery,
                    event_tx,
                );
            }
        }
        "contextCompaction" => {
            let item: UpstreamContextCompactionThreadItem = serde_json::from_value(ev.item)
                .context("decode item/completed contextCompaction item")?;
            emit_context_compacted_event(item.id, ev.turn_id, recovery, event_tx);
        }
        "fileChange" => {
            let item: UpstreamFileChangeThreadItem =
                serde_json::from_value(ev.item).context("decode item/completed fileChange item")?;
            let changes = file_changes_from_update_changes(&item.changes);
            let (success, stderr) = match item.status {
                UpstreamPatchApplyStatus::Completed => (true, String::new()),
                UpstreamPatchApplyStatus::Failed => (false, "patch failed".to_string()),
                UpstreamPatchApplyStatus::Declined => (false, "patch declined".to_string()),
                UpstreamPatchApplyStatus::InProgress => {
                    (false, "patch still in progress".to_string())
                }
            };
            handle_codex_event(
                Event {
                    id: item.id.clone(),
                    msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                        call_id: item.id,
                        turn_id: ev.turn_id,
                        stdout: String::new(),
                        stderr,
                        success,
                        changes,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        "commandExecution" => {
            let item: UpstreamCommandExecutionThreadItem = serde_json::from_value(ev.item)
                .context("decode item/completed commandExecution item")?;
            let aggregated_output = item.aggregated_output.unwrap_or_default();
            let duration = item
                .duration_ms
                .and_then(|ms| u64::try_from(ms).ok())
                .map(std::time::Duration::from_millis)
                .unwrap_or_else(|| std::time::Duration::from_millis(0));
            let exit_code = item.exit_code.unwrap_or(match item.status {
                crate::app_server::upstream_protocol::CommandExecutionStatus::Completed => 0,
                _ => 1,
            });
            handle_codex_event(
                Event {
                    id: item.id.clone(),
                    msg: EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                        call_id: item.id,
                        process_id: item.process_id,
                        turn_id: ev.turn_id,
                        command: command_tokens_from_string(&item.command),
                        cwd: item.cwd,
                        parsed_cmd: parsed_commands_from_actions(&item.command_actions),
                        source: ExecCommandSource::Agent,
                        interaction_input: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        aggregated_output: aggregated_output.clone(),
                        exit_code,
                        duration,
                        formatted_output: aggregated_output,
                    }),
                },
                recovery,
                event_tx,
            );
        }
        _ => {}
    }

    Ok(())
}

fn hook_run_summary_from_upstream(run: UpstreamHookRunSummary) -> HookRunSummary {
    HookRunSummary {
        id: run.id,
        event_name: match run.event_name {
            UpstreamHookEventName::PreToolUse => HookEventName::PreToolUse,
            UpstreamHookEventName::PermissionRequest => HookEventName::PermissionRequest,
            UpstreamHookEventName::PostToolUse => HookEventName::PostToolUse,
            UpstreamHookEventName::SessionStart => HookEventName::SessionStart,
            UpstreamHookEventName::UserPromptSubmit => HookEventName::UserPromptSubmit,
            UpstreamHookEventName::Stop => HookEventName::Stop,
        },
        handler_type: match run.handler_type {
            UpstreamHookHandlerType::Command => HookHandlerType::Command,
            UpstreamHookHandlerType::Prompt => HookHandlerType::Prompt,
            UpstreamHookHandlerType::Agent => HookHandlerType::Agent,
        },
        execution_mode: match run.execution_mode {
            UpstreamHookExecutionMode::Sync => HookExecutionMode::Sync,
            UpstreamHookExecutionMode::Async => HookExecutionMode::Async,
        },
        scope: match run.scope {
            UpstreamHookScope::Thread => HookScope::Thread,
            UpstreamHookScope::Turn => HookScope::Turn,
        },
        source_path: run.source_path,
        display_order: run.display_order,
        status: match run.status {
            UpstreamHookRunStatus::Running => HookRunStatus::Running,
            UpstreamHookRunStatus::Completed => HookRunStatus::Completed,
            UpstreamHookRunStatus::Failed => HookRunStatus::Failed,
            UpstreamHookRunStatus::Blocked => HookRunStatus::Blocked,
            UpstreamHookRunStatus::Stopped => HookRunStatus::Stopped,
        },
        status_message: run.status_message,
        started_at: run.started_at,
        completed_at: run.completed_at,
        duration_ms: run.duration_ms,
        entries: run
            .entries
            .into_iter()
            .map(hook_output_entry_from_upstream)
            .collect(),
    }
}

fn hook_output_entry_from_upstream(entry: UpstreamHookOutputEntry) -> HookOutputEntry {
    HookOutputEntry {
        kind: match entry.kind {
            UpstreamHookOutputEntryKind::Warning => HookOutputEntryKind::Warning,
            UpstreamHookOutputEntryKind::Stop => HookOutputEntryKind::Stop,
            UpstreamHookOutputEntryKind::Feedback => HookOutputEntryKind::Feedback,
            UpstreamHookOutputEntryKind::Context => HookOutputEntryKind::Context,
            UpstreamHookOutputEntryKind::Error => HookOutputEntryKind::Error,
        },
        text: entry.text,
    }
}

fn jsonrpc_request_id_to_string(request_id: &RequestId) -> String {
    match request_id {
        RequestId::String(value) => value.clone(),
        RequestId::Integer(value) => value.to_string(),
    }
}

fn mcp_request_id_from_jsonrpc_request_id(request_id: &RequestId) -> McpRequestId {
    match request_id {
        RequestId::String(value) => McpRequestId::String(value.clone()),
        RequestId::Integer(value) => McpRequestId::Integer(*value),
    }
}

fn elicitation_request_from_upstream(
    request: crate::app_server::upstream_protocol::McpServerElicitationRequest,
) -> ProtocolElicitationRequest {
    match request {
        crate::app_server::upstream_protocol::McpServerElicitationRequest::Form {
            meta,
            message,
            requested_schema,
        } => ProtocolElicitationRequest::Form {
            meta,
            message,
            requested_schema,
        },
        crate::app_server::upstream_protocol::McpServerElicitationRequest::Url {
            meta,
            message,
            url,
            elicitation_id,
        } => ProtocolElicitationRequest::Url {
            meta,
            message,
            url,
            elicitation_id,
        },
    }
}

fn request_user_input_questions_from_upstream(
    questions: Vec<crate::app_server::upstream_protocol::ToolRequestUserInputQuestion>,
) -> Vec<ProtocolRequestUserInputQuestion> {
    questions
        .into_iter()
        .map(|question| ProtocolRequestUserInputQuestion {
            id: question.id,
            header: question.header,
            question: question.question,
            is_other: question.is_other,
            is_secret: question.is_secret,
            options: question.options.map(|options| {
                options
                    .into_iter()
                    .map(|option| ProtocolRequestUserInputQuestionOption {
                        label: option.label,
                        description: option.description,
                    })
                    .collect()
            }),
        })
        .collect()
}

fn request_permission_profile_from_upstream(
    profile: crate::app_server::upstream_protocol::RequestPermissionProfile,
) -> ProtocolRequestPermissionProfile {
    ProtocolRequestPermissionProfile {
        network: profile
            .network
            .map(|network| codex_protocol::models::NetworkPermissions {
                enabled: network.enabled,
            }),
        file_system: profile
            .file_system
            .map(file_system_permissions_from_upstream),
    }
}

fn file_system_permissions_from_upstream(
    profile: UpstreamAdditionalFileSystemPermissions,
) -> codex_protocol::models::FileSystemPermissions {
    let mut read = profile.read;
    let mut write = profile.write;

    for entry in profile.entries.into_iter().flatten() {
        let UpstreamFileSystemPath::Path { path } = entry.path else {
            continue;
        };

        match entry.access {
            UpstreamFileSystemAccessMode::Read => push_unique_path(&mut read, path),
            UpstreamFileSystemAccessMode::Write => push_unique_path(&mut write, path),
            UpstreamFileSystemAccessMode::None => {}
        }
    }

    codex_protocol::models::FileSystemPermissions { read, write }
}

fn push_unique_path(
    paths: &mut Option<Vec<codex_protocol::AbsolutePathBuf>>,
    path: codex_protocol::AbsolutePathBuf,
) {
    let paths = paths.get_or_insert_with(Vec::new);
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn guardian_assessment_status_from_upstream(
    status: UpstreamGuardianApprovalReviewStatus,
) -> GuardianAssessmentStatus {
    match status {
        UpstreamGuardianApprovalReviewStatus::InProgress => GuardianAssessmentStatus::InProgress,
        UpstreamGuardianApprovalReviewStatus::Approved => GuardianAssessmentStatus::Approved,
        UpstreamGuardianApprovalReviewStatus::Denied => GuardianAssessmentStatus::Denied,
        UpstreamGuardianApprovalReviewStatus::Aborted => GuardianAssessmentStatus::Aborted,
    }
}

fn guardian_risk_level_from_upstream(
    level: UpstreamGuardianRiskLevel,
) -> ProtocolGuardianRiskLevel {
    match level {
        UpstreamGuardianRiskLevel::Low => ProtocolGuardianRiskLevel::Low,
        UpstreamGuardianRiskLevel::Medium => ProtocolGuardianRiskLevel::Medium,
        UpstreamGuardianRiskLevel::High => ProtocolGuardianRiskLevel::High,
    }
}

fn guardian_assessment_from_upstream_auto_approval_review(
    turn_id: String,
    target_item_id: String,
    review: crate::app_server::upstream_protocol::GuardianApprovalReview,
    action: Option<serde_json::Value>,
) -> GuardianAssessmentEvent {
    GuardianAssessmentEvent {
        id: target_item_id,
        turn_id,
        status: guardian_assessment_status_from_upstream(review.status),
        risk_score: review.risk_score,
        risk_level: review.risk_level.map(guardian_risk_level_from_upstream),
        rationale: review.rationale,
        action,
    }
}

fn command_tokens_from_string(command: &str) -> Vec<String> {
    shlex::split(command).unwrap_or_else(|| vec![command.to_string()])
}

fn emit_context_compacted_event(
    event_id: String,
    turn_id: String,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) {
    // Newer app-server builds emit `contextCompaction` items, while older/deprecated transports
    // can still send `thread/compacted` for the same turn. Surface exactly one legacy event.
    if recovery.last_context_compaction_turn_id.as_deref() == Some(turn_id.as_str()) {
        return;
    }
    recovery.last_context_compaction_turn_id = Some(turn_id);
    handle_codex_event(
        Event {
            id: event_id,
            msg: EventMsg::ContextCompacted(ContextCompactedEvent),
        },
        recovery,
        event_tx,
    );
}

fn parsed_commands_from_actions(actions: &[UpstreamCommandAction]) -> Vec<ParsedCommand> {
    actions
        .iter()
        .map(|action| match action {
            UpstreamCommandAction::Read {
                command,
                name,
                path,
            } => ParsedCommand::Read {
                cmd: command.clone(),
                name: name.clone(),
                path: path.clone(),
            },
            UpstreamCommandAction::ListFiles { command, path } => ParsedCommand::ListFiles {
                cmd: command.clone(),
                path: path.clone(),
            },
            UpstreamCommandAction::Search {
                command,
                query,
                path,
            } => ParsedCommand::Search {
                cmd: command.clone(),
                query: query.clone(),
                path: path.clone(),
            },
            UpstreamCommandAction::Unknown { command } => ParsedCommand::Unknown {
                cmd: command.clone(),
            },
        })
        .collect()
}

fn file_changes_from_update_changes(
    changes: &[UpstreamFileUpdateChange],
) -> std::collections::HashMap<PathBuf, FileChange> {
    let mut out: std::collections::HashMap<PathBuf, FileChange> =
        std::collections::HashMap::with_capacity(changes.len());
    for change in changes {
        let path = PathBuf::from(&change.path);
        let file_change = match &change.kind {
            UpstreamPatchChangeKind::Add => FileChange::Add {
                content: change.diff.clone(),
            },
            UpstreamPatchChangeKind::Delete => FileChange::Delete {
                content: change.diff.clone(),
            },
            UpstreamPatchChangeKind::Update { move_path } => {
                let unified_diff = strip_moved_to_suffix(&change.diff, move_path.as_ref());
                FileChange::Update {
                    unified_diff,
                    move_path: move_path.clone(),
                }
            }
        };
        out.insert(path, file_change);
    }
    out
}

fn strip_moved_to_suffix(diff: &str, move_path: Option<&PathBuf>) -> String {
    let Some(move_path) = move_path else {
        return diff.to_string();
    };

    let suffix = format!("\n\nMoved to: {}", move_path.display());
    diff.strip_suffix(&suffix).unwrap_or(diff).to_string()
}

fn handle_codex_event_notification(
    method: &str,
    params: Option<serde_json::Value>,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    if !method.starts_with("codex/event/") {
        return Ok(());
    }
    let Some(params) = params else {
        return Ok(());
    };

    let event: Event = serde_json::from_value(params)?;
    handle_codex_event(event, recovery, event_tx);
    Ok(())
}

fn handle_codex_event(
    event: Event,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) {
    // `codex-potter` uses `thread/rollback` internally to ensure stream recovery retries do not
    // accumulate redundant automatic `Continue` turns in the thread history. The app-server
    // forwards raw `codex/event/*` notifications for every core event, including rollback
    // lifecycle events that are UI-irrelevant (or even confusing) for CodexPotter users.
    if matches!(&event.msg, EventMsg::ThreadRolledBack(_))
        && recovery.event_mode == AppServerEventMode::Interactive
    {
        return;
    }

    match &event.msg {
        EventMsg::TurnStarted(ev) if !ev.turn_id.is_empty() => {
            recovery.active_turn_id = Some(ev.turn_id.clone());
        }
        EventMsg::TurnComplete(ev) => {
            if ev.turn_id.is_empty()
                || recovery.active_turn_id.as_deref() == Some(ev.turn_id.as_str())
            {
                recovery.active_turn_id = None;
            }
        }
        EventMsg::TurnAborted(ev) => match ev.turn_id.as_deref() {
            Some(turn_id) => {
                if recovery.active_turn_id.as_deref() == Some(turn_id) {
                    recovery.active_turn_id = None;
                }
            }
            None if ev.reason != codex_protocol::protocol::TurnAbortReason::Replaced => {
                recovery.active_turn_id = None;
            }
            None => {}
        },
        _ => {}
    }

    let event_id = event.id.clone();
    let mut should_forward = true;
    let mut round_outcome: Option<PotterRoundOutcome> = None;
    let mut pre_forward_events: Vec<EventMsg> = Vec::new();
    let mut post_forward_events: Vec<EventMsg> = Vec::new();

    let was_in_retry_streak = recovery.stream_recovery.is_in_retry_streak();
    let should_suppress_turn_complete = match &event.msg {
        EventMsg::TurnComplete(ev) => recovery.stream_recovery.should_suppress_turn_complete(ev),
        _ => false,
    };
    let turn_complete_counts_for_round_completion =
        matches!(&event.msg, EventMsg::TurnComplete(_)) && !should_suppress_turn_complete;
    let mut error_is_recoverable = false;

    recovery.stream_recovery.observe_event(&event.msg);

    if was_in_retry_streak && !recovery.stream_recovery.is_in_retry_streak() {
        recovery.pending_continue_retry = None;
        let msg = EventMsg::PotterStreamRecoveryRecovered;
        match recovery.event_mode {
            AppServerEventMode::Interactive => pre_forward_events.push(msg),
            AppServerEventMode::ExecJson => post_forward_events.push(msg),
        }
    }

    if let EventMsg::Error(err) = &event.msg
        && recovery.has_sent_turn_start
    {
        if recovery.pending_continue_retry.is_some()
            && codex_protocol::potter_stream_recovery::is_retryable_stream_error(err)
        {
            // A retryable error was already observed for the current turn. Wait for TurnComplete
            // and then issue the planned automatic `Continue`.
            error_is_recoverable = true;
            if recovery.event_mode == AppServerEventMode::Interactive {
                should_forward = false;
            }
        } else if recovery.pending_continue_retry.is_none()
            && let Some(decision) = recovery.stream_recovery.plan_retry(err)
        {
            match decision {
                ContinueRetryDecision::Retry(plan) => {
                    error_is_recoverable = true;
                    let msg = EventMsg::PotterStreamRecoveryUpdate {
                        attempt: plan.attempt,
                        max_attempts: plan.max_attempts,
                        error_message: err.message.clone(),
                    };
                    match recovery.event_mode {
                        AppServerEventMode::Interactive => pre_forward_events.push(msg),
                        AppServerEventMode::ExecJson => post_forward_events.push(msg),
                    }
                    recovery.pending_continue_retry = Some(plan);
                }
                ContinueRetryDecision::GiveUp {
                    attempts,
                    max_attempts,
                } => {
                    let msg = EventMsg::PotterStreamRecoveryGaveUp {
                        error_message: err.message.clone(),
                        attempts,
                        max_attempts,
                    };
                    match recovery.event_mode {
                        AppServerEventMode::Interactive => pre_forward_events.push(msg),
                        AppServerEventMode::ExecJson => post_forward_events.push(msg),
                    }
                    round_outcome = Some(PotterRoundOutcome::TaskFailed {
                        message: format!(
                            "{} (stream recovery gave up after {attempts}/{max_attempts} retries)",
                            err.message
                        ),
                    });
                }
            }

            if recovery.event_mode == AppServerEventMode::Interactive {
                should_forward = false;
            }
        }
    }

    if matches!(&event.msg, EventMsg::TurnAborted(_)) {
        recovery.pending_continue_retry = None;
    }

    if matches!(&event.msg, EventMsg::TurnComplete(_))
        && let Some(plan) = recovery.pending_continue_retry.take()
    {
        let tx = recovery.recovery_action_tx.clone();
        let action = RecoveryAction::RetryContinue {
            attempt: plan.attempt,
        };
        if plan.backoff.is_zero() {
            let _ = tx.send(action);
        } else {
            tokio::spawn(async move {
                tokio::time::sleep(plan.backoff).await;
                let _ = tx.send(action);
            });
        }
    }

    if should_suppress_turn_complete && recovery.event_mode == AppServerEventMode::Interactive {
        should_forward = false;
    }

    if round_outcome.is_none() {
        round_outcome = match &event.msg {
            EventMsg::TurnComplete(_) if turn_complete_counts_for_round_completion => {
                Some(PotterRoundOutcome::Completed)
            }
            EventMsg::TurnAborted(ev) => match ev.reason {
                codex_protocol::protocol::TurnAbortReason::Interrupted => {
                    Some(PotterRoundOutcome::Interrupted)
                }
                codex_protocol::protocol::TurnAbortReason::ReviewEnded => {
                    Some(PotterRoundOutcome::UserRequested)
                }
                codex_protocol::protocol::TurnAbortReason::Replaced => None,
            },
            EventMsg::Error(err) if should_forward && !error_is_recoverable => {
                Some(PotterRoundOutcome::Fatal {
                    message: err.message.clone(),
                })
            }
            _ => None,
        };
    }

    for msg in pre_forward_events {
        let _ = event_tx.send(Event {
            id: event_id.clone(),
            msg,
        });
    }
    if should_forward {
        let _ = event_tx.send(event);
    }
    for msg in post_forward_events {
        let _ = event_tx.send(Event {
            id: event_id.clone(),
            msg,
        });
    }

    if !recovery.has_finished_round
        && let Some(outcome) = round_outcome
    {
        recovery.has_finished_round = true;
        let _ = event_tx.send(Event {
            id: event_id,
            msg: EventMsg::PotterRoundFinished {
                outcome,
                duration_secs: measured_round_duration_secs(recovery.round_started_at),
            },
        });
    }
}

async fn handle_server_request(
    stdin: &mut ChildStdin,
    request: crate::app_server::upstream_protocol::JSONRPCRequest,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let request_id = request.id.clone();
    let method = request.method.clone();
    let server_request = match ServerRequest::try_from(request) {
        Ok(request) => request,
        Err(err) => {
            let message = format!("unsupported server request {method:?}: {err}");
            send_message(
                stdin,
                &JSONRPCMessage::Error(JSONRPCError {
                    error: JSONRPCErrorError {
                        code: -32601,
                        message,
                        data: None,
                    },
                    id: request_id,
                }),
            )
            .await?;
            return Ok(());
        }
    };

    let bypass_approvals_and_sandbox = recovery.bypass_approvals_and_sandbox;
    let active_turn_id = recovery.active_turn_id.clone();
    let resolve_event_id = |turn_id: Option<&str>| {
        turn_id
            .filter(|id| !id.is_empty())
            .map(ToString::to_string)
            .or_else(|| active_turn_id.clone())
            .unwrap_or_default()
    };

    let emit_warning = |turn_id: Option<&str>, message: String| {
        let id = resolve_event_id(turn_id);
        let _ = event_tx.send(Event {
            id,
            msg: EventMsg::Warning(WarningEvent { message }),
        });
    };

    let emit_error = |turn_id: Option<&str>, message: String| {
        let id = resolve_event_id(turn_id);
        let _ = event_tx.send(Event {
            id,
            msg: EventMsg::Error(ErrorEvent {
                message,
                codex_error_info: None,
            }),
        });
    };

    match server_request {
        ServerRequest::CommandExecution { .. } => {
            let response = CommandExecutionRequestApprovalResponse {
                decision: CommandExecutionApprovalDecision::Accept,
            };
            send_response(stdin, request_id, response).await?;
        }
        ServerRequest::FileChange { .. } => {
            let response = FileChangeRequestApprovalResponse {
                decision: FileChangeApprovalDecision::Accept,
            };
            send_response(stdin, request_id, response).await?;
        }
        ServerRequest::ToolRequestUserInput { params, .. } => {
            let mut warning_turn_id: Option<String> = None;
            let mut warning_item_id: Option<String> = None;
            let mut warning_questions: usize = 0;

            if let Some(params) = params {
                warning_questions = params.questions.len();
                warning_turn_id = Some(params.turn_id.clone());
                warning_item_id = Some(params.item_id.clone());

                handle_codex_event(
                    Event {
                        id: params.item_id.clone(),
                        msg: EventMsg::RequestUserInput(RequestUserInputEvent {
                            call_id: params.item_id,
                            turn_id: params.turn_id,
                            questions: request_user_input_questions_from_upstream(params.questions),
                        }),
                    },
                    recovery,
                    event_tx,
                );
            }

            let item_id = warning_item_id.as_deref().unwrap_or("<unknown>");
            emit_warning(
                warning_turn_id.as_deref(),
                format!(
                    "Upstream requested request_user_input (itemId={item_id}, questions={warning_questions}); running non-interactively, returning empty answers."
                ),
            );
            let response = ToolRequestUserInputResponse::default();
            send_response(stdin, request_id, response).await?;
        }
        ServerRequest::McpServerElicitationRequest { params, .. } => {
            let mut warning_turn_id: Option<String> = None;
            let mut warning_server_name: Option<String> = None;
            let mut warning_mode: &'static str = "<unknown>";

            if let Some(params) = params {
                warning_turn_id = params.turn_id.clone();
                warning_server_name = Some(params.server_name.clone());
                warning_mode = match &params.request {
                    crate::app_server::upstream_protocol::McpServerElicitationRequest::Form {
                        ..
                    } => "form",
                    crate::app_server::upstream_protocol::McpServerElicitationRequest::Url {
                        ..
                    } => "url",
                };

                handle_codex_event(
                    Event {
                        id: jsonrpc_request_id_to_string(&request_id),
                        msg: EventMsg::ElicitationRequest(ElicitationRequestEvent {
                            turn_id: params.turn_id,
                            server_name: params.server_name,
                            id: mcp_request_id_from_jsonrpc_request_id(&request_id),
                            request: Some(elicitation_request_from_upstream(params.request)),
                            message: None,
                        }),
                    },
                    recovery,
                    event_tx,
                );
            }

            let server_name = warning_server_name.as_deref().unwrap_or("<unknown>");
            emit_warning(
                warning_turn_id.as_deref(),
                format!(
                    "Upstream requested an MCP server elicitation (serverName={server_name}, mode={warning_mode}); running non-interactively, auto-cancelling."
                ),
            );
            let response = McpServerElicitationRequestResponse {
                action: McpServerElicitationAction::Cancel,
                content: None,
                meta: None,
            };
            send_response(stdin, request_id, response).await?;
        }
        ServerRequest::PermissionsRequestApproval { params, .. } => {
            let mut turn_id: Option<String> = None;
            let mut reason: Option<String> = None;
            let mut granted = GrantedPermissionProfile::default();
            let mut scope = PermissionGrantScope::Turn;

            if let Some(params) = params {
                let permissions_for_grant = params.permissions.clone();
                turn_id = Some(params.turn_id.clone());
                reason = params.reason.clone();

                handle_codex_event(
                    Event {
                        id: params.item_id.clone(),
                        msg: EventMsg::RequestPermissions(RequestPermissionsEvent {
                            call_id: params.item_id,
                            turn_id: params.turn_id,
                            reason: params.reason,
                            cwd: params.cwd,
                            permissions: request_permission_profile_from_upstream(
                                params.permissions,
                            ),
                        }),
                    },
                    recovery,
                    event_tx,
                );

                if bypass_approvals_and_sandbox {
                    granted = GrantedPermissionProfile::from(permissions_for_grant);
                    scope = PermissionGrantScope::Session;
                }
            }

            let note = match bypass_approvals_and_sandbox {
                true => "granting requested permissions",
                false => "denying requested permissions",
            };
            if let Some(reason) = reason.as_deref() {
                emit_warning(
                    turn_id.as_deref(),
                    format!(
                        "Upstream requested additional permissions (reason={reason}); running non-interactively, {note}."
                    ),
                );
            } else {
                emit_warning(
                    turn_id.as_deref(),
                    format!(
                        "Upstream requested additional permissions; running non-interactively, {note}."
                    ),
                );
            }

            let response = PermissionsRequestApprovalResponse {
                permissions: granted,
                scope,
                strict_auto_review: None,
            };
            send_response(stdin, request_id, response).await?;
        }
        ServerRequest::DynamicToolCall { params, .. } => {
            let (turn_id, call_id, tool) = match params {
                Some(params) => (
                    Some(params.turn_id),
                    Some(params.call_id),
                    Some(params.tool),
                ),
                None => (None, None, None),
            };
            let call_id = call_id.as_deref().unwrap_or("<unknown>");
            let tool = tool.as_deref().unwrap_or("<unknown>");
            emit_warning(
                turn_id.as_deref(),
                format!(
                    "Upstream requested a dynamic tool call (callId={call_id}, tool={tool}); codex-potter does not support dynamic tool calls, returning failure."
                ),
            );
            let response = DynamicToolCallResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: "codex-potter does not support dynamic tool calls.".to_string(),
                }],
                success: false,
            };
            send_response(stdin, request_id, response).await?;
        }
        ServerRequest::ChatgptAuthTokensRefresh { params, .. } => {
            let reason = params
                .as_ref()
                .map(|params| format!("{:?}", params.reason))
                .unwrap_or_else(|| "<unknown>".to_string());
            let message = format!(
                "Upstream requested a ChatGPT auth token refresh (reason={reason}), but codex-potter does not implement ChatGPT token refresh."
            );
            emit_error(None, message.clone());
            send_message(
                stdin,
                &JSONRPCMessage::Error(JSONRPCError {
                    error: JSONRPCErrorError {
                        code: -32000,
                        message,
                        data: None,
                    },
                    id: request_id,
                }),
            )
            .await?;
        }
        ServerRequest::ApplyPatchApproval { .. } => {
            let response = ApplyPatchApprovalResponse {
                decision: ReviewDecision::Approved,
            };
            send_response(stdin, request_id, response).await?;
        }
        ServerRequest::ExecCommandApproval { .. } => {
            let response = ExecCommandApprovalResponse {
                decision: ReviewDecision::Approved,
            };
            send_response(stdin, request_id, response).await?;
        }
    }

    Ok(())
}

async fn send_message<T>(stdin: &mut ChildStdin, message: &T) -> anyhow::Result<()>
where
    T: serde::Serialize,
{
    let json = serde_json::to_vec(message)?;
    stdin.write_all(&json).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn send_response<T>(
    stdin: &mut ChildStdin,
    request_id: RequestId,
    response: T,
) -> anyhow::Result<()>
where
    T: serde::Serialize,
{
    send_message(
        stdin,
        &JSONRPCMessage::Response(JSONRPCResponse {
            id: request_id,
            result: serde_json::to_value(response)?,
        }),
    )
    .await
}

async fn read_until_response(
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    request_id: RequestId,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<JSONRPCResponse> {
    match read_until_response_or_error(stdin, lines, &request_id, recovery, event_tx).await? {
        Ok(response) => Ok(response),
        Err(error) => {
            anyhow::bail!("app-server returned error for {request_id:?}: {error:?}");
        }
    }
}

async fn read_until_response_or_error(
    stdin: &mut ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    request_id: &RequestId,
    recovery: &mut StreamRecoveryContext,
    event_tx: &UnboundedSender<Event>,
) -> anyhow::Result<Result<JSONRPCResponse, JSONRPCErrorError>> {
    loop {
        let Some(line) = lines.next_line().await? else {
            anyhow::bail!("app-server stdout closed while waiting for response {request_id:?}");
        };
        let msg: JSONRPCMessage =
            serde_json::from_str(&line).with_context(|| format!("decode json-rpc: {line}"))?;

        match msg {
            JSONRPCMessage::Response(response) if &response.id == request_id => {
                return Ok(Ok(response));
            }
            JSONRPCMessage::Error(err) if &err.id == request_id => return Ok(Err(err.error)),
            JSONRPCMessage::Notification(notification) => {
                if notification.method.starts_with("codex/event/") {
                    handle_codex_event_notification(
                        &notification.method,
                        notification.params,
                        recovery,
                        event_tx,
                    )?;
                } else {
                    handle_typed_notification(notification, recovery, event_tx)?;
                }
            }
            JSONRPCMessage::Request(request) => {
                handle_server_request(stdin, request, recovery, event_tx).await?;
            }
            _ => {}
        }
    }
}

fn synthesize_session_configured(
    thread_start_or_resume: &ThreadStartOrResume,
) -> anyhow::Result<SessionConfiguredEvent> {
    let thread_id =
        ThreadId::from_string(thread_start_or_resume.thread_id()).context("parse thread id")?;

    Ok(SessionConfiguredEvent {
        session_id: thread_id,
        forked_from_id: None,
        model: thread_start_or_resume.model().to_string(),
        model_provider_id: thread_start_or_resume.model_provider().to_string(),
        service_tier: thread_start_or_resume.service_tier(),
        cwd: thread_start_or_resume.cwd().to_path_buf(),
        reasoning_effort: thread_start_or_resume.reasoning_effort(),
        history_log_id: 0,
        history_entry_count: 0,
        initial_messages: None,
        rollout_path: thread_start_or_resume.rollout_path()?.to_path_buf(),
    })
}

enum ThreadStartOrResume {
    Start(ThreadStartResponse),
    Resume(ThreadResumeResponse),
}

impl ThreadStartOrResume {
    fn thread_id(&self) -> &str {
        match self {
            ThreadStartOrResume::Start(resp) => &resp.thread.id,
            ThreadStartOrResume::Resume(resp) => &resp.thread.id,
        }
    }

    fn model(&self) -> &str {
        match self {
            ThreadStartOrResume::Start(resp) => &resp.model,
            ThreadStartOrResume::Resume(resp) => &resp.model,
        }
    }

    fn model_provider(&self) -> &str {
        match self {
            ThreadStartOrResume::Start(resp) => &resp.model_provider,
            ThreadStartOrResume::Resume(resp) => &resp.model_provider,
        }
    }

    fn service_tier(&self) -> Option<ServiceTier> {
        match self {
            ThreadStartOrResume::Start(resp) => resp
                .service_tier
                .as_deref()
                .and_then(upstream_response_service_tier_to_internal),
            ThreadStartOrResume::Resume(resp) => resp
                .service_tier
                .as_deref()
                .and_then(upstream_response_service_tier_to_internal),
        }
    }

    fn cwd(&self) -> &Path {
        match self {
            ThreadStartOrResume::Start(resp) => resp.cwd.as_path(),
            ThreadStartOrResume::Resume(resp) => resp.cwd.as_path(),
        }
    }

    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        match self {
            ThreadStartOrResume::Start(resp) => resp.reasoning_effort,
            ThreadStartOrResume::Resume(resp) => resp.reasoning_effort,
        }
    }

    fn rollout_path(&self) -> anyhow::Result<&Path> {
        match self {
            ThreadStartOrResume::Start(resp) => resp
                .thread
                .path
                .as_deref()
                .context("thread/start response missing rollout path"),
            ThreadStartOrResume::Resume(resp) => resp
                .thread
                .path
                .as_deref()
                .context("thread/resume response missing rollout path"),
        }
    }
}

fn next_request_id(next_id: &mut i64) -> RequestId {
    let id = *next_id;
    *next_id += 1;
    RequestId::Integer(id)
}

/// `0` is reserved for synthetic or legacy "duration unavailable" round markers.
/// Real measured round durations therefore round up to at least one second.
fn measured_round_duration_secs(round_started_at: Instant) -> u64 {
    round_started_at.elapsed().as_secs().max(1)
}

#[cfg(test)]
mod session_configured_tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn thread_start_or_resume_maps_priority_response_service_tier_to_fast() {
        let response: ThreadStartResponse = serde_json::from_value(json!({
            "thread": {
                "id": "thread-1"
            },
            "model": "gpt-5.4",
            "modelProvider": "openai",
            "serviceTier": "priority",
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
        .expect("deserialize thread/start response with priority service tier");

        let response = ThreadStartOrResume::Start(response);

        assert_eq!(response.service_tier(), Some(ServiceTier::Fast));
    }
}

#[cfg(test)]
mod stream_recovery_tests {
    use super::*;
    use std::time::Duration;

    use codex_protocol::protocol::AgentMessageDeltaEvent;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::ThreadRolledBackEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use pretty_assertions::assert_eq;

    struct RecoveryHarness {
        recovery: StreamRecoveryContext,
        event_tx: UnboundedSender<Event>,
        event_rx: UnboundedReceiver<Event>,
        action_rx: UnboundedReceiver<RecoveryAction>,
    }

    fn recovery_harness(
        event_mode: AppServerEventMode,
        has_sent_turn_start: bool,
    ) -> RecoveryHarness {
        let (event_tx, event_rx) = unbounded_channel::<Event>();
        let (action_tx, action_rx) = unbounded_channel::<RecoveryAction>();
        let recovery = StreamRecoveryContext {
            stream_recovery: PotterStreamRecovery::new(),
            recovery_action_tx: action_tx,
            pending_continue_retry: None,
            active_turn_id: None,
            last_context_compaction_turn_id: None,
            has_sent_turn_start,
            has_finished_round: false,
            round_started_at: Instant::now(),
            last_turn_start_was_recovery_continue: false,
            event_mode,
            bypass_approvals_and_sandbox: false,
        };

        RecoveryHarness {
            recovery,
            event_tx,
            event_rx,
            action_rx,
        }
    }

    fn retryable_error_event() -> ErrorEvent {
        ErrorEvent {
            message: "stream disconnected before completion: error sending request for url (...)"
                .to_string(),
            codex_error_info: Some(CodexErrorInfo::ResponseStreamDisconnected {
                http_status_code: None,
            }),
        }
    }

    fn retryable_sign_in_again_error_event() -> ErrorEvent {
        ErrorEvent {
            message: "unexpected status 401: Your access token could not be refreshed because you have since logged out or signed in to another account. Please sign in again.".to_string(),
            codex_error_info: Some(CodexErrorInfo::Unauthorized),
        }
    }

    #[test]
    fn completed_rounds_round_up_short_measured_duration_to_preserve_zero_sentinel() {
        let mut harness = recovery_harness(AppServerEventMode::Interactive, false);
        harness.recovery.round_started_at = Instant::now() - Duration::from_millis(100);

        handle_codex_event(
            Event {
                id: "turn-1".to_string(),
                msg: EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    model_context_window: None,
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );
        handle_codex_event(
            Event {
                id: "turn-1".to_string(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "turn-1".to_string(),
                    last_agent_message: None,
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let events = std::iter::from_fn(|| harness.event_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(matches!(
            events.last(),
            Some(Event {
                msg: EventMsg::PotterRoundFinished {
                    outcome: PotterRoundOutcome::Completed,
                    duration_secs: 1,
                },
                ..
            })
        ));
    }

    #[test]
    fn interrupted_rounds_round_up_short_measured_duration_to_preserve_zero_sentinel() {
        let mut harness = recovery_harness(AppServerEventMode::Interactive, false);
        harness.recovery.round_started_at = Instant::now() - Duration::from_millis(100);

        handle_codex_event(
            Event {
                id: "turn-1".to_string(),
                msg: EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    model_context_window: None,
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );
        handle_codex_event(
            Event {
                id: "turn-1".to_string(),
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: Some("turn-1".to_string()),
                    reason: TurnAbortReason::Interrupted,
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let events = std::iter::from_fn(|| harness.event_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(matches!(
            events.last(),
            Some(Event {
                msg: EventMsg::PotterRoundFinished {
                    outcome: PotterRoundOutcome::Interrupted,
                    duration_secs: 1,
                },
                ..
            })
        ));
    }

    #[test]
    fn stream_recovery_translates_retryable_error_to_potter_events() {
        let mut harness = recovery_harness(AppServerEventMode::Interactive, true);

        handle_codex_event(
            Event {
                id: "err".into(),
                msg: EventMsg::Error(retryable_error_event()),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let event = harness
            .event_rx
            .try_recv()
            .expect("expected injected event");
        let EventMsg::PotterStreamRecoveryUpdate {
            attempt,
            max_attempts,
            error_message,
        } = event.msg
        else {
            panic!("expected PotterStreamRecoveryUpdate, got: {:?}", event.msg);
        };
        assert_eq!(attempt, 1);
        assert_eq!(max_attempts, 10);
        assert_eq!(
            error_message,
            "stream disconnected before completion: error sending request for url (...)"
        );

        assert!(
            harness.event_rx.try_recv().is_err(),
            "expected retryable Error to be suppressed"
        );

        assert!(
            harness.action_rx.try_recv().is_err(),
            "expected no immediate retry action"
        );

        handle_codex_event(
            Event {
                id: "turn-complete".into(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "turn-1".to_string(),
                    last_agent_message: None,
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        assert!(
            harness.event_rx.try_recv().is_err(),
            "expected empty TurnComplete to be suppressed during retry streak"
        );
        assert_eq!(
            harness
                .action_rx
                .try_recv()
                .expect("expected RetryContinue action"),
            RecoveryAction::RetryContinue { attempt: 1 }
        );

        handle_codex_event(
            Event {
                id: "activity".into(),
                msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    delta: "hello".to_string(),
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let recovered = harness
            .event_rx
            .try_recv()
            .expect("expected recovered event");
        assert!(matches!(
            recovered.msg,
            EventMsg::PotterStreamRecoveryRecovered
        ));

        let forwarded = harness
            .event_rx
            .try_recv()
            .expect("expected forwarded activity event");
        assert!(matches!(forwarded.msg, EventMsg::AgentMessageDelta(_)));
    }

    #[test]
    fn stream_recovery_ignores_retryable_error_before_first_turn_start() {
        let mut harness = recovery_harness(AppServerEventMode::Interactive, false);

        handle_codex_event(
            Event {
                id: "err".into(),
                msg: EventMsg::Error(retryable_error_event()),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let event = harness
            .event_rx
            .try_recv()
            .expect("expected forwarded error");
        assert!(matches!(event.msg, EventMsg::Error(_)));

        let round_finished = harness
            .event_rx
            .try_recv()
            .expect("expected round finished marker");
        assert!(matches!(
            round_finished.msg,
            EventMsg::PotterRoundFinished {
                outcome: PotterRoundOutcome::Fatal { .. },
                ..
            }
        ));
        assert!(
            harness.action_rx.try_recv().is_err(),
            "expected no continue action"
        );
    }

    #[test]
    fn stream_recovery_retries_sign_in_again_error_within_round() {
        let mut harness = recovery_harness(AppServerEventMode::Interactive, true);

        handle_codex_event(
            Event {
                id: "err".into(),
                msg: EventMsg::Error(retryable_sign_in_again_error_event()),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let update = harness
            .event_rx
            .try_recv()
            .expect("expected recovery update for auth refresh noise");
        let EventMsg::PotterStreamRecoveryUpdate {
            attempt,
            max_attempts,
            error_message,
        } = update.msg
        else {
            panic!("expected PotterStreamRecoveryUpdate, got: {:?}", update.msg);
        };
        assert_eq!((attempt, max_attempts), (1, 10));
        assert!(error_message.contains("Please sign in again."));
        assert!(
            harness.event_rx.try_recv().is_err(),
            "expected auth retry error to be suppressed"
        );

        handle_codex_event(
            Event {
                id: "turn-complete".into(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "turn-1".to_string(),
                    last_agent_message: None,
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        assert!(
            harness.event_rx.try_recv().is_err(),
            "expected empty TurnComplete to be suppressed during auth retry"
        );
        assert_eq!(
            harness
                .action_rx
                .try_recv()
                .expect("expected RetryContinue action"),
            RecoveryAction::RetryContinue { attempt: 1 }
        );
        assert!(
            !harness.recovery.has_finished_round,
            "round should stay alive for automatic Continue"
        );
    }

    #[test]
    fn stream_recovery_gives_up_after_retry_cap() {
        let mut harness = recovery_harness(AppServerEventMode::Interactive, true);

        let err = retryable_error_event();
        for _ in 0..10 {
            let Some(ContinueRetryDecision::Retry(_)) =
                harness.recovery.stream_recovery.plan_retry(&err)
            else {
                panic!("expected retry plan while warming retry streak");
            };
        }

        handle_codex_event(
            Event {
                id: "err".into(),
                msg: EventMsg::Error(err),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let event = harness
            .event_rx
            .try_recv()
            .expect("expected injected event");
        let EventMsg::PotterStreamRecoveryGaveUp {
            error_message,
            attempts,
            max_attempts,
        } = event.msg
        else {
            panic!("expected PotterStreamRecoveryGaveUp, got: {:?}", event.msg);
        };
        assert!(error_message.contains("stream disconnected before completion"));
        assert_eq!((attempts, max_attempts), (10, 10));

        let round_finished = harness
            .event_rx
            .try_recv()
            .expect("expected round finished marker");
        assert!(matches!(
            round_finished.msg,
            EventMsg::PotterRoundFinished {
                outcome: PotterRoundOutcome::TaskFailed { .. },
                ..
            }
        ));

        assert!(
            harness.event_rx.try_recv().is_err(),
            "expected Error to be suppressed after giving up"
        );
        assert!(
            harness.action_rx.try_recv().is_err(),
            "expected no continue action after giving up"
        );
    }

    #[test]
    fn thread_rollback_failed_error_is_forwarded_and_ends_round() {
        let mut harness = recovery_harness(AppServerEventMode::Interactive, true);

        handle_codex_event(
            Event {
                id: "rollback-failed".into(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "Cannot rollback while a turn is in progress.".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let forwarded = harness
            .event_rx
            .try_recv()
            .expect("expected forwarded error");
        assert!(matches!(forwarded.msg, EventMsg::Error(_)));

        let round_finished = harness
            .event_rx
            .try_recv()
            .expect("expected round finished marker");
        assert!(matches!(
            round_finished.msg,
            EventMsg::PotterRoundFinished {
                outcome: PotterRoundOutcome::Fatal { .. },
                ..
            }
        ));
        assert!(
            harness.action_rx.try_recv().is_err(),
            "expected no recovery action from ThreadRollbackFailed"
        );
        assert!(
            harness.recovery.has_finished_round,
            "round should end as fatal"
        );
    }

    #[test]
    fn thread_rolled_back_event_is_forwarded_only_in_exec_json_mode() {
        {
            let mut harness = recovery_harness(AppServerEventMode::Interactive, true);
            handle_codex_event(
                Event {
                    id: "rolled-back".into(),
                    msg: EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns: 1 }),
                },
                &mut harness.recovery,
                &harness.event_tx,
            );

            assert!(
                harness.event_rx.try_recv().is_err(),
                "expected ThreadRolledBack to be suppressed"
            );
            assert!(
                harness.action_rx.try_recv().is_err(),
                "expected no recovery action from ThreadRolledBack"
            );
            assert!(
                !harness.recovery.has_finished_round,
                "round should continue"
            );
        }

        {
            let mut harness = recovery_harness(AppServerEventMode::ExecJson, true);
            handle_codex_event(
                Event {
                    id: "rolled-back".into(),
                    msg: EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns: 1 }),
                },
                &mut harness.recovery,
                &harness.event_tx,
            );

            let forwarded = harness
                .event_rx
                .try_recv()
                .expect("expected forwarded rollback event");
            assert!(matches!(forwarded.msg, EventMsg::ThreadRolledBack(_)));
            assert!(
                harness.event_rx.try_recv().is_err(),
                "expected no additional events"
            );
            assert!(
                harness.action_rx.try_recv().is_err(),
                "expected no recovery action from ThreadRolledBack"
            );
            assert!(
                !harness.recovery.has_finished_round,
                "round should continue"
            );
        }
    }

    #[test]
    fn exec_json_forwards_recovery_error_and_empty_turn_complete_without_finishing_round() {
        let mut harness = recovery_harness(AppServerEventMode::ExecJson, true);

        handle_codex_event(
            Event {
                id: "err".into(),
                msg: EventMsg::Error(retryable_error_event()),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let forwarded_error = harness
            .event_rx
            .try_recv()
            .expect("expected forwarded Error");
        assert!(matches!(forwarded_error.msg, EventMsg::Error(_)));

        let update = harness
            .event_rx
            .try_recv()
            .expect("expected PotterStreamRecoveryUpdate");
        assert!(matches!(
            update.msg,
            EventMsg::PotterStreamRecoveryUpdate { .. }
        ));

        handle_codex_event(
            Event {
                id: "turn-complete".into(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "turn-1".to_string(),
                    last_agent_message: None,
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let forwarded_turn_complete = harness
            .event_rx
            .try_recv()
            .expect("expected forwarded TurnComplete");
        assert!(matches!(
            forwarded_turn_complete.msg,
            EventMsg::TurnComplete(_)
        ));
        assert!(
            harness.event_rx.try_recv().is_err(),
            "expected empty TurnComplete not to finish the round"
        );
        assert!(
            matches!(
                harness
                    .action_rx
                    .try_recv()
                    .expect("expected RetryContinue action"),
                RecoveryAction::RetryContinue { attempt: 1 }
            ),
            "expected retry attempt 1"
        );
        assert!(
            !harness.recovery.has_finished_round,
            "round should still be running"
        );

        handle_codex_event(
            Event {
                id: "activity".into(),
                msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    delta: "hello".to_string(),
                }),
            },
            &mut harness.recovery,
            &harness.event_tx,
        );

        let activity = harness
            .event_rx
            .try_recv()
            .expect("expected forwarded activity");
        assert!(matches!(activity.msg, EventMsg::AgentMessageDelta(_)));

        let recovered = harness
            .event_rx
            .try_recv()
            .expect("expected recovered marker");
        assert!(matches!(
            recovered.msg,
            EventMsg::PotterStreamRecoveryRecovered
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(unix, windows))]
    use crate::app_server::test_support::lock_dummy_codex_test;
    #[cfg(any(unix, windows))]
    use crate::app_server::test_support::write_dummy_codex_script;
    use codex_protocol::models::MessagePhase;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    #[cfg(unix)]
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc::unbounded_channel;
    use tokio::time::Duration;
    use tokio::time::timeout;

    fn recovery_context(event_mode: AppServerEventMode) -> StreamRecoveryContext {
        let (recovery_action_tx, _recovery_action_rx) = unbounded_channel::<RecoveryAction>();
        StreamRecoveryContext {
            stream_recovery: PotterStreamRecovery::new(),
            recovery_action_tx,
            pending_continue_retry: None,
            active_turn_id: None,
            last_context_compaction_turn_id: None,
            has_sent_turn_start: true,
            has_finished_round: false,
            round_started_at: Instant::now(),
            last_turn_start_was_recovery_continue: false,
            event_mode,
            bypass_approvals_and_sandbox: false,
        }
    }

    #[test]
    fn parse_codex_cli_version_extracts_semver_from_codex_version_output() {
        let version = parse_codex_cli_version("codex-cli 0.121.0\n").expect("parse version");
        assert_eq!(version, "0.121.0");

        let version = parse_codex_cli_version("codex-cli v1.2.3").expect("parse version");
        assert_eq!(version, "1.2.3");

        let version =
            parse_codex_cli_version("codex-cli (0.121.0-alpha.1)").expect("parse version");
        assert_eq!(version, "0.121.0-alpha.1");
    }

    #[test]
    fn parse_codex_cli_version_rejects_empty_or_unparseable_outputs() {
        parse_codex_cli_version("").expect_err("empty output should fail");
        parse_codex_cli_version("codex-cli").expect_err("no version token should fail");
        parse_codex_cli_version("codex-cli not-a-version").expect_err("bad version should fail");
    }

    #[test]
    fn request_permission_profile_maps_path_entries_to_legacy_read_write_roots() {
        let profile = serde_json::from_value(serde_json::json!({
            "network": {
                "enabled": true
            },
            "fileSystem": {
                "read": ["/tmp/readable"],
                "write": ["/tmp/writable"],
                "entries": [
                    {
                        "path": {
                            "type": "path",
                            "path": "/tmp/readable"
                        },
                        "access": "read"
                    },
                    {
                        "path": {
                            "type": "path",
                            "path": "/tmp/writable"
                        },
                        "access": "write"
                    },
                    {
                        "path": {
                            "type": "glob_pattern",
                            "pattern": "**/*.rs"
                        },
                        "access": "read"
                    }
                ]
            }
        }))
        .expect("deserialize upstream request permission profile");

        let mapped = request_permission_profile_from_upstream(profile);

        assert_eq!(
            mapped,
            ProtocolRequestPermissionProfile {
                network: Some(codex_protocol::models::NetworkPermissions {
                    enabled: Some(true),
                }),
                file_system: Some(codex_protocol::models::FileSystemPermissions {
                    read: Some(vec![
                        codex_protocol::AbsolutePathBuf::from_absolute_path("/tmp/readable")
                            .expect("absolute read path"),
                    ]),
                    write: Some(vec![
                        codex_protocol::AbsolutePathBuf::from_absolute_path("/tmp/writable")
                            .expect("absolute write path"),
                    ]),
                }),
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_upstream_client_info_uses_codex_tui_name_and_cli_version() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        write_dummy_codex_script(
            &codex_bin,
            r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--version" ]]; then
  echo "codex-cli 0.121.0"
  exit 0
fi

echo "unexpected args: $*" >&2
exit 1
"#,
        );

        let client_info =
            resolve_upstream_client_info(codex_bin.to_str().expect("utf-8 path")).await;
        assert_eq!(
            client_info,
            ClientInfo {
                name: "codex-tui".to_string(),
                title: None,
                version: "0.121.0".to_string(),
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_upstream_client_info_falls_back_to_codex_potter_version() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        write_dummy_codex_script(
            &codex_bin,
            r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--version" ]]; then
  echo "boom" >&2
  exit 17
fi

echo "unexpected args: $*" >&2
exit 1
"#,
        );

        let client_info =
            resolve_upstream_client_info(codex_bin.to_str().expect("utf-8 path")).await;
        assert_eq!(
            client_info,
            ClientInfo {
                name: "codex-tui".to_string(),
                title: None,
                version: codex_tui::CODEX_POTTER_VERSION.to_string(),
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_upstream_client_info_caches_version_per_codex_bin() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let version_count = temp.path().join("version-count");

        write_dummy_codex_script(
            &codex_bin,
            format!(
                r#"#!/usr/bin/env bash
set -euo pipefail

COUNT_FILE={count_file:?}

if [[ "${{1:-}}" == "--version" ]]; then
  count=0
  if [[ -f "$COUNT_FILE" ]]; then
    count="$(cat "$COUNT_FILE")"
  fi
  count=$((count + 1))
  printf '%s\n' "$count" > "$COUNT_FILE"
  echo "codex-cli 0.121.0"
  exit 0
fi

echo "unexpected args: $*" >&2
exit 1
"#,
                count_file = version_count.display().to_string(),
            ),
        );

        let codex_bin = codex_bin.to_str().expect("utf-8 path");
        let first = resolve_upstream_client_info(codex_bin).await;
        let second = resolve_upstream_client_info(codex_bin).await;

        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(&version_count).expect("read version count"),
            "1\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_upstream_client_info_does_not_cache_fallback_version() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let version_count = temp.path().join("version-count");

        write_dummy_codex_script(
            &codex_bin,
            format!(
                r#"#!/usr/bin/env bash
set -euo pipefail

COUNT_FILE={count_file:?}
count=0
if [[ -f "$COUNT_FILE" ]]; then
  count="$(cat "$COUNT_FILE")"
fi
count=$((count + 1))
printf '%s\n' "$count" > "$COUNT_FILE"

if [[ "${{1:-}}" != "--version" ]]; then
  echo "unexpected args: $*" >&2
  exit 1
fi

if [[ "$count" -eq 1 ]]; then
  echo "boom" >&2
  exit 17
fi

echo "codex-cli 0.121.0"
"#,
                count_file = version_count.display().to_string(),
            ),
        );

        let codex_bin = codex_bin.to_str().expect("utf-8 path");
        let fallback = resolve_upstream_client_info(codex_bin).await;
        let resolved = resolve_upstream_client_info(codex_bin).await;

        assert_eq!(
            fallback.version,
            codex_tui::CODEX_POTTER_VERSION.to_string()
        );
        assert_eq!(resolved.version, "0.121.0");
        assert_eq!(
            std::fs::read_to_string(&version_count).expect("read version count"),
            "2\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_initializes_upstream_app_server_with_codex_tui_client_info() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--version" ]]; then
  echo "codex-cli 0.121.0"
  exit 0
fi

if [[ "${1:-}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

IFS= read -r initialize
echo "$initialize" | grep -q '"method":"initialize"' || {
  echo "expected initialize, got: $initialize" >&2
  exit 1
}
echo "$initialize" | grep -q '"name":"codex-tui"' || {
  echo "expected codex-tui clientInfo name, got: $initialize" >&2
  exit 1
}
echo "$initialize" | grep -q '"title":null' || {
  echo "expected null clientInfo title, got: $initialize" >&2
  exit 1
}
echo "$initialize" | grep -q '"version":"0.121.0"' || {
  echo "expected parsed codex version, got: $initialize" >&2
  exit 1
}
echo "$initialize" | grep -q '"experimentalApi":true' || {
  echo "expected experimentalApi capability, got: $initialize" >&2
  exit 1
}
echo '{"id":1,"result":{"userAgent":"test-agent","platformFamily":"unix","platformOs":"test-os"}}'

IFS= read -r _line
IFS= read -r _line
echo '{"id":2,"result":{"thread":{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"readOnly"},"reasoningEffort":null}}'

while IFS= read -r _line; do
  :
done
"#;

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, _event_rx) = unbounded_channel::<Event>();
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        drop(op_tx);

        timeout(
            Duration::from_secs(5),
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            ),
        )
        .await
        .expect("backend timed out")
        .expect("backend failed");
    }

    #[test]
    fn thread_start_settings_into_params_preserves_model_override() {
        let params = ThreadStartSettings {
            model: Some("o3".to_string()),
            developer_instructions: None,
            sandbox_mode: None,
            cwd: None,
        }
        .into_params();

        assert_eq!(params.model.as_deref(), Some("o3"));
    }

    #[test]
    fn thread_resume_settings_into_params_preserves_model_override() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("thread id");
        let params = ThreadResumeSettings {
            thread_id,
            model: Some("o3".to_string()),
            developer_instructions: None,
            sandbox_mode: None,
            cwd: None,
        }
        .into_params();

        assert_eq!(params.thread_id, thread_id.to_string());
        assert_eq!(params.model.as_deref(), Some("o3"));
    }

    #[test]
    fn default_collaboration_mode_matches_upstream_default_shape() {
        let value = serde_json::to_value(default_collaboration_mode(
            "gpt-5.4",
            Some(ReasoningEffort::High),
        ))
        .expect("serialize collaboration mode");

        assert_eq!(
            value,
            serde_json::json!({
                "mode": "default",
                "settings": {
                    "model": "gpt-5.4",
                    "reasoning_effort": "high",
                    "developer_instructions": null,
                },
            })
        );
    }

    #[test]
    fn summarize_response_result_truncates_long_payloads() {
        let payload = serde_json::json!({
            "message": "x".repeat(600),
        });

        let summary = summarize_response_result(&payload);

        assert!(summary.starts_with("{\"message\":\""));
        assert!(summary.ends_with("...[truncated]"));
        assert!(summary.len() < 450, "summary should stay bounded");
    }

    #[test]
    fn typed_agent_message_item_completed_emits_agent_message_event() {
        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);

        handle_typed_item_completed(
            UpstreamItemCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: serde_json::json!({
                    "type": "agentMessage",
                    "id": "msg-1",
                    "text": "bridge completed commentary",
                    "phase": "commentary"
                }),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge agent message item");

        let event = event_rx.try_recv().expect("expected bridged agent message");
        assert_eq!(event.id, "msg-1");
        let EventMsg::AgentMessage(agent_message) = event.msg else {
            panic!("expected AgentMessage event");
        };
        assert_eq!(agent_message.message, "bridge completed commentary");
        assert_eq!(agent_message.phase, Some(MessagePhase::Commentary));
        assert!(
            event_rx.try_recv().is_err(),
            "expected no extra events for completed agent message"
        );
    }

    #[test]
    fn typed_reasoning_item_completed_emits_legacy_reasoning_events() {
        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);

        handle_typed_item_completed(
            UpstreamItemCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: serde_json::json!({
                    "type": "reasoning",
                    "id": "reasoning-1",
                    "summary": [
                        "**Inspecting for code duplication**\n\nSearch duplicated helpers.",
                        "**Planning next edit**\n\nKeep the seam minimal."
                    ],
                    "content": [
                        "raw chain of thought"
                    ]
                }),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge reasoning item");

        let first = event_rx
            .try_recv()
            .expect("expected first reasoning summary");
        assert_eq!(first.id, "reasoning-1:summary:0");
        let EventMsg::AgentReasoning(summary) = first.msg else {
            panic!("expected AgentReasoning summary event");
        };
        assert_eq!(
            summary.text,
            "**Inspecting for code duplication**\n\nSearch duplicated helpers."
        );

        let second = event_rx
            .try_recv()
            .expect("expected second reasoning summary");
        assert_eq!(second.id, "reasoning-1:summary:1");
        let EventMsg::AgentReasoning(summary) = second.msg else {
            panic!("expected AgentReasoning summary event");
        };
        assert_eq!(
            summary.text,
            "**Planning next edit**\n\nKeep the seam minimal."
        );

        let third = event_rx.try_recv().expect("expected raw reasoning content");
        assert_eq!(third.id, "reasoning-1:content:0");
        let EventMsg::AgentReasoningRawContent(content) = third.msg else {
            panic!("expected AgentReasoningRawContent event");
        };
        assert_eq!(content.text, "raw chain of thought");

        assert!(
            event_rx.try_recv().is_err(),
            "expected no extra events for completed reasoning item"
        );
    }

    #[test]
    fn typed_reasoning_summary_part_added_emits_section_break_event() {
        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);

        handle_typed_notification(
            JSONRPCNotification {
                method: "item/reasoning/summaryPartAdded".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "reasoning-1",
                    "summaryIndex": 2
                })),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge reasoning summary part boundary");

        let event = event_rx.try_recv().expect("expected section break event");
        assert_eq!(event.id, "turn-1");
        let EventMsg::AgentReasoningSectionBreak(section_break) = event.msg else {
            panic!("expected AgentReasoningSectionBreak event");
        };
        assert_eq!(section_break.item_id, "reasoning-1");
        assert_eq!(section_break.summary_index, 2);
        assert!(
            event_rx.try_recv().is_err(),
            "expected no extra events for reasoning summary boundary"
        );
    }

    #[test]
    fn typed_mcp_startup_failure_emits_warning_event() {
        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);

        handle_typed_notification(
            JSONRPCNotification {
                method: "mcpServer/startupStatus/updated".to_string(),
                params: Some(serde_json::json!({
                    "name": "bad_mcp",
                    "status": "failed",
                    "error": "MCP client for `bad_mcp` failed to start: startup failed"
                })),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge MCP startup failure");

        let event = event_rx.try_recv().expect("expected warning event");
        let EventMsg::Warning(WarningEvent { message }) = event.msg else {
            panic!("expected Warning event");
        };
        assert_eq!(
            message,
            "MCP client for `bad_mcp` failed to start: startup failed"
        );
        assert!(
            event_rx.try_recv().is_err(),
            "expected no extra events for MCP startup failure"
        );
    }

    #[test]
    fn typed_permission_request_hook_notifications_emit_hook_events() {
        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);

        handle_typed_notification(
            JSONRPCNotification {
                method: "hook/started".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "run": {
                        "id": "hook-1",
                        "eventName": "permissionRequest",
                        "handlerType": "command",
                        "executionMode": "sync",
                        "scope": "turn",
                        "sourcePath": "/tmp/hooks.json",
                        "displayOrder": 0,
                        "status": "running",
                        "statusMessage": "checking permissions",
                        "startedAt": 10,
                        "completedAt": null,
                        "durationMs": null,
                        "entries": []
                    }
                })),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge permission request hook start");

        handle_typed_notification(
            JSONRPCNotification {
                method: "hook/completed".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "run": {
                        "id": "hook-1",
                        "eventName": "permissionRequest",
                        "handlerType": "command",
                        "executionMode": "sync",
                        "scope": "turn",
                        "sourcePath": "/tmp/hooks.json",
                        "displayOrder": 0,
                        "status": "completed",
                        "statusMessage": null,
                        "startedAt": 10,
                        "completedAt": 15,
                        "durationMs": 5,
                        "entries": [{
                            "kind": "context",
                            "text": "permissions checked"
                        }]
                    }
                })),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge permission request hook completion");

        let started = event_rx.try_recv().expect("expected hook start event");
        assert_eq!(started.id, "hook-1");
        let EventMsg::HookStarted(started) = started.msg else {
            panic!("expected HookStarted event");
        };
        assert_eq!(started.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(started.run.event_name, HookEventName::PermissionRequest);

        let completed = event_rx.try_recv().expect("expected hook completed event");
        assert_eq!(completed.id, "hook-1");
        let EventMsg::HookCompleted(completed) = completed.msg else {
            panic!("expected HookCompleted event");
        };
        assert_eq!(completed.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(completed.run.event_name, HookEventName::PermissionRequest);
        assert_eq!(
            completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Context,
                text: "permissions checked".to_string(),
            }]
        );
        assert!(
            event_rx.try_recv().is_err(),
            "expected no extra hook events"
        );
    }

    #[test]
    fn typed_context_compaction_events_update_last_context_compaction_turn_id() {
        {
            let (event_tx, mut event_rx) = unbounded_channel::<Event>();
            let mut recovery = recovery_context(AppServerEventMode::Interactive);

            handle_typed_item_completed(
                UpstreamItemCompletedNotification {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item: serde_json::json!({
                        "type": "contextCompaction",
                        "id": "compaction-1"
                    }),
                },
                &mut recovery,
                &event_tx,
            )
            .expect("bridge context compaction item");

            let event = event_rx
                .try_recv()
                .expect("expected bridged context compaction");
            assert_eq!(event.id, "compaction-1");
            assert!(matches!(event.msg, EventMsg::ContextCompacted(_)));
            assert_eq!(
                recovery.last_context_compaction_turn_id.as_deref(),
                Some("turn-1")
            );
            assert!(
                event_rx.try_recv().is_err(),
                "expected no extra events for completed context compaction"
            );
        }

        {
            let (event_tx, mut event_rx) = unbounded_channel::<Event>();
            let mut recovery = recovery_context(AppServerEventMode::Interactive);

            handle_typed_notification(
                JSONRPCNotification {
                    method: "thread/compacted".to_string(),
                    params: Some(serde_json::json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1"
                    })),
                },
                &mut recovery,
                &event_tx,
            )
            .expect("bridge deprecated thread/compacted");

            let event = event_rx
                .try_recv()
                .expect("expected bridged deprecated compaction notification");
            assert_eq!(event.id, "turn-1");
            assert!(matches!(event.msg, EventMsg::ContextCompacted(_)));
            assert_eq!(
                recovery.last_context_compaction_turn_id.as_deref(),
                Some("turn-1")
            );
            assert!(
                event_rx.try_recv().is_err(),
                "expected no extra events for deprecated compaction notification"
            );
        }
    }

    #[test]
    fn typed_context_compaction_item_dedupes_deprecated_thread_compacted_notification() {
        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);

        handle_typed_item_completed(
            UpstreamItemCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: serde_json::json!({
                    "type": "contextCompaction",
                    "id": "compaction-1"
                }),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge context compaction item");

        handle_typed_notification(
            JSONRPCNotification {
                method: "thread/compacted".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1"
                })),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge deprecated thread/compacted");

        let event = event_rx
            .try_recv()
            .expect("expected single bridged context compaction event");
        assert_eq!(event.id, "compaction-1");
        assert!(matches!(event.msg, EventMsg::ContextCompacted(_)));
        assert!(
            event_rx.try_recv().is_err(),
            "expected duplicate deprecated notification to be suppressed"
        );
    }

    #[test]
    fn typed_turn_plan_updated_notification_emits_plan_update_event() {
        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);
        recovery.active_turn_id = Some("turn-1".to_string());

        handle_typed_notification(
            JSONRPCNotification {
                method: "turn/plan/updated".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "explanation": "bridge plan updates",
                    "plan": [
                        {
                            "step": "Inspect docs",
                            "status": "inProgress"
                        },
                        {
                            "step": "Patch bridge",
                            "status": "pending"
                        }
                    ]
                })),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge turn plan update");

        let event = event_rx.try_recv().expect("expected bridged plan update");
        assert_eq!(event.id, "turn-1");
        let EventMsg::PlanUpdate(update) = event.msg else {
            panic!("expected PlanUpdate event");
        };
        assert_eq!(update.explanation.as_deref(), Some("bridge plan updates"));
        assert_eq!(update.plan.len(), 2);
        assert_eq!(update.plan[0].step, "Inspect docs");
        assert!(matches!(update.plan[0].status, StepStatus::InProgress));
        assert_eq!(update.plan[1].step, "Patch bridge");
        assert!(matches!(update.plan[1].status, StepStatus::Pending));
        assert!(
            event_rx.try_recv().is_err(),
            "expected no extra events for plan update notification"
        );
    }

    #[test]
    fn typed_command_execution_output_delta_notification_emits_output_delta_event() {
        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);

        handle_typed_notification(
            JSONRPCNotification {
                method: "item/commandExecution/outputDelta".to_string(),
                params: Some(serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "call-1",
                    "delta": "hello\n"
                })),
            },
            &mut recovery,
            &event_tx,
        )
        .expect("bridge command execution output delta");

        let event = event_rx.try_recv().expect("expected bridged output delta");
        assert_eq!(event.id, "turn-1");
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected ExecCommandOutputDelta event");
        };
        assert_eq!(
            delta,
            ExecCommandOutputDeltaEvent {
                call_id: "call-1".to_string(),
                stream: ExecOutputStream::Stdout,
                chunk: b"hello\n".to_vec(),
            }
        );
        assert!(
            event_rx.try_recv().is_err(),
            "expected no extra events for output delta notification"
        );
    }

    #[test]
    fn turn_complete_clears_active_turn_id_only_when_matching() {
        let (event_tx, _event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);
        recovery.active_turn_id = Some("turn-new".to_string());

        handle_codex_event(
            Event {
                id: "tc-1".to_string(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "turn-old".to_string(),
                    last_agent_message: None,
                }),
            },
            &mut recovery,
            &event_tx,
        );

        assert_eq!(recovery.active_turn_id.as_deref(), Some("turn-new"));

        handle_codex_event(
            Event {
                id: "tc-2".to_string(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "turn-new".to_string(),
                    last_agent_message: None,
                }),
            },
            &mut recovery,
            &event_tx,
        );

        assert_eq!(recovery.active_turn_id, None);
    }

    #[test]
    fn turn_aborted_clears_active_turn_id_only_when_matching() {
        let (event_tx, _event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);
        recovery.active_turn_id = Some("turn-new".to_string());

        handle_codex_event(
            Event {
                id: "ta-1".to_string(),
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: Some("turn-old".to_string()),
                    reason: TurnAbortReason::Interrupted,
                }),
            },
            &mut recovery,
            &event_tx,
        );

        assert_eq!(recovery.active_turn_id.as_deref(), Some("turn-new"));

        handle_codex_event(
            Event {
                id: "ta-2".to_string(),
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: Some("turn-new".to_string()),
                    reason: TurnAbortReason::Interrupted,
                }),
            },
            &mut recovery,
            &event_tx,
        );

        assert_eq!(recovery.active_turn_id, None);
    }

    #[test]
    fn active_turn_id_is_preserved_by_replaced_without_turn_id() {
        let (event_tx, _event_rx) = unbounded_channel::<Event>();
        let mut recovery = recovery_context(AppServerEventMode::Interactive);
        recovery.active_turn_id = Some("turn-1".to_string());

        handle_codex_event(
            Event {
                id: "ta-1".to_string(),
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: None,
                    reason: TurnAbortReason::Replaced,
                }),
            },
            &mut recovery,
            &event_tx,
        );

        assert_eq!(recovery.active_turn_id.as_deref(), Some("turn-1"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_emits_round_finished_for_typed_turn_completed() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r initialize
echo "$initialize" | grep -q '"method":"initialize"' || {
  echo "expected initialize, got: $initialize" >&2
  exit 1
}
echo "$initialize" | grep -q '"experimentalApi":true' || {
  echo "expected experimentalApi capability, got: $initialize" >&2
  exit 1
}
echo '{"id":1,"result":{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{"id":2,"result":{"thread":{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"readOnly"},"reasoningEffort":null}}'

# turn/start request
IFS= read -r turn_start
echo "$turn_start" | grep -q '"method":"turn/start"' || {
  echo "expected turn/start, got: $turn_start" >&2
  exit 1
}
echo "$turn_start" | grep -q '"collaborationMode":' || {
  echo "expected collaborationMode, got: $turn_start" >&2
  exit 1
}
echo "$turn_start" | grep -q '"mode":"default"' || {
  echo "expected default collaboration mode, got: $turn_start" >&2
  exit 1
}
echo "$turn_start" | grep -q '"model":"test-model"' || {
  echo "expected collaboration mode model override, got: $turn_start" >&2
  exit 1
}
echo "$turn_start" | grep -q '"reasoning_effort":null' || {
  echo "expected null collaboration mode reasoning_effort, got: $turn_start" >&2
  exit 1
}
echo "$turn_start" | grep -q '"developer_instructions":null' || {
  echo "expected null collaboration mode developer_instructions, got: $turn_start" >&2
  exit 1
}
echo '{"method":"turn/started","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"inProgress","error":null}}}'
echo '{"id":3,"result":{"turn":{"id":"turn-1"}}}'
echo '{"method":"turn/completed","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"completed","error":null}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#;

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_round_finished = timeout(Duration::from_secs(5), async {
            let mut saw_turn_started = false;
            while let Some(event) = event_rx.recv().await {
                match event.msg {
                    EventMsg::TurnStarted(TurnStartedEvent { turn_id, .. }) => {
                        assert_eq!(turn_id, "turn-1");
                        saw_turn_started = true;
                    }
                    EventMsg::PotterRoundFinished {
                        outcome: PotterRoundOutcome::Completed,
                        ..
                    } => {
                        return saw_turn_started;
                    }
                    _ => {}
                }
            }
            false
        })
        .await;

        assert_eq!(
            saw_round_finished,
            Ok(true),
            "did not observe TurnStarted + PotterRoundFinished(Completed)"
        );

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_interrupt_after_completed_turn_without_active_turn_is_noop() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("unexpected-interrupt-after-complete");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# turn/start request
IFS= read -r _line
echo '{{"method":"turn/started","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"inProgress","error":null}}}}}}'
echo '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'
echo '{{"method":"turn/completed","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"completed","error":null}}}}}}'

if IFS= read -r -t 1 extra; then
  touch "$MARKER"
  echo "unexpected request after completed turn: $extra" >&2
  exit 1
fi

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#,
            marker = marker.display(),
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_round_finished = timeout(Duration::from_secs(5), async {
            while let Some(event) = event_rx.recv().await {
                if matches!(
                    event.msg,
                    EventMsg::PotterRoundFinished {
                        outcome: PotterRoundOutcome::Completed,
                        ..
                    }
                ) {
                    return true;
                }
            }
            false
        })
        .await;

        assert_eq!(
            saw_round_finished,
            Ok(true),
            "did not observe PotterRoundFinished(Completed)"
        );

        op_tx.send(Op::Interrupt).expect("send late interrupt");
        let saw_unexpected_interrupt = timeout(Duration::from_millis(500), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            true
        })
        .await
        .unwrap_or(false);
        assert!(
            !saw_unexpected_interrupt,
            "backend sent turn/interrupt without an active turn"
        );

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_accepts_initialize_response_without_codex_home() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r initialize
echo "$initialize" | grep -q '"method":"initialize"' || {
  echo "expected initialize, got: $initialize" >&2
  exit 1
}
echo '{"id":1,"result":{"userAgent":"test-agent","platformFamily":"unix","platformOs":"test-os"}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{"id":2,"result":{"thread":{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"readOnly"},"reasoningEffort":null}}'

# turn/start request
IFS= read -r _line
echo '{"method":"turn/started","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"inProgress","error":null}}}'
echo '{"id":3,"result":{"turn":{"id":"turn-1"}}}'
echo '{"method":"turn/completed","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"completed","error":null}}}'

while IFS= read -r _line; do
  :
done
"#;

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_round_finished = timeout(Duration::from_secs(5), async {
            while let Some(event) = event_rx.recv().await {
                if let EventMsg::PotterRoundFinished {
                    outcome: PotterRoundOutcome::Completed,
                    ..
                } = event.msg
                {
                    return true;
                }
            }
            false
        })
        .await;

        assert_eq!(
            saw_round_finished,
            Ok(true),
            "did not observe PotterRoundFinished(Completed)"
        );

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_reports_initialize_decode_failure_with_payload_context() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

IFS= read -r _line
echo '{"id":1,"result":null}'
"#;

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let (fatal_exit_tx, mut fatal_exit_rx) = unbounded_channel::<String>();
        let (_op_tx, op_rx) = unbounded_channel::<Op>();

        run_app_server_backend(
            AppServerBackendConfig {
                codex_bin: codex_bin.display().to_string(),
                developer_instructions: None,
                launch: AppServerLaunchConfig {
                    spawn_sandbox: None,
                    thread_sandbox: None,
                    bypass_approvals_and_sandbox: false,
                },
                upstream_cli_args: Default::default(),
                codex_home: None,
                thread_cwd: None,
                resume_thread_id: None,
                event_mode: AppServerEventMode::Interactive,
            },
            op_rx,
            event_tx,
            fatal_exit_tx,
        )
        .await
        .expect("backend should swallow initialize decode failures");

        let error_event = timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("event timed out")
            .expect("event channel closed");
        let EventMsg::Error(ErrorEvent { message, .. }) = error_event.msg else {
            panic!("expected Error event, got {:?}", error_event.msg);
        };
        assert!(
            message.contains("decode initialize response"),
            "unexpected error message: {message:?}"
        );
        assert!(
            message.contains("payload: null"),
            "missing response payload context: {message:?}"
        );

        let finished_event = timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("event timed out")
            .expect("event channel closed");
        let EventMsg::PotterRoundFinished {
            outcome:
                PotterRoundOutcome::TaskFailed {
                    message: finished_message,
                },
            ..
        } = finished_event.msg
        else {
            panic!(
                "expected PotterRoundFinished(TaskFailed), got {:?}",
                finished_event.msg
            );
        };
        assert_eq!(finished_message, message);
        assert!(
            fatal_exit_rx.try_recv().is_err(),
            "expected no fatal exit message"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn backend_initializes_from_windows_cmd_shim() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex.cmd");

        let script = r#"@echo off
setlocal EnableExtensions DisableDelayedExpansion

if /I not "%~1"=="app-server" (
  echo expected app-server, got: %* 1>&2
  exit /b 1
)

set /p ignored=
echo {"id":1,"result":{"userAgent":"test-agent","platformFamily":"windows","platformOs":"windows"}}
set /p ignored=
set /p ignored=
echo {"id":2,"result":{"thread":{"id":"00000000-0000-0000-0000-000000000000","preview":"","modelProvider":"test-provider","createdAt":0,"updatedAt":0,"path":"rollout.jsonl","cwd":"project","cliVersion":"0.0.0","source":"appServer","gitInfo":null,"turns":[]},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"readOnly"},"reasoningEffort":null}}
"#;

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        drop(op_tx);

        timeout(
            Duration::from_secs(5),
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            ),
        )
        .await
        .expect("backend timed out")
        .expect("backend failed");

        let session_event = timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("event timed out")
            .expect("event channel closed");
        assert!(matches!(
            session_event.msg,
            EventMsg::SessionConfigured(SessionConfiguredEvent { model, .. })
                if model == "test-model"
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_ignores_unexpected_error_notification_schema() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{"id":1,"result":{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{"method":"error","params":"Token data is not available."}'
echo '{"id":2,"result":{"thread":{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"readOnly"},"reasoningEffort":null}}'

# turn/start request
IFS= read -r _line
echo '{"method":"turn/started","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"inProgress","error":null}}}'
echo '{"id":3,"result":{"turn":{"id":"turn-1"}}}'
echo '{"method":"turn/completed","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"completed","error":null}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#;

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let (_fatal_exit_tx, mut fatal_exit_rx) = unbounded_channel::<String>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_round_finished = timeout(Duration::from_secs(5), async {
            let mut saw_turn_started = false;
            let mut saw_decode_warning = false;
            while let Some(event) = event_rx.recv().await {
                match event.msg {
                    EventMsg::TurnStarted(TurnStartedEvent { turn_id, .. }) => {
                        assert_eq!(turn_id, "turn-1");
                        saw_turn_started = true;
                    }
                    EventMsg::Warning(WarningEvent { message }) => {
                        if message.contains("Failed to decode upstream `error` notification") {
                            saw_decode_warning = true;
                        }
                    }
                    EventMsg::PotterRoundFinished {
                        outcome: PotterRoundOutcome::Completed,
                        ..
                    } => {
                        return saw_turn_started && saw_decode_warning;
                    }
                    _ => {}
                }
            }
            false
        })
        .await;

        assert_eq!(
            saw_round_finished,
            Ok(true),
            "did not observe Warning + TurnStarted + PotterRoundFinished(Completed)"
        );

        assert!(fatal_exit_rx.try_recv().is_err(), "expected no fatal exit");

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");
    }

    #[tokio::test]
    async fn backend_reports_startup_failure_as_task_failed_round() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_codex = temp.path().join("missing-codex");

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();
        let (fatal_exit_tx, mut fatal_exit_rx) = unbounded_channel::<String>();

        let (_op_tx, op_rx) = unbounded_channel::<Op>();
        run_app_server_backend(
            AppServerBackendConfig {
                codex_bin: missing_codex.display().to_string(),
                developer_instructions: None,
                launch: AppServerLaunchConfig {
                    spawn_sandbox: None,
                    thread_sandbox: None,
                    bypass_approvals_and_sandbox: false,
                },
                upstream_cli_args: Default::default(),
                codex_home: None,
                thread_cwd: None,
                resume_thread_id: None,
                event_mode: AppServerEventMode::Interactive,
            },
            op_rx,
            event_tx,
            fatal_exit_tx,
        )
        .await
        .expect("backend should swallow startup failures");

        let error_event = timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("event timed out")
            .expect("event channel closed");
        let EventMsg::Error(ErrorEvent { message, .. }) = error_event.msg else {
            panic!("expected Error event, got {:?}", error_event.msg);
        };
        assert!(
            message.starts_with("Failed to run `codex app-server`:"),
            "unexpected error message: {message:?}"
        );

        let finished_event = timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("event timed out")
            .expect("event channel closed");
        let EventMsg::PotterRoundFinished {
            outcome:
                PotterRoundOutcome::TaskFailed {
                    message: finished_message,
                },
            ..
        } = finished_event.msg
        else {
            panic!(
                "expected PotterRoundFinished(TaskFailed), got {:?}",
                finished_event.msg
            );
        };
        assert_eq!(finished_message, message);

        assert!(
            fatal_exit_rx.try_recv().is_err(),
            "expected no fatal exit message"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_surfaces_server_requests_and_guardian_review_notifications() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{"id":1,"result":{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{"id":2,"result":{"thread":{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"readOnly"},"reasoningEffort":null}}'

# turn/start request
IFS= read -r _line
echo '{"method":"turn/started","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"inProgress","error":null}}}'
echo '{"id":3,"result":{"turn":{"id":"turn-1"}}}'

# item/tool/requestUserInput server request
echo '{"id":10,"method":"item/tool/requestUserInput","params":{"threadId":"00000000-0000-0000-0000-000000000000","turnId":"turn-1","itemId":"tool-req-user-input","questions":[{"id":"q1","header":"Choice","question":"Pick one","options":[{"label":"A","description":"Option A"}]}]}}'
IFS= read -r response
echo "$response" | grep -q '"id":10' || { echo "missing response id 10: $response" >&2; exit 1; }
echo "$response" | grep -q '"answers":{}' || { echo "missing empty answers: $response" >&2; exit 1; }

# item/permissions/requestApproval server request
echo '{"id":11,"method":"item/permissions/requestApproval","params":{"threadId":"00000000-0000-0000-0000-000000000000","turnId":"turn-1","itemId":"tool-req-perms","cwd":"/tmp/project","reason":"need permissions","permissions":{"network":{"enabled":true},"fileSystem":{"read":["/tmp"],"write":[]}}}}'
IFS= read -r response
echo "$response" | grep -q '"id":11' || { echo "missing response id 11: $response" >&2; exit 1; }
echo "$response" | grep -q '"scope":"turn"' || { echo "missing scope=turn: $response" >&2; exit 1; }

# mcpServer/elicitation/request server request
echo '{"id":12,"method":"mcpServer/elicitation/request","params":{"threadId":"00000000-0000-0000-0000-000000000000","turnId":"turn-1","serverName":"example-server","mode":"form","_meta":null,"message":"Please fill this","requestedSchema":{"type":"object","properties":{}}}}'
IFS= read -r response
echo "$response" | grep -q '"id":12' || { echo "missing response id 12: $response" >&2; exit 1; }
echo "$response" | grep -q '"action":"cancel"' || { echo "missing action=cancel: $response" >&2; exit 1; }

# item/autoApprovalReview notifications
echo '{"method":"item/autoApprovalReview/started","params":{"threadId":"00000000-0000-0000-0000-000000000000","turnId":"turn-1","targetItemId":"tool-req-perms","review":{"status":"inProgress","riskScore":null,"riskLevel":null,"rationale":null},"action":{"type":"example"}}}'
echo '{"method":"item/autoApprovalReview/completed","params":{"threadId":"00000000-0000-0000-0000-000000000000","turnId":"turn-1","targetItemId":"tool-req-perms","review":{"status":"approved","riskScore":10,"riskLevel":"low","rationale":"ok"},"action":{"type":"example"}}}'

echo '{"method":"turn/completed","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"completed","error":null}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#;

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_events = timeout(Duration::from_secs(10), async {
            let mut saw_request_user_input = false;
            let mut saw_request_permissions = false;
            let mut saw_elicitation_request = false;
            let mut saw_guardian_in_progress = false;
            let mut saw_guardian_approved = false;

            while let Some(event) = event_rx.recv().await {
                match event.msg {
                    EventMsg::RequestUserInput(ev) => {
                        assert_eq!(ev.call_id, "tool-req-user-input");
                        saw_request_user_input = true;
                    }
                    EventMsg::RequestPermissions(ev) => {
                        assert_eq!(ev.call_id, "tool-req-perms");
                        assert_eq!(
                            ev.cwd,
                            Some(
                                codex_protocol::AbsolutePathBuf::from_absolute_path("/tmp/project")
                                    .expect("absolute permission cwd")
                            )
                        );
                        saw_request_permissions = true;
                    }
                    EventMsg::ElicitationRequest(ev) => {
                        assert_eq!(ev.server_name, "example-server");
                        saw_elicitation_request = true;
                    }
                    EventMsg::GuardianAssessment(ev) => match ev.status {
                        GuardianAssessmentStatus::InProgress => {
                            saw_guardian_in_progress = true;
                        }
                        GuardianAssessmentStatus::Approved => {
                            saw_guardian_approved = true;
                        }
                        _ => {}
                    },
                    EventMsg::PotterRoundFinished {
                        outcome: PotterRoundOutcome::Completed,
                        ..
                    } => {
                        return saw_request_user_input
                            && saw_request_permissions
                            && saw_elicitation_request
                            && saw_guardian_in_progress
                            && saw_guardian_approved;
                    }
                    _ => {}
                }
            }

            false
        })
        .await;

        assert_eq!(
            saw_events,
            Ok(true),
            "did not observe tool request events + guardian review + PotterRoundFinished"
        );

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_emits_round_finished_for_typed_turn_failed_status() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{"id":1,"result":{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{"id":2,"result":{"thread":{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"readOnly"},"reasoningEffort":null}}'

# turn/start request
IFS= read -r _line
echo '{"id":3,"result":{"turn":{"id":"turn-1"}}}'
echo '{"method":"turn/completed","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"failed","error":{"message":"fatal error"}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#;

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_round_finished_message = timeout(Duration::from_secs(5), async {
            while let Some(event) = event_rx.recv().await {
                if let EventMsg::PotterRoundFinished {
                    outcome: PotterRoundOutcome::Fatal { message },
                    ..
                } = event.msg
                {
                    return Some(message);
                }
            }
            None
        })
        .await;

        assert_eq!(
            saw_round_finished_message,
            Ok(Some("fatal error".to_string())),
            "did not observe PotterRoundFinished(Fatal)"
        );

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_ignores_unknown_turn_completed_codex_error_info() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{"id":1,"result":{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{"id":2,"result":{"thread":{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"readOnly"},"reasoningEffort":null}}'

# turn/start request
IFS= read -r _line
echo '{"id":3,"result":{"turn":{"id":"turn-1"}}}'
echo '{"method":"turn/completed","params":{"threadId":"00000000-0000-0000-0000-000000000000","turn":{"id":"turn-1","items":[],"status":"failed","error":{"message":"fatal error","codexErrorInfo":{"brandNewProblem":{"httpStatusCode":503}}}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#;

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_round_finished_message = timeout(Duration::from_secs(5), async {
            while let Some(event) = event_rx.recv().await {
                if let EventMsg::PotterRoundFinished {
                    outcome: PotterRoundOutcome::Fatal { message },
                    ..
                } = event.msg
                {
                    return Some(message);
                }
            }
            None
        })
        .await;

        assert_eq!(
            saw_round_finished_message,
            Ok(Some("fatal error".to_string())),
            "did not observe PotterRoundFinished(Fatal)"
        );

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_emits_patch_apply_end_for_typed_file_change_item() {
        use std::collections::HashMap;
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");

        let patch = "--- a/foo.txt\\n+++ b/foo.txt\\n@@ -1 +1 @@\\n-a\\n+b\\n";
        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

PATCH="{patch}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# turn/start request
IFS= read -r _line
echo '{{"method":"turn/started","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"inProgress","error":null}}}}}}'
echo '{{"method":"item/started","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turnId":"turn-1","item":{{"type":"fileChange","id":"patch-1","changes":[{{"path":"foo.txt","kind":{{"type":"update"}},"diff":"'"$PATCH"'"}}],"status":"inProgress"}}}}}}'
echo '{{"method":"item/completed","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turnId":"turn-1","item":{{"type":"fileChange","id":"patch-1","changes":[{{"path":"foo.txt","kind":{{"type":"update"}},"diff":"'"$PATCH"'"}}],"status":"completed"}}}}}}'
echo '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'
echo '{{"method":"turn/completed","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"completed","error":null}}}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_patch_apply_end = timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if let EventMsg::PatchApplyEnd(ev) = event.msg {
                    return Some(ev);
                }
            }
            None
        })
        .await;

        let patch_apply_end = saw_patch_apply_end
            .expect("backend timed out")
            .expect("did not observe PatchApplyEnd");

        let mut expected_changes = HashMap::new();
        expected_changes.insert(
            PathBuf::from("foo.txt"),
            FileChange::Update {
                unified_diff: patch.replace("\\n", "\n"),
                move_path: None,
            },
        );

        assert_eq!(patch_apply_end.call_id, "patch-1");
        assert_eq!(patch_apply_end.turn_id, "turn-1");
        assert_eq!(patch_apply_end.stdout, "");
        assert_eq!(patch_apply_end.stderr, "");
        assert_eq!(patch_apply_end.success, true);
        assert_eq!(patch_apply_end.changes, expected_changes);

        drop(op_tx);

        timeout(Duration::from_secs(10), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_allows_another_turn_after_turn_complete() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("saw-second-turn-start");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# first turn/start request
IFS= read -r _line
echo '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'

	# signal completion for the first turn
	echo '{{"method":"turn/completed","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"completed","error":null}}}}}}'

# second turn/start request (should still be accepted)
IFS= read -r _line
touch "$MARKER"
echo '{{"id":4,"result":{{"turn":{{"id":"turn-2"}}}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#,
            marker = marker.display()
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_turn_complete = timeout(Duration::from_secs(5), async {
            let mut observed = false;
            while let Some(event) = event_rx.recv().await {
                if matches!(event.msg, EventMsg::TurnComplete(TurnCompleteEvent { .. })) {
                    observed = true;
                    break;
                }
            }
            observed
        })
        .await;
        assert_eq!(saw_turn_complete, Ok(true), "did not observe TurnComplete");

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "Continue".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send second user input");

        timeout(Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for dummy server marker");

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");

        assert!(
            marker.exists(),
            "dummy server did not observe second turn/start"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_turn_interrupt_during_turn_start_without_turn_id_requests_startup_interrupt() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("saw-startup-interrupt");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# turn/start request: withhold both turn/started and the response until the client interrupts.
IFS= read -r _line

# startup turn/interrupt request
IFS= read -r interrupt
echo "$interrupt" | grep -q '"method":"turn/interrupt"' || {{
  echo "expected turn/interrupt, got: $interrupt" >&2
  exit 1
}}
echo "$interrupt" | grep -q '"threadId":"00000000-0000-0000-0000-000000000000"' || {{
  echo "expected threadId in turn/interrupt, got: $interrupt" >&2
  exit 1
}}
echo "$interrupt" | grep -q '"turnId":""' || {{
  echo "expected empty turnId in startup interrupt, got: $interrupt" >&2
  exit 1
}}
touch "$MARKER"
echo '{{"id":4,"result":{{}}}}'

# Let the original turn/start request finish after the startup interrupt.
echo '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'
echo '{{"method":"turn/started","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"inProgress","error":null}}}}}}'
echo '{{"method":"turn/completed","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"interrupted","error":null}}}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#,
            marker = marker.display(),
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        op_tx.send(Op::Interrupt).expect("send interrupt");

        timeout(Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for dummy server marker");

        let saw_round_finished = timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if matches!(
                    event.msg,
                    EventMsg::PotterRoundFinished {
                        outcome: PotterRoundOutcome::Interrupted,
                        ..
                    }
                ) {
                    return true;
                }
            }
            false
        })
        .await;

        assert_eq!(
            saw_round_finished,
            Ok(true),
            "did not observe PotterRoundFinished(Interrupted)"
        );

        drop(op_tx);

        timeout(Duration::from_secs(10), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");

        assert!(
            marker.exists(),
            "dummy server did not observe startup turn/interrupt before turn/start response"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_turn_interrupt_during_turn_start_requests_turn_interrupt() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp
            .path()
            .join("saw-turn-interrupt-before-turn-start-response");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# turn/start request: announce the turn but withhold the response until the client interrupts.
IFS= read -r _line
echo '{{"method":"turn/started","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"inProgress","error":null}}}}}}'

# turn/interrupt request
IFS= read -r interrupt
echo "$interrupt" | grep -q '"method":"turn/interrupt"' || {{
  echo "expected turn/interrupt, got: $interrupt" >&2
  exit 1
}}
echo "$interrupt" | grep -q '"threadId":"00000000-0000-0000-0000-000000000000"' || {{
  echo "expected threadId in turn/interrupt, got: $interrupt" >&2
  exit 1
}}
echo "$interrupt" | grep -q '"turnId":"turn-1"' || {{
  echo "expected turnId=turn-1 in turn/interrupt, got: $interrupt" >&2
  exit 1
}}
touch "$MARKER"
echo '{{"id":4,"result":{{}}}}'

# Let the original turn/start request finish after the interrupt request has been accepted.
echo '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'
echo '{{"method":"turn/completed","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"interrupted","error":null}}}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#,
            marker = marker.display(),
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_turn_started = timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if matches!(event.msg, EventMsg::TurnStarted(TurnStartedEvent { .. })) {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(
            saw_turn_started,
            Ok(true),
            "did not observe TurnStarted before interrupt"
        );

        op_tx.send(Op::Interrupt).expect("send interrupt");

        timeout(Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for dummy server marker");

        let saw_round_finished = timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if matches!(
                    event.msg,
                    EventMsg::PotterRoundFinished {
                        outcome: PotterRoundOutcome::Interrupted,
                        ..
                    }
                ) {
                    return true;
                }
            }
            false
        })
        .await;

        assert_eq!(
            saw_round_finished,
            Ok(true),
            "did not observe PotterRoundFinished(Interrupted)"
        );

        drop(op_tx);

        timeout(Duration::from_secs(10), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");

        assert!(
            marker.exists(),
            "dummy server did not observe turn/interrupt before turn/start response"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_clean_background_terminals_during_turn_start_requests_clean() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp
            .path()
            .join("saw-clean-background-terminals-before-turn-start-response");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# turn/start request: announce the turn but withhold the response until the client cleans terminals.
IFS= read -r _line
echo '{{"method":"turn/started","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"inProgress","error":null}}}}}}'

# thread/backgroundTerminals/clean request
IFS= read -r clean
echo "$clean" | grep -q '"method":"thread/backgroundTerminals/clean"' || {{
  echo "expected thread/backgroundTerminals/clean, got: $clean" >&2
  exit 1
}}
echo "$clean" | grep -q '"threadId":"00000000-0000-0000-0000-000000000000"' || {{
  echo "expected threadId in clean request, got: $clean" >&2
  exit 1
}}
touch "$MARKER"
echo '{{"id":4,"result":{{}}}}'

# Let the original turn/start request finish after the clean request has been accepted.
echo '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'
echo '{{"method":"turn/completed","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"completed","error":null}}}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#,
            marker = marker.display(),
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_turn_started = timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if matches!(event.msg, EventMsg::TurnStarted(TurnStartedEvent { .. })) {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(
            saw_turn_started,
            Ok(true),
            "did not observe TurnStarted before clean request"
        );

        op_tx
            .send(Op::CleanBackgroundTerminals)
            .expect("send clean background terminals");

        timeout(Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for dummy server marker");

        let saw_round_finished = timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if matches!(
                    event.msg,
                    EventMsg::PotterRoundFinished {
                        outcome: PotterRoundOutcome::Completed,
                        ..
                    }
                ) {
                    return true;
                }
            }
            false
        })
        .await;

        assert_eq!(
            saw_round_finished,
            Ok(true),
            "did not observe PotterRoundFinished(Completed)"
        );

        drop(op_tx);

        timeout(Duration::from_secs(10), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");

        assert!(
            marker.exists(),
            "dummy server did not observe thread/backgroundTerminals/clean before turn/start response"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_turn_interrupt_requests_turn_interrupt() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("saw-turn-interrupt");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# turn/start request
IFS= read -r _line
echo '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'
echo '{{"method":"turn/started","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"inProgress","error":null}}}}}}'

# turn/interrupt request
IFS= read -r interrupt
echo "$interrupt" | grep -q '"method":"turn/interrupt"' || {{
  echo "expected turn/interrupt, got: $interrupt" >&2
  exit 1
}}
echo "$interrupt" | grep -q '"threadId":"00000000-0000-0000-0000-000000000000"' || {{
  echo "expected threadId in turn/interrupt, got: $interrupt" >&2
  exit 1
}}
echo "$interrupt" | grep -q '"turnId":"turn-1"' || {{
  echo "expected turnId=turn-1 in turn/interrupt, got: $interrupt" >&2
  exit 1
}}
touch "$MARKER"
echo '{{"id":4,"result":{{}}}}'

# signal interruption for the turn
echo '{{"method":"turn/completed","params":{{"threadId":"00000000-0000-0000-0000-000000000000","turn":{{"id":"turn-1","items":[],"status":"interrupted","error":null}}}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#,
            marker = marker.display(),
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_turn_started = timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if matches!(event.msg, EventMsg::TurnStarted(TurnStartedEvent { .. })) {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(
            saw_turn_started,
            Ok(true),
            "did not observe TurnStarted before interrupt"
        );

        op_tx.send(Op::Interrupt).expect("send interrupt");

        timeout(Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for dummy server marker");

        let saw_round_finished = timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if matches!(
                    event.msg,
                    EventMsg::PotterRoundFinished {
                        outcome: PotterRoundOutcome::Interrupted,
                        ..
                    }
                ) {
                    return true;
                }
            }
            false
        })
        .await;

        assert_eq!(
            saw_round_finished,
            Ok(true),
            "did not observe PotterRoundFinished(Interrupted)"
        );

        drop(op_tx);

        timeout(Duration::from_secs(10), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");

        assert!(
            marker.exists(),
            "dummy server did not observe turn/interrupt"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_stream_recovery_rolls_back_last_continue_turn() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("saw-rollback-then-continue");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# first turn/start request
IFS= read -r _line
echo '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'

# signal a retryable stream error, followed by an empty completion
echo '{{"method":"codex/event/test","params":{{"id":"err-1","msg":{{"type":"error","message":"stream disconnected before completion: error sending request for url (...)"}}}}}}'
echo '{{"method":"codex/event/test","params":{{"id":"tc-1","msg":{{"type":"turn_complete","last_agent_message":null}}}}}}'

# second turn/start request (first automatic Continue)
IFS= read -r turn_start
echo "$turn_start" | grep -q '"method":"turn/start"' || {{
  echo "expected turn/start, got: $turn_start" >&2
  exit 1
}}
echo "$turn_start" | grep -q '"text":"Continue"' || {{
  echo "expected Continue prompt, got: $turn_start" >&2
  exit 1
}}
echo '{{"id":4,"result":{{"turn":{{"id":"turn-2"}}}}}}'

# signal another retryable stream error (attempt 2)
echo '{{"method":"codex/event/test","params":{{"id":"err-2","msg":{{"type":"error","message":"stream disconnected before completion: error sending request for url (...)"}}}}}}'
echo '{{"method":"codex/event/test","params":{{"id":"tc-2","msg":{{"type":"turn_complete","last_agent_message":null}}}}}}'

# thread/rollback request should occur before the next Continue
IFS= read -r rollback
echo "$rollback" | grep -q '"method":"thread/rollback"' || {{
  echo "expected thread/rollback, got: $rollback" >&2
  exit 1
}}
echo "$rollback" | grep -q '"numTurns":1' || {{
  echo "expected numTurns=1, got: $rollback" >&2
  exit 1
}}
echo '{{"method":"codex/event/thread_rolled_back","params":{{"id":"rb-1","msg":{{"type":"thread_rolled_back","num_turns":1}}}}}}'
echo '{{"id":5,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}}}}}}'

# third turn/start request (automatic Continue after rollback)
IFS= read -r turn_start
echo "$turn_start" | grep -q '"method":"turn/start"' || {{
  echo "expected turn/start, got: $turn_start" >&2
  exit 1
}}
echo "$turn_start" | grep -q '"text":"Continue"' || {{
  echo "expected Continue prompt, got: $turn_start" >&2
  exit 1
}}
touch "$MARKER"
echo '{{"id":6,"result":{{"turn":{{"id":"turn-3"}}}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#,
            marker = marker.display()
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, _event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        timeout(Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for dummy server marker");

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");

        assert!(
            marker.exists(),
            "dummy server did not observe rollback+continue"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_user_input_cancels_pending_stream_recovery_continue() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("saw-manual-input-without-auto-continue");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r _line
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","path":"rollout.jsonl"}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# first turn/start request
IFS= read -r _line
echo '{{"id":3,"result":{{"turn":{{"id":"turn-1"}}}}}}'

# signal a retryable stream error, followed by an empty completion
echo '{{"method":"codex/event/test","params":{{"id":"err-1","msg":{{"type":"error","message":"stream disconnected before completion: error sending request for url (...)"}}}}}}'
echo '{{"method":"codex/event/test","params":{{"id":"tc-1","msg":{{"type":"turn_complete","last_agent_message":null}}}}}}'

# second turn/start request (first automatic Continue)
IFS= read -r turn_start
echo "$turn_start" | grep -q '"method":"turn/start"' || {{
  echo "expected turn/start, got: $turn_start" >&2
  exit 1
}}
echo "$turn_start" | grep -q '"text":"Continue"' || {{
  echo "expected Continue prompt, got: $turn_start" >&2
  exit 1
}}
echo '{{"id":4,"result":{{"turn":{{"id":"turn-2"}}}}}}'

# signal another retryable stream error (attempt 2), which would normally schedule a rollback+Continue
echo '{{"method":"codex/event/test","params":{{"id":"err-2","msg":{{"type":"error","message":"stream disconnected before completion: error sending request for url (...)"}}}}}}'
echo '{{"method":"codex/event/test","params":{{"id":"tc-2","msg":{{"type":"turn_complete","last_agent_message":null}}}}}}'

# third turn/start request (manual user input should cancel the pending automatic Continue)
IFS= read -r turn_start
echo "$turn_start" | grep -q '"method":"turn/start"' || {{
  if echo "$turn_start" | grep -q '"method":"thread/rollback"'; then
    echo "unexpected thread/rollback before manual input: $turn_start" >&2
  else
    echo "expected manual turn/start, got: $turn_start" >&2
  fi
  exit 1
}}
echo "$turn_start" | grep -q '"text":"manual"' || {{
  echo "expected manual prompt, got: $turn_start" >&2
  exit 1
}}
echo '{{"id":5,"result":{{"turn":{{"id":"turn-3"}}}}}}'

# Ensure the pending retry action does not fire after the manual input.
if IFS= read -r -t 2 unexpected; then
  echo "expected no further requests, got: $unexpected" >&2
  exit 1
fi

touch "$MARKER"

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done
"#,
            marker = marker.display(),
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, mut event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        let backend = tokio::spawn(async move {
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            )
            .await
        });

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send user input");

        let saw_second_attempt = timeout(Duration::from_secs(5), async {
            while let Some(event) = event_rx.recv().await {
                if let EventMsg::PotterStreamRecoveryUpdate { attempt, .. } = event.msg
                    && attempt == 2
                {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(
            saw_second_attempt,
            Ok(true),
            "did not observe stream recovery attempt 2"
        );

        op_tx
            .send(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "manual".to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .expect("send manual input");

        timeout(Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for dummy server marker");

        drop(op_tx);

        timeout(Duration::from_secs(5), backend)
            .await
            .expect("backend timed out")
            .expect("backend panicked")
            .expect("backend failed");

        assert!(
            marker.exists(),
            "dummy server did not observe manual input without auto continue"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_exits_when_op_channel_is_closed() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("saw-stdin-eof");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "--dangerously-bypass-approvals-and-sandbox" ]]; then
  echo "expected --dangerously-bypass-approvals-and-sandbox, got: $*" >&2
  exit 1
fi
if [[ "${{2:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# Emit enough stderr output to fill a typical pipe buffer if the client isn't draining it.
dd if=/dev/zero bs=1 count=131072 1>&2 2>/dev/null

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r thread_start
echo "$thread_start" | grep -q '"sandbox":"danger-full-access"' || {{
  echo "expected sandbox=danger-full-access in thread/start, got: $thread_start" >&2
  exit 1
}}
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","preview":"","modelProvider":"test-provider","createdAt":0,"updatedAt":0,"path":"rollout.jsonl","cwd":"project","cliVersion":"0.0.0","source":"appServer","gitInfo":null,"turns":[]}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done

touch "$MARKER"
"#,
            marker = marker.display()
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, _event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        drop(op_tx);

        timeout(
            Duration::from_secs(10),
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: Some(
                            crate::app_server::upstream_protocol::SandboxMode::DangerFullAccess,
                        ),
                        bypass_approvals_and_sandbox: true,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            ),
        )
        .await
        .expect("backend timed out")
        .expect("backend failed");

        assert!(marker.exists(), "dummy server did not observe stdin EOF");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_exits_when_op_channel_is_closed_workspace_write() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("saw-stdin-eof");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

        if [[ "${{1:-}}" != "--sandbox" ]]; then
  echo "expected --sandbox, got: $*" >&2
  exit 1
fi
if [[ "${{2:-}}" != "workspace-write" ]]; then
  echo "expected workspace-write, got: $*" >&2
  exit 1
fi
if [[ "${{3:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# Emit enough stderr output to fill a typical pipe buffer if the client isn't draining it.
dd if=/dev/zero bs=1 count=131072 1>&2 2>/dev/null

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r thread_start
echo "$thread_start" | grep -q '"sandbox":"workspace-write"' || {{
  echo "expected sandbox=workspace-write in thread/start, got: $thread_start" >&2
  exit 1
}}
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","preview":"","modelProvider":"test-provider","createdAt":0,"updatedAt":0,"path":"rollout.jsonl","cwd":"project","cliVersion":"0.0.0","source":"appServer","gitInfo":null,"turns":[]}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"workspaceWrite","writableRoots":[],"networkAccess":false,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}},"reasoningEffort":null}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done

touch "$MARKER"
"#,
            marker = marker.display()
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, _event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        drop(op_tx);

        timeout(
            Duration::from_secs(10),
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: Some(
                            crate::app_server::upstream_protocol::SandboxMode::WorkspaceWrite,
                        ),
                        thread_sandbox: Some(
                            crate::app_server::upstream_protocol::SandboxMode::WorkspaceWrite,
                        ),
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            ),
        )
        .await
        .expect("backend timed out")
        .expect("backend failed");

        assert!(marker.exists(), "dummy server did not observe stdin EOF");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_does_not_pass_sandbox_flag_for_default_mode() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("saw-stdin-eof");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r thread_start
echo "$thread_start" | grep -q '"sandbox":null' || {{
  echo "expected sandbox=null in thread/start, got: $thread_start" >&2
  exit 1
}}
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","preview":"","modelProvider":"test-provider","createdAt":0,"updatedAt":0,"path":"rollout.jsonl","cwd":"project","cliVersion":"0.0.0","source":"appServer","gitInfo":null,"turns":[]}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done

touch "$MARKER"
"#,
            marker = marker.display()
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, _event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        drop(op_tx);

        timeout(
            Duration::from_secs(10),
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: None,
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            ),
        )
        .await
        .expect("backend timed out")
        .expect("backend failed");

        assert!(marker.exists(), "dummy server did not observe stdin EOF");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backend_sets_codex_home_env_when_provided() {
        let _guard = lock_dummy_codex_test().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_bin = temp.path().join("dummy-codex");
        let marker = temp.path().join("saw-stdin-eof");
        let codex_home = temp.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("create codex home");

        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

MARKER="{marker}"
EXPECTED_CODEX_HOME="{codex_home}"

if [[ "${{1:-}}" != "app-server" ]]; then
  echo "expected app-server, got: $*" >&2
  exit 1
fi

if [[ "${{CODEX_HOME:-}}" != "$EXPECTED_CODEX_HOME" ]]; then
  echo "expected CODEX_HOME=$EXPECTED_CODEX_HOME, got: ${{CODEX_HOME:-}}" >&2
  exit 1
fi

# initialize request
IFS= read -r _line
echo '{{"id":1,"result":{{"userAgent":"test-agent","codexHome":"/tmp/codex-home","platformFamily":"unix","platformOs":"test-os"}}}}'

# initialized notification
IFS= read -r _line

# thread/start request
IFS= read -r thread_start
if echo "$thread_start" | grep -Fq '"codex_home"'; then
  echo "did not expect codex_home in thread/start config, got: $thread_start" >&2
  exit 1
fi
echo '{{"id":2,"result":{{"thread":{{"id":"00000000-0000-0000-0000-000000000000","preview":"","modelProvider":"test-provider","createdAt":0,"updatedAt":0,"path":"rollout.jsonl","cwd":"project","cliVersion":"0.0.0","source":"appServer","gitInfo":null,"turns":[]}},"model":"test-model","modelProvider":"test-provider","cwd":"project","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{{"type":"readOnly"}},"reasoningEffort":null}}}}'

# Wait for the client to close stdin to request shutdown.
while IFS= read -r _line; do
  :
done

touch "$MARKER"
"#,
            marker = marker.display(),
            codex_home = codex_home.display(),
        );

        write_dummy_codex_script(&codex_bin, script);

        let (event_tx, _event_rx) = unbounded_channel::<Event>();

        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        drop(op_tx);

        timeout(
            Duration::from_secs(5),
            run_app_server_backend_inner(
                AppServerBackendConfig {
                    codex_bin: codex_bin.display().to_string(),
                    developer_instructions: None,
                    launch: AppServerLaunchConfig {
                        spawn_sandbox: None,
                        thread_sandbox: None,
                        bypass_approvals_and_sandbox: false,
                    },
                    upstream_cli_args: Default::default(),
                    codex_home: Some(codex_home),
                    thread_cwd: None,
                    resume_thread_id: None,
                    event_mode: AppServerEventMode::Interactive,
                },
                &mut op_rx,
                &event_tx,
            ),
        )
        .await
        .expect("backend timed out")
        .expect("backend failed");

        assert!(marker.exists(), "dummy server did not observe stdin EOF");
    }
}
