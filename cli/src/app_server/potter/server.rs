//! CodexPotter project-level app-server implementation.
//!
//! This JSON-RPC server is the "control plane" for CodexPotter:
//!
//! - Maintains active project state (fresh projects and resumed projects).
//! - Spawns per-round upstream `codex app-server` backends via `crate::app_server::codex_backend`.
//! - Forwards all `EventMsg` notifications to clients via `codex/event/potter`.
//! - Persists project boundaries to `potter-rollout.jsonl` and supports replay via `project/resume`.
//!
//! The server is long-lived and can serve multiple sequential project runs. Each round backend is
//! short-lived and isolated by spawning a new upstream process.

use std::io::BufRead as _;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use chrono::Local;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::PotterProjectOutcome;
use codex_protocol::protocol::PotterRoundOutcome;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::UserInput;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::watch;

use crate::app_server::potter::POTTER_EVENT_NOTIFICATION_METHOD;
use crate::app_server::potter::PotterAppServerClientNotification;
use crate::app_server::potter::PotterAppServerClientRequest;
use crate::app_server::potter::PotterEventMode;
use crate::app_server::potter::ProjectInterruptParams;
use crate::app_server::potter::ProjectListEntry;
use crate::app_server::potter::ProjectListParams;
use crate::app_server::potter::ProjectListResponse;
use crate::app_server::potter::ProjectResolveInterruptParams;
use crate::app_server::potter::ProjectResolveInterruptResponse;
use crate::app_server::potter::ProjectResumeParams;
use crate::app_server::potter::ProjectResumeReplay;
use crate::app_server::potter::ProjectResumeReplayRound;
use crate::app_server::potter::ProjectResumeResponse;
use crate::app_server::potter::ProjectResumeUnfinishedRound;
use crate::app_server::potter::ProjectStartParams;
use crate::app_server::potter::ProjectStartResponse;
use crate::app_server::potter::ProjectStartRoundsParams;
use crate::app_server::potter::ProjectStartRoundsResponse;
use crate::app_server::potter::ResolveInterruptAction;
use crate::app_server::potter::ResumePolicy;
use crate::app_server::upstream_protocol::JSONRPCError;
use crate::app_server::upstream_protocol::JSONRPCErrorError;
use crate::app_server::upstream_protocol::JSONRPCMessage;
use crate::app_server::upstream_protocol::JSONRPCNotification;
use crate::app_server::upstream_protocol::JSONRPCRequest;
use crate::app_server::upstream_protocol::JSONRPCResponse;
use crate::app_server::upstream_protocol::RequestId;

#[derive(Debug, Clone)]
pub struct PotterAppServerConfig {
    pub default_workdir: PathBuf,
    pub codex_bin: String,
    pub backend_launch: crate::app_server::AppServerLaunchConfig,
    pub codex_compat_home: Option<PathBuf>,
    pub rounds: NonZeroUsize,
}

#[derive(Debug)]
struct RunningProject {
    project_id: String,
    handle: tokio::task::JoinHandle<()>,
    interrupt_tx: watch::Sender<bool>,
}

#[derive(Debug, Clone)]
struct ResumedProject {
    project_id: String,
    resolved: crate::workflow::resume::ResolvedProjectPaths,
    progress_file_rel: PathBuf,
    potter_rollout_lines: Vec<crate::workflow::rollout::PotterRolloutLine>,
    index: crate::workflow::rollout_resume_index::PotterRolloutResumeIndex,
}

#[derive(Debug, Clone)]
struct InterruptedProject {
    project_id: String,
    user_prompt_file: PathBuf,
    rounds_run: u32,
    plan: FreshProjectPlan,
}

struct ServerState {
    config: PotterAppServerConfig,
    running: Option<RunningProject>,
    resumed: Option<ResumedProject>,
    interrupted: Option<InterruptedProject>,
}

enum InternalEvent {
    ProjectFinished { project_id: String },
    ProjectInterrupted { project: Box<InterruptedProject> },
}

enum ProjectRunExit {
    Completed,
    Interrupted(Box<InterruptedProject>),
}

fn decode_jsonrpc_message_line(line: &str) -> anyhow::Result<Option<JSONRPCMessage>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let msg: JSONRPCMessage = serde_json::from_str(trimmed)
        .with_context(|| format!("decode potter app-server JSON-RPC: {trimmed:?}"))?;
    Ok(Some(msg))
}

pub async fn run_potter_app_server(config: PotterAppServerConfig) -> anyhow::Result<()> {
    tokio::task::LocalSet::new()
        .run_until(run_potter_app_server_inner(config))
        .await
}

async fn run_potter_app_server_inner(config: PotterAppServerConfig) -> anyhow::Result<()> {
    let (writer_tx, mut writer_rx) = unbounded_channel::<JSONRPCMessage>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = writer_rx.recv().await {
            let json = serde_json::to_vec(&msg).context("serialize potter app-server jsonrpc")?;
            stdout
                .write_all(&json)
                .await
                .context("write potter app-server stdout")?;
            stdout
                .write_all(b"\n")
                .await
                .context("write potter app-server newline")?;
            stdout
                .flush()
                .await
                .context("flush potter app-server stdout")?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let (internal_tx, mut internal_rx) = unbounded_channel::<InternalEvent>();
    let mut state = ServerState {
        config,
        running: None,
        resumed: None,
        interrupted: None,
    };

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    loop {
        tokio::select! {
            maybe_line = lines.next_line() => {
                let Some(line) = maybe_line.context("read potter app-server stdin line")? else {
                    break;
                };

                let msg = match decode_jsonrpc_message_line(&line) {
                    Ok(Some(msg)) => msg,
                    Ok(None) => continue,
                    Err(err) => {
                        eprintln!("warning: {err:#}");
                        continue;
                    }
                };
                handle_jsonrpc_message(msg, &mut state, &writer_tx, &internal_tx).await;
            }
            Some(event) = internal_rx.recv() => match event {
                InternalEvent::ProjectFinished { project_id } => {
                    if state
                        .running
                        .as_ref()
                        .is_some_and(|running| running.project_id == project_id)
                    {
                        state.running = None;
                    }
                }
                InternalEvent::ProjectInterrupted { project } => {
                    let project = *project;
                    if state
                        .running
                        .as_ref()
                        .is_some_and(|running| running.project_id == project.project_id)
                    {
                        state.running = None;
                    }
                    state.resumed = None;
                    state.interrupted = Some(project);

                    let project = state
                        .interrupted
                        .as_ref()
                        .expect("interrupted project just set");
                    emit_potter_event(
                        writer_tx.clone(),
                        Event {
                            id: "".to_string(),
                            msg: EventMsg::PotterProjectInterrupted {
                                project_id: project.project_id.clone(),
                                user_prompt_file: project.user_prompt_file.clone(),
                            },
                        },
                    );
                }
            }
        }
    }

    drop(writer_tx);
    let _ = writer.await;
    Ok(())
}

async fn handle_jsonrpc_message(
    msg: JSONRPCMessage,
    state: &mut ServerState,
    writer_tx: &UnboundedSender<JSONRPCMessage>,
    internal_tx: &UnboundedSender<InternalEvent>,
) {
    match msg {
        JSONRPCMessage::Request(request) => {
            if let Err(err) = handle_request(request, state, writer_tx, internal_tx).await {
                eprintln!("potter app-server request failed: {err:#}");
            }
        }
        JSONRPCMessage::Notification(notification) => {
            if let Err(err) = handle_notification(notification).await {
                eprintln!("potter app-server notification failed: {err:#}");
            }
        }
        JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {}
    }
}

async fn handle_notification(notification: JSONRPCNotification) -> anyhow::Result<()> {
    let _notification = PotterAppServerClientNotification::try_from(notification)?;
    Ok(())
}

async fn handle_request(
    request: JSONRPCRequest,
    state: &mut ServerState,
    writer_tx: &UnboundedSender<JSONRPCMessage>,
    internal_tx: &UnboundedSender<InternalEvent>,
) -> anyhow::Result<()> {
    let request_id = request.id.clone();
    let method = request.method.clone();

    let parsed = match PotterAppServerClientRequest::try_from(request) {
        Ok(parsed) => parsed,
        Err(err) => {
            send_error(
                writer_tx,
                request_id,
                -32602,
                format!("invalid request {method:?}: {err}"),
            );
            return Ok(());
        }
    };

    clear_finished_running_project(state);

    match parsed {
        PotterAppServerClientRequest::Initialize { request_id, .. } => {
            send_response(writer_tx, request_id, serde_json::json!({}));
        }
        PotterAppServerClientRequest::ProjectList {
            request_id, params, ..
        } => match project_list(&state.config.default_workdir, params) {
            Ok(response) => send_response(writer_tx, request_id, response),
            Err(err) => send_error(writer_tx, request_id, -32000, format!("{err:#}")),
        },
        PotterAppServerClientRequest::ProjectStart { request_id, params } => {
            if state.running.is_some() {
                send_error(
                    writer_tx,
                    request_id,
                    -32000,
                    "a project is already running".to_string(),
                );
                return Ok(());
            }
            if state.interrupted.is_some() {
                send_error(
                    writer_tx,
                    request_id,
                    -32000,
                    "a project is interrupted; resolve it first".to_string(),
                );
                return Ok(());
            }

            match start_project(state, params, writer_tx, internal_tx).await {
                Ok(response) => send_response(writer_tx, request_id, response),
                Err(err) => send_error(writer_tx, request_id, -32000, format!("{err:#}")),
            }
        }
        PotterAppServerClientRequest::ProjectResume { request_id, params } => {
            if state.running.is_some() {
                send_error(
                    writer_tx,
                    request_id,
                    -32000,
                    "a project is already running".to_string(),
                );
                return Ok(());
            }
            if state.interrupted.is_some() {
                send_error(
                    writer_tx,
                    request_id,
                    -32000,
                    "a project is interrupted; resolve it first".to_string(),
                );
                return Ok(());
            }

            match resume_project(state, params) {
                Ok(response) => send_response(writer_tx, request_id, response),
                Err(err) => send_error(writer_tx, request_id, -32000, format!("{err:#}")),
            }
        }
        PotterAppServerClientRequest::ProjectStartRounds { request_id, params } => {
            if state.running.is_some() {
                send_error(
                    writer_tx,
                    request_id,
                    -32000,
                    "a project is already running".to_string(),
                );
                return Ok(());
            }
            if state.interrupted.is_some() {
                send_error(
                    writer_tx,
                    request_id,
                    -32000,
                    "a project is interrupted; resolve it first".to_string(),
                );
                return Ok(());
            }

            match start_rounds(state, params, writer_tx, internal_tx).await {
                Ok(response) => send_response(writer_tx, request_id, response),
                Err(err) => send_error(writer_tx, request_id, -32000, format!("{err:#}")),
            }
        }
        PotterAppServerClientRequest::ProjectInterrupt { request_id, params } => {
            match interrupt_project(state, params) {
                Ok(()) => send_response(writer_tx, request_id, serde_json::json!({})),
                Err(err) => send_error(writer_tx, request_id, -32000, format!("{err:#}")),
            }
        }
        PotterAppServerClientRequest::ProjectResolveInterrupt { request_id, params } => {
            match resolve_interrupt_project(state, params, writer_tx, internal_tx) {
                Ok(response) => send_response(writer_tx, request_id, response),
                Err(err) => send_error(writer_tx, request_id, -32000, format!("{err:#}")),
            }
        }
    }

    Ok(())
}

fn clear_finished_running_project(state: &mut ServerState) {
    if state
        .running
        .as_ref()
        .is_some_and(|running| running.handle.is_finished())
    {
        state.running = None;
    }
}

fn project_list(
    default_workdir: &Path,
    params: ProjectListParams,
) -> anyhow::Result<ProjectListResponse> {
    let ProjectListParams { cwd } = params;
    let workdir = cwd.unwrap_or_else(|| default_workdir.to_path_buf());

    let rows = crate::workflow::resume_picker_index::discover_resumable_projects(&workdir)
        .with_context(|| format!("discover resumable projects under {}", workdir.display()))?;

    let mut projects = Vec::new();
    for row in rows {
        let Some(created_at) = system_time_to_unix_secs(row.created_at) else {
            continue;
        };
        let Some(updated_at) = system_time_to_unix_secs(row.updated_at) else {
            continue;
        };
        projects.push(ProjectListEntry {
            project_path: row.project_path,
            user_request: row.user_request,
            created_at_unix_secs: created_at,
            updated_at_unix_secs: updated_at,
            git_branch: row.git_branch,
        });
    }

    Ok(ProjectListResponse { projects })
}

async fn start_project(
    state: &mut ServerState,
    params: ProjectStartParams,
    writer_tx: &UnboundedSender<JSONRPCMessage>,
    internal_tx: &UnboundedSender<InternalEvent>,
) -> anyhow::Result<ProjectStartResponse> {
    let ProjectStartParams {
        user_message,
        cwd,
        rounds,
        event_mode,
    } = params;

    let workdir = cwd.unwrap_or_else(|| state.config.default_workdir.clone());
    let workdir = workdir
        .canonicalize()
        .with_context(|| format!("canonicalize {}", workdir.display()))?;

    let init = crate::workflow::project::init_project(&workdir, &user_message, Local::now())
        .context("initialize .codexpotter project")?;
    let progress_file_abs = workdir.join(&init.progress_file_rel);
    let project_dir_rel = init
        .progress_file_rel
        .parent()
        .context("derive project_dir from progress file path")?
        .to_path_buf();
    let project_dir_abs = workdir.join(&project_dir_rel);

    let potter_rollout_path = crate::workflow::rollout::potter_rollout_path(&project_dir_abs);
    let git_branch = crate::workflow::project::progress_file_git_branch(&progress_file_abs)
        .context("read git_branch from progress file")?;

    let rounds_total_u32 = match rounds {
        Some(rounds) if rounds > 0 => rounds,
        Some(_) => anyhow::bail!("rounds must be >= 1"),
        None => crate::rounds::round_budget_to_u32(state.config.rounds)?,
    };
    let mode = event_mode.unwrap_or_default();

    let project_id = progress_file_abs.to_string_lossy().to_string();
    spawn_fresh_project(
        &mut state.running,
        &mut state.resumed,
        state.config.clone(),
        writer_tx.clone(),
        internal_tx.clone(),
        project_id.clone(),
        FreshProjectPlan {
            workdir: workdir.clone(),
            user_message: user_message.clone(),
            project_dir_rel: project_dir_rel.clone(),
            progress_file_rel: init.progress_file_rel.clone(),
            git_commit_start: init.git_commit_start.clone(),
            potter_rollout_path,
            rounds_total: rounds_total_u32,
            event_mode: mode,
            project_started_at: Instant::now(),
            round_start_index: 0,
            emit_project_started_event: true,
            initial_turn_prompt_override: None,
        },
    )?;

    Ok(ProjectStartResponse {
        project_id,
        working_dir: workdir,
        project_dir: project_dir_abs,
        progress_file_rel: init.progress_file_rel,
        progress_file: progress_file_abs,
        git_commit_start: init.git_commit_start,
        git_branch,
        rounds_total: rounds_total_u32,
    })
}

fn resume_project(
    state: &mut ServerState,
    params: ProjectResumeParams,
) -> anyhow::Result<ProjectResumeResponse> {
    let ProjectResumeParams {
        project_path,
        cwd,
        event_mode: _,
    } = params;

    let cwd = cwd.unwrap_or_else(|| state.config.default_workdir.clone());
    let resolved = crate::workflow::resume::resolve_project_paths(&cwd, &project_path)?;

    let progress_file_rel = resolved
        .progress_file
        .strip_prefix(&resolved.workdir)
        .context("derive progress file relative path")?
        .to_path_buf();

    let git_branch = crate::workflow::project::progress_file_git_branch(&resolved.progress_file)
        .context("read git_branch from progress file")?;

    let potter_rollout_path = crate::workflow::rollout::potter_rollout_path(&resolved.project_dir);
    let potter_rollout_lines = load_potter_rollout_lines(&potter_rollout_path)?;
    let index = crate::workflow::rollout_resume_index::build_resume_index(&potter_rollout_lines)?;

    let replay = build_resume_replay(&resolved, &index)?;
    let unfinished_round = build_unfinished_round_pre_action(&resolved, &replay, &index)?;

    let project_id = resolved.progress_file.to_string_lossy().to_string();

    state.resumed = Some(ResumedProject {
        project_id: project_id.clone(),
        resolved: resolved.clone(),
        progress_file_rel: progress_file_rel.clone(),
        potter_rollout_lines,
        index,
    });

    Ok(ProjectResumeResponse {
        project_id,
        working_dir: resolved.workdir,
        project_dir: resolved.project_dir,
        progress_file_rel,
        progress_file: resolved.progress_file,
        git_branch,
        replay,
        unfinished_round,
    })
}

async fn start_rounds(
    state: &mut ServerState,
    params: ProjectStartRoundsParams,
    writer_tx: &UnboundedSender<JSONRPCMessage>,
    internal_tx: &UnboundedSender<InternalEvent>,
) -> anyhow::Result<ProjectStartRoundsResponse> {
    let ProjectStartRoundsParams {
        project_id,
        rounds,
        resume_policy,
        event_mode,
    } = params;

    let Some(resumed) = state.resumed.clone() else {
        anyhow::bail!("no resumed project is active");
    };
    anyhow::ensure!(resumed.project_id == project_id, "resumed project mismatch");

    let mode = event_mode.unwrap_or_default();
    let resume_policy = resume_policy.unwrap_or_default();

    let rounds_total_u32 = match rounds {
        Some(rounds) if rounds > 0 => rounds,
        Some(_) => anyhow::bail!("rounds must be >= 1"),
        None => crate::rounds::round_budget_to_u32(state.config.rounds)?,
    };

    let potter_rollout_path =
        crate::workflow::rollout::potter_rollout_path(&resumed.resolved.project_dir);

    // Resume continuation always starts a new iteration window; reset the progress file flag.
    crate::workflow::project::set_progress_file_finite_incantatem(
        &resumed.resolved.workdir,
        &resumed.progress_file_rel,
        false,
    )
    .context("reset progress file finite_incantatem")?;

    let baseline_rounds = count_completed_rounds(&resumed.potter_rollout_lines);
    let baseline_rounds_u32 =
        crate::rounds::usize_to_u32(baseline_rounds, "potter-rollout baseline rounds")?;
    let git_commit_start = crate::workflow::project::progress_file_git_commit_start(
        &resumed.resolved.workdir,
        &resumed.progress_file_rel,
    )
    .context("read git_commit from progress file")?;

    spawn_resumed_project(
        &mut state.running,
        &mut state.resumed,
        state.config.clone(),
        writer_tx.clone(),
        internal_tx.clone(),
        resumed.project_id.clone(),
        ResumedProjectPlan {
            resumed,
            baseline_rounds: baseline_rounds_u32,
            git_commit_start,
            potter_rollout_path,
            rounds_total: rounds_total_u32,
            resume_policy,
            event_mode: mode,
            project_started_at: Instant::now(),
        },
    )?;

    Ok(ProjectStartRoundsResponse {
        rounds_total: rounds_total_u32,
    })
}

fn interrupt_project(
    state: &mut ServerState,
    params: ProjectInterruptParams,
) -> anyhow::Result<()> {
    let ProjectInterruptParams { project_id } = params;

    if let Some(running) = state.running.as_ref() {
        let running_project_id = running.project_id.clone();
        let already_requested = *running.interrupt_tx.borrow();
        let interrupt_tx = running.interrupt_tx.clone();

        anyhow::ensure!(
            running_project_id == project_id,
            "active running project mismatch: running={running_project_id} requested={project_id}",
        );

        if already_requested {
            let running = state
                .running
                .take()
                .context("take running project after id match")?;
            running.handle.abort();
            state.resumed = None;
            return Ok(());
        }

        let _ = interrupt_tx.send(true);
        return Ok(());
    }

    if let Some(resumed) = state.resumed.as_ref() {
        anyhow::ensure!(
            resumed.project_id == project_id,
            "active resumed project mismatch: resumed={} requested={project_id}",
            resumed.project_id
        );
        state.resumed = None;
        return Ok(());
    }

    Ok(())
}

fn resolve_interrupt_project(
    state: &mut ServerState,
    params: ProjectResolveInterruptParams,
    writer_tx: &UnboundedSender<JSONRPCMessage>,
    internal_tx: &UnboundedSender<InternalEvent>,
) -> anyhow::Result<ProjectResolveInterruptResponse> {
    let ProjectResolveInterruptParams {
        project_id,
        action,
        turn_prompt_override,
    } = params;

    let interrupted = state
        .interrupted
        .as_ref()
        .context("no interrupted project to resolve")?;

    anyhow::ensure!(
        interrupted.project_id == project_id,
        "active interrupted project mismatch: interrupted={} requested={project_id}",
        interrupted.project_id
    );

    match action {
        ResolveInterruptAction::Stop => {
            let interrupted = state
                .interrupted
                .take()
                .context("take interrupted project after id match")?;

            let InterruptedProject {
                rounds_run,
                user_prompt_file,
                plan,
                ..
            } = interrupted;

            let FreshProjectPlan {
                workdir,
                git_commit_start,
                project_started_at,
                ..
            } = plan;

            let git_commit_end = crate::workflow::project::resolve_git_commit(&workdir);
            emit_potter_event(
                writer_tx.clone(),
                Event {
                    id: "".to_string(),
                    msg: EventMsg::PotterProjectCompleted {
                        outcome: PotterProjectOutcome::Interrupted,
                    },
                },
            );

            Ok(ProjectResolveInterruptResponse {
                summary: Some(crate::app_server::potter::InterruptedProjectSummary {
                    rounds: rounds_run,
                    duration: project_started_at.elapsed(),
                    user_prompt_file,
                    git_commit_start,
                    git_commit_end,
                }),
            })
        }
        ResolveInterruptAction::Continue => {
            let turn_prompt_override = turn_prompt_override
                .as_ref()
                .map(|prompt| prompt.trim())
                .filter(|prompt| !prompt.is_empty())
                .context("turn_prompt_override is required for continue")?
                .to_string();

            let interrupted = state
                .interrupted
                .take()
                .context("take interrupted project after id match")?;

            let mut plan = interrupted.plan;
            anyhow::ensure!(
                plan.round_start_index < plan.rounds_total,
                "no rounds remaining to continue (round_start_index={} rounds_total={})",
                plan.round_start_index,
                plan.rounds_total
            );
            plan.initial_turn_prompt_override = Some(turn_prompt_override);
            spawn_fresh_project(
                &mut state.running,
                &mut state.resumed,
                state.config.clone(),
                writer_tx.clone(),
                internal_tx.clone(),
                project_id,
                plan,
            )?;

            Ok(ProjectResolveInterruptResponse { summary: None })
        }
    }
}

fn send_response<T>(writer_tx: &UnboundedSender<JSONRPCMessage>, request_id: RequestId, payload: T)
where
    T: serde::Serialize,
{
    let result = match serde_json::to_value(payload) {
        Ok(value) => value,
        Err(err) => {
            send_error(
                writer_tx,
                request_id,
                -32000,
                format!("failed to serialize response: {err}"),
            );
            return;
        }
    };
    let _ = writer_tx.send(JSONRPCMessage::Response(JSONRPCResponse {
        id: request_id,
        result,
    }));
}

fn send_error(
    writer_tx: &UnboundedSender<JSONRPCMessage>,
    request_id: RequestId,
    code: i64,
    message: String,
) {
    let _ = writer_tx.send(JSONRPCMessage::Error(JSONRPCError {
        error: JSONRPCErrorError {
            code,
            message,
            data: None,
        },
        id: request_id,
    }));
}

fn emit_potter_event(writer_tx: UnboundedSender<JSONRPCMessage>, event: Event) {
    let Ok(params) = serde_json::to_value(event) else {
        return;
    };
    let _ = writer_tx.send(JSONRPCMessage::Notification(JSONRPCNotification {
        method: POTTER_EVENT_NOTIFICATION_METHOD.to_string(),
        params: Some(params),
    }));
}

fn system_time_to_unix_secs(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn load_potter_rollout_lines(
    potter_rollout_path: &Path,
) -> anyhow::Result<Vec<crate::workflow::rollout::PotterRolloutLine>> {
    if !potter_rollout_path.exists() {
        anyhow::bail!(
            "unsupported project: the project is from an older version of CodexPotter (missing potter-rollout.jsonl)",
        );
    }
    if !potter_rollout_path.is_file() {
        anyhow::bail!(
            "unsupported project: expected a file at {}",
            potter_rollout_path.display()
        );
    }

    let lines = crate::workflow::rollout::read_lines(potter_rollout_path)
        .with_context(|| format!("read {}", potter_rollout_path.display()))?;
    if lines.is_empty() {
        anyhow::bail!("potter-rollout is empty: {}", potter_rollout_path.display());
    }
    Ok(lines)
}

fn count_completed_rounds(lines: &[crate::workflow::rollout::PotterRolloutLine]) -> usize {
    // `Op::Interrupt` intentionally does not consume round budget: we may retry the interrupted
    // round (same round index / round_current) without advancing.
    lines
        .iter()
        .filter(|line| match line {
            crate::workflow::rollout::PotterRolloutLine::RoundFinished { outcome } => {
                !matches!(outcome, PotterRoundOutcome::Interrupted)
            }
            _ => false,
        })
        .count()
}

fn build_resume_replay(
    resolved: &crate::workflow::resume::ResolvedProjectPaths,
    index: &crate::workflow::rollout_resume_index::PotterRolloutResumeIndex,
) -> anyhow::Result<ProjectResumeReplay> {
    let mut completed_rounds = Vec::new();
    let mut is_first_round = true;

    for round in &index.completed_rounds {
        let mut events = Vec::new();
        if is_first_round {
            is_first_round = false;
            events.push(EventMsg::PotterProjectStarted {
                user_message: index.project_started.user_message.clone(),
                working_dir: resolved.workdir.clone(),
                project_dir: resolved.project_dir.clone(),
                user_prompt_file: index.project_started.user_prompt_file.clone(),
            });
        }

        events.push(EventMsg::PotterRoundStarted {
            current: round.round_current,
            total: round.round_total,
        });

        let rollout_path = resolve_rollout_path_for_replay(resolved, &round.rollout_path);
        if let Some(cfg) =
            synthesize_session_configured_event(round.thread_id, rollout_path.clone())?
        {
            events.push(EventMsg::SessionConfigured(cfg));
        }
        let mut rollout_events = read_upstream_rollout_event_msgs(&rollout_path)
            .with_context(|| format!("replay rollout {}", rollout_path.display()))?;
        events.append(&mut rollout_events);

        if let Some(project_succeeded) = &round.project_succeeded {
            events.push(EventMsg::PotterProjectSucceeded {
                rounds: project_succeeded.rounds,
                duration: std::time::Duration::from_secs(project_succeeded.duration_secs),
                user_prompt_file: project_succeeded.user_prompt_file.clone(),
                git_commit_start: project_succeeded.git_commit_start.clone(),
                git_commit_end: project_succeeded.git_commit_end.clone(),
            });
        }

        events.push(EventMsg::PotterRoundFinished {
            outcome: round.outcome.clone(),
        });

        completed_rounds.push(ProjectResumeReplayRound {
            outcome: round.outcome.clone(),
            events,
        });
    }

    Ok(ProjectResumeReplay { completed_rounds })
}

fn build_unfinished_round_pre_action(
    resolved: &crate::workflow::resume::ResolvedProjectPaths,
    replay: &ProjectResumeReplay,
    index: &crate::workflow::rollout_resume_index::PotterRolloutResumeIndex,
) -> anyhow::Result<Option<ProjectResumeUnfinishedRound>> {
    let Some(unfinished) = &index.unfinished_round else {
        return Ok(None);
    };

    let mut pre_action_events = Vec::new();
    if replay.completed_rounds.is_empty() {
        pre_action_events.push(EventMsg::PotterProjectStarted {
            user_message: index.project_started.user_message.clone(),
            working_dir: resolved.workdir.clone(),
            project_dir: resolved.project_dir.clone(),
            user_prompt_file: index.project_started.user_prompt_file.clone(),
        });
    }

    pre_action_events.push(EventMsg::PotterRoundStarted {
        current: unfinished.round_current,
        total: unfinished.round_total,
    });
    pre_action_events.push(EventMsg::PotterRoundFinished {
        outcome: PotterRoundOutcome::Completed,
    });

    let remaining_rounds_including_current =
        remaining_rounds_including_current(unfinished.round_current, unfinished.round_total)?;

    Ok(Some(ProjectResumeUnfinishedRound {
        round_current: unfinished.round_current,
        round_total: unfinished.round_total,
        pre_action_events,
        remaining_rounds_including_current,
    }))
}

fn remaining_rounds_including_current(round_current: u32, round_total: u32) -> anyhow::Result<u32> {
    if round_current == 0 {
        anyhow::bail!("potter-rollout: round_current must be >= 1");
    }
    if round_total == 0 {
        anyhow::bail!("potter-rollout: round_total must be >= 1");
    }
    if round_current > round_total {
        anyhow::bail!(
            "potter-rollout: round_current {round_current} exceeds round_total {round_total}",
        );
    }
    Ok(round_total.saturating_sub(round_current).saturating_add(1))
}

fn resolve_rollout_path_for_replay(
    project: &crate::workflow::resume::ResolvedProjectPaths,
    rollout_path: &Path,
) -> PathBuf {
    if rollout_path.is_absolute() {
        return rollout_path.to_path_buf();
    }
    project.workdir.join(rollout_path)
}

fn synthesize_session_configured_event(
    thread_id: ThreadId,
    rollout_path: PathBuf,
) -> anyhow::Result<Option<SessionConfiguredEvent>> {
    let Some(snapshot) = read_rollout_context_snapshot(&rollout_path)? else {
        return Ok(None);
    };

    Ok(Some(SessionConfiguredEvent {
        session_id: thread_id,
        forked_from_id: None,
        model: snapshot.model,
        model_provider_id: snapshot.model_provider_id,
        cwd: snapshot.cwd,
        reasoning_effort: None,
        history_log_id: 0,
        history_entry_count: 0,
        initial_messages: None,
        rollout_path,
    }))
}

struct RolloutContextSnapshot {
    cwd: PathBuf,
    model: String,
    model_provider_id: String,
}

fn read_rollout_context_snapshot(
    rollout_path: &Path,
) -> anyhow::Result<Option<RolloutContextSnapshot>> {
    let file = std::fs::File::open(rollout_path)
        .with_context(|| format!("open rollout {}", rollout_path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut cwd: Option<PathBuf> = None;
    let mut model: Option<String> = None;
    let mut model_provider_id: Option<String> = None;

    for (idx, line) in reader.lines().enumerate() {
        let line_number = idx + 1;
        let line = line.with_context(|| format!("read rollout line {line_number}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("parse rollout json line {line_number}: {line}"))?;
        let Some(item_type) = value.get("type").and_then(serde_json::Value::as_str) else {
            continue;
        };
        match item_type {
            "turn_context" => {
                if cwd.is_some() && model.is_some() {
                    continue;
                }
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                if cwd.is_none()
                    && let Some(v) = payload.get("cwd")
                {
                    cwd = serde_json::from_value::<PathBuf>(v.clone()).ok();
                }
                if model.is_none() {
                    model = payload
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned);
                }
            }
            "session_meta" => {
                if model_provider_id.is_some() {
                    continue;
                }
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                model_provider_id = payload
                    .get("model_provider")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
            }
            _ => {}
        }

        if cwd.is_some() && model.is_some() && model_provider_id.is_some() {
            break;
        }
    }

    let Some(cwd) = cwd else {
        return Ok(None);
    };
    let Some(model) = model else {
        return Ok(None);
    };

    Ok(Some(RolloutContextSnapshot {
        cwd,
        model,
        model_provider_id: model_provider_id.unwrap_or_default(),
    }))
}

fn read_upstream_rollout_event_msgs(rollout_path: &Path) -> anyhow::Result<Vec<EventMsg>> {
    let file = std::fs::File::open(rollout_path)
        .with_context(|| format!("open rollout {}", rollout_path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut out = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line_number = idx + 1;
        let line = line.with_context(|| format!("read rollout line {line_number}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("parse rollout json line {line_number}: {line}"))?;
        let Some(item_type) = value.get("type").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if item_type != "event_msg" {
            continue;
        }
        let payload = value
            .get("payload")
            .context("rollout event_msg missing payload")?;
        let msg = serde_json::from_value::<EventMsg>(payload.clone())
            .with_context(|| format!("decode EventMsg from rollout line {line_number}"))?;
        out.push(msg);
    }

    Ok(crate::workflow::resume::filter_pending_interactive_prompts_for_replay(out))
}

#[derive(Debug, Clone)]
struct FreshProjectPlan {
    workdir: PathBuf,
    user_message: String,
    project_dir_rel: PathBuf,
    progress_file_rel: PathBuf,
    git_commit_start: String,
    potter_rollout_path: PathBuf,
    rounds_total: u32,
    event_mode: PotterEventMode,
    project_started_at: Instant,
    round_start_index: u32,
    emit_project_started_event: bool,
    initial_turn_prompt_override: Option<String>,
}

impl FreshProjectPlan {
    /// Build a continuation plan after an interrupted iteration round.
    ///
    /// `interrupted_round_index` is zero-based (same scale as `round_start_index`).
    fn continuation_after_interrupt(&self, interrupted_round_index: u32) -> FreshProjectPlan {
        FreshProjectPlan {
            // Continue should retry the interrupted iteration round; do not advance the round
            // index (and do not consume round budget) just because we interrupted.
            round_start_index: interrupted_round_index,
            emit_project_started_event: false,
            initial_turn_prompt_override: None,
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone)]
struct ResumedProjectPlan {
    resumed: ResumedProject,
    baseline_rounds: u32,
    git_commit_start: String,
    potter_rollout_path: PathBuf,
    rounds_total: u32,
    resume_policy: ResumePolicy,
    event_mode: PotterEventMode,
    project_started_at: Instant,
}

fn spawn_fresh_project(
    running: &mut Option<RunningProject>,
    resumed: &mut Option<ResumedProject>,
    config: PotterAppServerConfig,
    writer_tx: UnboundedSender<JSONRPCMessage>,
    internal_tx: UnboundedSender<InternalEvent>,
    project_id: String,
    plan: FreshProjectPlan,
) -> anyhow::Result<()> {
    anyhow::ensure!(running.is_none(), "internal error: project already running");
    *resumed = None;

    let (interrupt_tx, interrupt_rx) = watch::channel(false);
    let project_id_for_event = project_id.clone();
    let handle = tokio::task::spawn_local(async move {
        match run_fresh_project(config, writer_tx.clone(), plan, interrupt_rx).await {
            Ok(ProjectRunExit::Completed) => {
                let _ = internal_tx.send(InternalEvent::ProjectFinished {
                    project_id: project_id_for_event,
                });
            }
            Ok(ProjectRunExit::Interrupted(project)) => {
                let _ = internal_tx.send(InternalEvent::ProjectInterrupted { project });
            }
            Err(err) => {
                eprintln!("potter app-server fresh project failed: {err:#}");
                let _ = internal_tx.send(InternalEvent::ProjectFinished {
                    project_id: project_id_for_event,
                });
            }
        }
    });

    *running = Some(RunningProject {
        project_id,
        handle,
        interrupt_tx,
    });

    Ok(())
}

fn spawn_resumed_project(
    running: &mut Option<RunningProject>,
    resumed: &mut Option<ResumedProject>,
    config: PotterAppServerConfig,
    writer_tx: UnboundedSender<JSONRPCMessage>,
    internal_tx: UnboundedSender<InternalEvent>,
    project_id: String,
    plan: ResumedProjectPlan,
) -> anyhow::Result<()> {
    anyhow::ensure!(running.is_none(), "internal error: project already running");
    *resumed = None;

    let (interrupt_tx, interrupt_rx) = watch::channel(false);
    let project_id_for_event = project_id.clone();
    let handle = tokio::task::spawn_local(async move {
        match run_resumed_project(config, writer_tx.clone(), plan, interrupt_rx).await {
            Ok(ProjectRunExit::Completed) => {
                let _ = internal_tx.send(InternalEvent::ProjectFinished {
                    project_id: project_id_for_event,
                });
            }
            Ok(ProjectRunExit::Interrupted(project)) => {
                let _ = internal_tx.send(InternalEvent::ProjectInterrupted { project });
            }
            Err(err) => {
                eprintln!("potter app-server resumed project failed: {err:#}");
                let _ = internal_tx.send(InternalEvent::ProjectFinished {
                    project_id: project_id_for_event,
                });
            }
        }
    });

    *running = Some(RunningProject {
        project_id,
        handle,
        interrupt_tx,
    });

    Ok(())
}

async fn run_fresh_project(
    config: PotterAppServerConfig,
    writer_tx: UnboundedSender<JSONRPCMessage>,
    plan: FreshProjectPlan,
    interrupt_rx: watch::Receiver<bool>,
) -> anyhow::Result<ProjectRunExit> {
    let developer_prompt =
        crate::workflow::project::render_developer_prompt(&plan.progress_file_rel);
    let turn_prompt = crate::workflow::project::fixed_prompt()
        .trim_end()
        .to_string();

    let backend_event_mode = backend_event_mode_for_potter(plan.event_mode);

    let round_context = crate::workflow::round_runner::PotterRoundContext {
        codex_bin: config.codex_bin,
        developer_prompt,
        backend_launch: config.backend_launch,
        backend_event_mode,
        codex_compat_home: config.codex_compat_home,
        thread_cwd: Some(plan.workdir.clone()),
        turn_prompt,
        workdir: plan.workdir.clone(),
        progress_file_rel: plan.progress_file_rel.clone(),
        user_prompt_file: plan.progress_file_rel.clone(),
        git_commit_start: plan.git_commit_start.clone(),
        potter_rollout_path: plan.potter_rollout_path.clone(),
        project_started_at: plan.project_started_at,
    };

    let round_context_with_override =
        plan.initial_turn_prompt_override
            .clone()
            .map(
                |turn_prompt| crate::workflow::round_runner::PotterRoundContext {
                    turn_prompt,
                    ..round_context.clone()
                },
            );

    let mut ui = EventForwardingRoundUi::new(writer_tx, interrupt_rx);

    let mut outcome = PotterProjectOutcome::BudgetExhausted;

    for round_index in plan.round_start_index..plan.rounds_total {
        let current_round = round_index.saturating_add(1);
        let project_started = if plan.emit_project_started_event && round_index == 0 {
            Some(crate::workflow::round_runner::PotterProjectStartedInfo {
                user_message: Some(plan.user_message.clone()),
                working_dir: plan.workdir.clone(),
                project_dir: plan.project_dir_rel.clone(),
                user_prompt_file: plan.progress_file_rel.clone(),
            })
        } else {
            None
        };

        let round_context = if round_index == plan.round_start_index {
            round_context_with_override
                .as_ref()
                .unwrap_or(&round_context)
        } else {
            &round_context
        };

        let round_result = crate::workflow::round_runner::run_potter_round(
            &mut ui,
            round_context,
            crate::workflow::round_runner::PotterRoundOptions {
                pad_before_first_cell: round_index != plan.round_start_index,
                project_started,
                round_current: current_round,
                round_total: plan.rounds_total,
                project_succeeded_rounds: current_round,
            },
        )
        .await;

        let round_result = match round_result {
            Ok(result) => result,
            Err(err) => {
                let message = format!("{err:#}");
                ui.synthesize_round_fatal_closure(&message);
                outcome = PotterProjectOutcome::Fatal { message };
                break;
            }
        };

        match round_result.exit_reason {
            codex_tui::ExitReason::Completed => {
                if round_result.stop_due_to_finite_incantatem {
                    outcome = PotterProjectOutcome::Succeeded;
                    break;
                }
                if round_index.saturating_add(1) >= plan.rounds_total {
                    outcome = PotterProjectOutcome::BudgetExhausted;
                }
            }
            codex_tui::ExitReason::Interrupted => {
                let continuation_plan = plan.continuation_after_interrupt(round_index);
                return Ok(ProjectRunExit::Interrupted(Box::new(InterruptedProject {
                    project_id: plan
                        .workdir
                        .join(&plan.progress_file_rel)
                        .to_string_lossy()
                        .to_string(),
                    user_prompt_file: plan.progress_file_rel.clone(),
                    rounds_run: current_round,
                    plan: continuation_plan,
                })));
            }
            codex_tui::ExitReason::TaskFailed(message) => {
                outcome = PotterProjectOutcome::TaskFailed { message };
                break;
            }
            codex_tui::ExitReason::Fatal(message) => {
                outcome = PotterProjectOutcome::Fatal { message };
                break;
            }
            codex_tui::ExitReason::UserRequested => {
                outcome = PotterProjectOutcome::Fatal {
                    message: String::from("user requested"),
                };
                break;
            }
        }
    }

    ui.emit_marker(EventMsg::PotterProjectCompleted { outcome });
    Ok(ProjectRunExit::Completed)
}

async fn run_resumed_project(
    config: PotterAppServerConfig,
    writer_tx: UnboundedSender<JSONRPCMessage>,
    plan: ResumedProjectPlan,
    interrupt_rx: watch::Receiver<bool>,
) -> anyhow::Result<ProjectRunExit> {
    let ResumedProjectPlan {
        resumed,
        baseline_rounds,
        git_commit_start,
        potter_rollout_path,
        rounds_total,
        resume_policy,
        event_mode,
        project_started_at,
        ..
    } = plan;

    let developer_prompt =
        crate::workflow::project::render_developer_prompt(&resumed.progress_file_rel);
    let turn_prompt = crate::workflow::project::fixed_prompt()
        .trim_end()
        .to_string();

    let backend_event_mode = backend_event_mode_for_potter(event_mode);

    let round_context = crate::workflow::round_runner::PotterRoundContext {
        codex_bin: config.codex_bin,
        developer_prompt,
        backend_launch: config.backend_launch,
        backend_event_mode,
        codex_compat_home: config.codex_compat_home,
        thread_cwd: Some(resumed.resolved.workdir.clone()),
        turn_prompt,
        workdir: resumed.resolved.workdir.clone(),
        progress_file_rel: resumed.progress_file_rel.clone(),
        user_prompt_file: resumed.progress_file_rel.clone(),
        git_commit_start,
        potter_rollout_path,
        project_started_at,
    };

    let mut ui = EventForwardingRoundUi::new(writer_tx, interrupt_rx);

    if let Some(unfinished) = resumed.index.unfinished_round.clone()
        && matches!(resume_policy, ResumePolicy::ContinueUnfinishedRound)
    {
        let total_rounds = unfinished.round_total;

        let rollout_path =
            resolve_rollout_path_for_replay(&resumed.resolved, &unfinished.rollout_path);
        let (remaining_after_continue, replay_event_msgs) = match (|| {
            let remaining = remaining_rounds_including_current(
                unfinished.round_current,
                unfinished.round_total,
            )?;
            let remaining_after_continue = remaining.saturating_sub(1);

            let mut replay_event_msgs = Vec::new();
            if let Some(cfg) =
                synthesize_session_configured_event(unfinished.thread_id, rollout_path.clone())?
            {
                replay_event_msgs.push(EventMsg::SessionConfigured(cfg));
            }
            let mut rollout_events = read_upstream_rollout_event_msgs(&rollout_path)
                .with_context(|| format!("replay rollout {}", rollout_path.display()))?;
            replay_event_msgs.append(&mut rollout_events);
            Ok::<(u32, Vec<EventMsg>), anyhow::Error>((remaining_after_continue, replay_event_msgs))
        })() {
            Ok(values) => values,
            Err(err) => {
                let message = format!("{err:#}");
                ui.emit_marker(EventMsg::Error(ErrorEvent {
                    message: message.clone(),
                    codex_error_info: None,
                }));
                ui.emit_marker(EventMsg::PotterProjectCompleted {
                    outcome: PotterProjectOutcome::Fatal { message },
                });
                return Ok(ProjectRunExit::Completed);
            }
        };

        let mut rounds_run = 0u32;
        let mut outcome = PotterProjectOutcome::BudgetExhausted;

        let round_result = crate::workflow::round_runner::continue_potter_round(
            &mut ui,
            &round_context,
            crate::workflow::round_runner::PotterContinueRoundOptions {
                pad_before_first_cell: true,
                round_current: unfinished.round_current,
                round_total: total_rounds,
                project_succeeded_rounds: baseline_rounds.saturating_add(1),
                resume_thread_id: unfinished.thread_id,
                replay_event_msgs,
            },
        )
        .await;

        let round_result = match round_result {
            Ok(result) => result,
            Err(err) => {
                let message = format!("{err:#}");
                ui.synthesize_round_fatal_closure(&message);
                outcome = PotterProjectOutcome::Fatal { message };
                ui.emit_marker(EventMsg::PotterProjectCompleted { outcome });
                return Ok(ProjectRunExit::Completed);
            }
        };

        rounds_run = rounds_run.saturating_add(1);

        match round_result.exit_reason {
            codex_tui::ExitReason::Completed => {
                if round_result.stop_due_to_finite_incantatem {
                    outcome = PotterProjectOutcome::Succeeded;
                    ui.emit_marker(EventMsg::PotterProjectCompleted { outcome });
                    return Ok(ProjectRunExit::Completed);
                }
            }
            codex_tui::ExitReason::Interrupted => {
                outcome = PotterProjectOutcome::Fatal {
                    message: String::from("interrupted"),
                };
                ui.emit_marker(EventMsg::PotterProjectCompleted { outcome });
                return Ok(ProjectRunExit::Completed);
            }
            codex_tui::ExitReason::TaskFailed(message) => {
                outcome = PotterProjectOutcome::TaskFailed { message };
                ui.emit_marker(EventMsg::PotterProjectCompleted { outcome });
                return Ok(ProjectRunExit::Completed);
            }
            codex_tui::ExitReason::Fatal(message) => {
                outcome = PotterProjectOutcome::Fatal { message };
                ui.emit_marker(EventMsg::PotterProjectCompleted { outcome });
                return Ok(ProjectRunExit::Completed);
            }
            codex_tui::ExitReason::UserRequested => {
                outcome = PotterProjectOutcome::Fatal {
                    message: String::from("user requested"),
                };
                ui.emit_marker(EventMsg::PotterProjectCompleted { outcome });
                return Ok(ProjectRunExit::Completed);
            }
        }

        for offset in 0..remaining_after_continue {
            if rounds_run >= rounds_total {
                break;
            }
            let current_round = unfinished
                .round_current
                .saturating_add(offset.saturating_add(1));
            let project_succeeded_rounds = baseline_rounds.saturating_add(offset.saturating_add(2));
            let round_result = crate::workflow::round_runner::run_potter_round(
                &mut ui,
                &round_context,
                crate::workflow::round_runner::PotterRoundOptions {
                    pad_before_first_cell: true,
                    project_started: None,
                    round_current: current_round,
                    round_total: total_rounds,
                    project_succeeded_rounds,
                },
            )
            .await;

            let round_result = match round_result {
                Ok(result) => result,
                Err(err) => {
                    let message = format!("{err:#}");
                    ui.synthesize_round_fatal_closure(&message);
                    outcome = PotterProjectOutcome::Fatal { message };
                    break;
                }
            };

            rounds_run = rounds_run.saturating_add(1);

            match round_result.exit_reason {
                codex_tui::ExitReason::Completed => {
                    if round_result.stop_due_to_finite_incantatem {
                        outcome = PotterProjectOutcome::Succeeded;
                        break;
                    }
                }
                codex_tui::ExitReason::Interrupted => {
                    outcome = PotterProjectOutcome::Fatal {
                        message: String::from("interrupted"),
                    };
                    break;
                }
                codex_tui::ExitReason::TaskFailed(message) => {
                    outcome = PotterProjectOutcome::TaskFailed { message };
                    break;
                }
                codex_tui::ExitReason::Fatal(message) => {
                    outcome = PotterProjectOutcome::Fatal { message };
                    break;
                }
                codex_tui::ExitReason::UserRequested => {
                    outcome = PotterProjectOutcome::Fatal {
                        message: String::from("user requested"),
                    };
                    break;
                }
            }
        }

        ui.emit_marker(EventMsg::PotterProjectCompleted { outcome });
        return Ok(ProjectRunExit::Completed);
    }

    // No unfinished round to continue (or policy says to start new rounds).
    let mut rounds_run = 0u32;
    let mut outcome = PotterProjectOutcome::BudgetExhausted;
    while rounds_run < rounds_total {
        let current_round = rounds_run.saturating_add(1);
        let project_succeeded_rounds = baseline_rounds.saturating_add(current_round);
        let round_result = crate::workflow::round_runner::run_potter_round(
            &mut ui,
            &round_context,
            crate::workflow::round_runner::PotterRoundOptions {
                pad_before_first_cell: true,
                project_started: None,
                round_current: current_round,
                round_total: rounds_total,
                project_succeeded_rounds,
            },
        )
        .await;

        let round_result = match round_result {
            Ok(result) => result,
            Err(err) => {
                let message = format!("{err:#}");
                ui.synthesize_round_fatal_closure(&message);
                outcome = PotterProjectOutcome::Fatal { message };
                break;
            }
        };

        rounds_run = rounds_run.saturating_add(1);
        match round_result.exit_reason {
            codex_tui::ExitReason::Completed => {
                if round_result.stop_due_to_finite_incantatem {
                    outcome = PotterProjectOutcome::Succeeded;
                    break;
                }
                if rounds_run >= rounds_total {
                    outcome = PotterProjectOutcome::BudgetExhausted;
                }
            }
            codex_tui::ExitReason::Interrupted => {
                outcome = PotterProjectOutcome::Fatal {
                    message: String::from("interrupted"),
                };
                break;
            }
            codex_tui::ExitReason::TaskFailed(message) => {
                outcome = PotterProjectOutcome::TaskFailed { message };
                break;
            }
            codex_tui::ExitReason::Fatal(message) => {
                outcome = PotterProjectOutcome::Fatal { message };
                break;
            }
            codex_tui::ExitReason::UserRequested => {
                outcome = PotterProjectOutcome::Fatal {
                    message: String::from("user requested"),
                };
                break;
            }
        }
    }

    ui.emit_marker(EventMsg::PotterProjectCompleted { outcome });
    Ok(ProjectRunExit::Completed)
}

fn backend_event_mode_for_potter(mode: PotterEventMode) -> crate::app_server::AppServerEventMode {
    match mode {
        PotterEventMode::Interactive => crate::app_server::AppServerEventMode::Interactive,
        PotterEventMode::ExecJson => crate::app_server::AppServerEventMode::ExecJson,
    }
}

struct EventForwardingRoundUi {
    writer_tx: UnboundedSender<JSONRPCMessage>,
    interrupt_rx: watch::Receiver<bool>,
    token_usage: TokenUsage,
    thread_id: Option<ThreadId>,
    saw_round_finished: bool,
}

impl EventForwardingRoundUi {
    fn new(
        writer_tx: UnboundedSender<JSONRPCMessage>,
        interrupt_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            writer_tx,
            interrupt_rx,
            token_usage: TokenUsage::default(),
            thread_id: None,
            saw_round_finished: false,
        }
    }

    fn forward_event(&mut self, event: &Event) {
        if let EventMsg::TokenCount(ev) = &event.msg
            && let Some(info) = &ev.info
        {
            self.token_usage = info.total_token_usage.clone();
        }
        if let EventMsg::SessionConfigured(cfg) = &event.msg {
            self.thread_id = Some(cfg.session_id);
        }

        if matches!(&event.msg, EventMsg::PotterRoundFinished { .. }) {
            self.saw_round_finished = true;
        }

        let Ok(params) = serde_json::to_value(event) else {
            return;
        };
        let _ = self
            .writer_tx
            .send(JSONRPCMessage::Notification(JSONRPCNotification {
                method: POTTER_EVENT_NOTIFICATION_METHOD.to_string(),
                params: Some(params),
            }));
    }

    fn synthesize_round_fatal_closure(&mut self, message: &str) {
        let error = Event {
            id: "".to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: message.to_string(),
                codex_error_info: None,
            }),
        };
        self.forward_event(&error);

        if !self.saw_round_finished {
            let finished = Event {
                id: "".to_string(),
                msg: EventMsg::PotterRoundFinished {
                    outcome: PotterRoundOutcome::Fatal {
                        message: message.to_string(),
                    },
                },
            };
            self.forward_event(&finished);
        }
    }

    fn emit_marker(&mut self, msg: EventMsg) {
        let event = Event {
            id: "".to_string(),
            msg,
        };
        self.forward_event(&event);
    }
}

impl crate::workflow::round_runner::PotterRoundUi for EventForwardingRoundUi {
    fn set_project_started_at(&mut self, _started_at: Instant) {}

    fn render_round<'a>(
        &'a mut self,
        params: codex_tui::RenderRoundParams,
    ) -> crate::workflow::round_runner::UiFuture<'a, codex_tui::AppExitInfo> {
        Box::pin(async move {
            let codex_tui::RenderRoundParams {
                prompt,
                codex_op_tx,
                mut codex_event_rx,
                mut fatal_exit_rx,
                ..
            } = params;

            self.token_usage = TokenUsage::default();
            self.thread_id = None;
            self.saw_round_finished = false;

            codex_op_tx
                .send(codex_protocol::protocol::Op::UserInput {
                    items: vec![UserInput::Text {
                        text: prompt,
                        text_elements: Vec::new(),
                    }],
                    final_output_json_schema: None,
                })
                .map_err(|_| anyhow::anyhow!("codex op channel closed"))?;

            let mut interrupt_sent = false;
            if *self.interrupt_rx.borrow() {
                let _ = codex_op_tx.send(codex_protocol::protocol::Op::Interrupt);
                interrupt_sent = true;
            }

            loop {
                while let Ok(event) = codex_event_rx.try_recv() {
                    self.forward_event(&event);
                    if let EventMsg::PotterRoundFinished { outcome } = &event.msg {
                        return Ok(codex_tui::AppExitInfo {
                            token_usage: self.token_usage.clone(),
                            thread_id: self.thread_id,
                            exit_reason: exit_reason_from_outcome(outcome),
                        });
                    }
                }

                if let Ok(message) = fatal_exit_rx.try_recv() {
                    self.synthesize_round_fatal_closure(&message);
                    return Ok(codex_tui::AppExitInfo {
                        token_usage: self.token_usage.clone(),
                        thread_id: self.thread_id,
                        exit_reason: codex_tui::ExitReason::Fatal(message),
                    });
                }

                tokio::select! {
                    interrupt_changed = self.interrupt_rx.changed(), if !interrupt_sent => {
                        if interrupt_changed.is_ok() && *self.interrupt_rx.borrow() {
                            let _ = codex_op_tx.send(codex_protocol::protocol::Op::Interrupt);
                            interrupt_sent = true;
                        }
                    }
                    Some(message) = fatal_exit_rx.recv() => {
                        while let Ok(event) = codex_event_rx.try_recv() {
                            self.forward_event(&event);
                        }
                        self.synthesize_round_fatal_closure(&message);
                        return Ok(codex_tui::AppExitInfo {
                            token_usage: self.token_usage.clone(),
                            thread_id: self.thread_id,
                            exit_reason: codex_tui::ExitReason::Fatal(message),
                        });
                    }
                    maybe_event = codex_event_rx.recv() => {
                        let Some(event) = maybe_event else {
                            let message = "event stream closed unexpectedly".to_string();
                            self.synthesize_round_fatal_closure(&message);
                            return Ok(codex_tui::AppExitInfo {
                                token_usage: self.token_usage.clone(),
                                thread_id: self.thread_id,
                                exit_reason: codex_tui::ExitReason::Fatal(message),
                            });
                        };
                        self.forward_event(&event);
                        if let EventMsg::PotterRoundFinished { outcome } = &event.msg {
                            return Ok(codex_tui::AppExitInfo {
                                token_usage: self.token_usage.clone(),
                                thread_id: self.thread_id,
                                exit_reason: exit_reason_from_outcome(outcome),
                            });
                        }
                    }
                }
            }
        })
    }
}

fn exit_reason_from_outcome(outcome: &PotterRoundOutcome) -> codex_tui::ExitReason {
    match outcome {
        PotterRoundOutcome::Completed => codex_tui::ExitReason::Completed,
        PotterRoundOutcome::Interrupted => codex_tui::ExitReason::Interrupted,
        PotterRoundOutcome::UserRequested => codex_tui::ExitReason::UserRequested,
        PotterRoundOutcome::TaskFailed { message } => {
            codex_tui::ExitReason::TaskFailed(message.clone())
        }
        PotterRoundOutcome::Fatal { message } => codex_tui::ExitReason::Fatal(message.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc::UnboundedReceiver;

    #[test]
    fn decode_jsonrpc_message_line_errors_on_invalid_json() {
        let err = decode_jsonrpc_message_line("{not json").expect_err("should fail");
        assert!(
            err.to_string()
                .contains("decode potter app-server JSON-RPC")
        );
    }

    #[test]
    fn decode_jsonrpc_message_line_ignores_empty_lines() {
        assert!(
            decode_jsonrpc_message_line(" \t ")
                .expect("decode")
                .is_none()
        );
    }

    #[test]
    fn count_completed_rounds_ignores_interrupted_outcome() {
        let lines = vec![
            crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Interrupted,
            },
            crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
            },
            crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::TaskFailed {
                    message: String::from("nope"),
                },
            },
            crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::UserRequested,
            },
        ];

        assert_eq!(count_completed_rounds(&lines), 3);
    }

    #[tokio::test]
    async fn event_forwarding_round_ui_sends_interrupt_and_waits_for_round_finished() {
        let (writer_tx, _writer_rx) = unbounded_channel::<JSONRPCMessage>();
        let (interrupt_tx, interrupt_rx) = watch::channel(false);

        let (codex_op_tx, mut codex_op_rx) = unbounded_channel::<codex_protocol::protocol::Op>();
        let (codex_event_tx, codex_event_rx) = unbounded_channel::<Event>();
        let (_fatal_exit_tx, fatal_exit_rx) = unbounded_channel::<String>();

        let params = codex_tui::RenderRoundParams {
            prompt: "Hello".to_string(),
            pad_before_first_cell: false,
            status_header_prefix: None,
            prompt_footer: codex_tui::PromptFooterContext::new(PathBuf::from("/tmp"), None),
            codex_op_tx,
            codex_event_rx,
            fatal_exit_rx,
        };

        let render = async move {
            let mut ui = EventForwardingRoundUi::new(writer_tx, interrupt_rx);
            crate::workflow::round_runner::PotterRoundUi::render_round(&mut ui, params).await
        };

        let driver = async move {
            let first_op = codex_op_rx.recv().await.expect("op");
            assert_eq!(
                first_op,
                codex_protocol::protocol::Op::UserInput {
                    items: vec![UserInput::Text {
                        text: "Hello".to_string(),
                        text_elements: Vec::new(),
                    }],
                    final_output_json_schema: None,
                }
            );

            interrupt_tx.send(true).expect("interrupt");

            let second_op = codex_op_rx.recv().await.expect("op");
            assert_eq!(second_op, codex_protocol::protocol::Op::Interrupt);

            codex_event_tx
                .send(Event {
                    id: String::new(),
                    msg: EventMsg::PotterRoundFinished {
                        outcome: PotterRoundOutcome::UserRequested,
                    },
                })
                .expect("round finished");
        };

        let (exit_info, ()) = tokio::join!(render, driver);
        let exit_info = exit_info.expect("render");
        assert!(matches!(
            exit_info.exit_reason,
            codex_tui::ExitReason::UserRequested
        ));
    }

    #[tokio::test]
    async fn start_rounds_without_resumed_project_returns_jsonrpc_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = PotterAppServerConfig {
            default_workdir: temp.path().to_path_buf(),
            codex_bin: "codex".to_string(),
            backend_launch: crate::app_server::AppServerLaunchConfig {
                spawn_sandbox: None,
                thread_sandbox: None,
                bypass_approvals_and_sandbox: false,
            },
            codex_compat_home: None,
            rounds: NonZeroUsize::new(1).expect("nonzero rounds"),
        };
        let mut state = ServerState {
            config,
            running: None,
            resumed: None,
            interrupted: None,
        };

        let (writer_tx, mut writer_rx) = unbounded_channel::<JSONRPCMessage>();
        let (internal_tx, _internal_rx) = unbounded_channel::<InternalEvent>();

        handle_request(
            JSONRPCRequest {
                id: RequestId::Integer(1),
                method: "project/start_rounds".to_string(),
                params: Some(serde_json::json!({
                    "projectId": "project_1",
                    "rounds": 1,
                })),
            },
            &mut state,
            &writer_tx,
            &internal_tx,
        )
        .await
        .expect("handle request");

        let msg = writer_rx.recv().await.expect("response");
        let JSONRPCMessage::Error(error) = msg else {
            panic!("expected JSONRPC error response, got {msg:?}");
        };
        assert_eq!(error.id, RequestId::Integer(1));
        assert_eq!(error.error.code, -32000);
        assert!(
            error.error.message.contains("no resumed project is active"),
            "unexpected error message: {:?}",
            error.error.message
        );
    }

    #[tokio::test]
    async fn resumed_project_missing_rollout_emits_project_completed_marker() {
        let temp = tempfile::tempdir().expect("tempdir");

        let config = PotterAppServerConfig {
            default_workdir: temp.path().to_path_buf(),
            codex_bin: "codex".to_string(),
            backend_launch: crate::app_server::AppServerLaunchConfig {
                spawn_sandbox: None,
                thread_sandbox: None,
                bypass_approvals_and_sandbox: false,
            },
            codex_compat_home: None,
            rounds: NonZeroUsize::new(1).expect("nonzero rounds"),
        };

        let workdir = temp.path().to_path_buf();
        let project_dir = temp.path().join("project");
        let progress_file = project_dir.join("MAIN.md");
        let resolved = crate::workflow::resume::ResolvedProjectPaths {
            progress_file,
            project_dir: project_dir.clone(),
            workdir: workdir.clone(),
        };

        let project_id = "project_1".to_string();
        let progress_file_rel = PathBuf::from(".codexpotter/projects/2026/03/04/6/MAIN.md");

        let index = crate::workflow::rollout_resume_index::PotterRolloutResumeIndex {
            project_started: crate::workflow::rollout_resume_index::ProjectStartedIndex {
                user_message: Some("hello".to_string()),
                user_prompt_file: progress_file_rel.clone(),
            },
            completed_rounds: Vec::new(),
            unfinished_round: Some(
                crate::workflow::rollout_resume_index::UnfinishedRoundIndex {
                    round_current: 1,
                    round_total: 1,
                    thread_id: ThreadId::default(),
                    rollout_path: PathBuf::from("missing-rollout.jsonl"),
                },
            ),
        };

        let plan = ResumedProjectPlan {
            resumed: ResumedProject {
                project_id: project_id.clone(),
                resolved,
                progress_file_rel: progress_file_rel.clone(),
                potter_rollout_lines: Vec::new(),
                index,
            },
            baseline_rounds: 0,
            git_commit_start: String::new(),
            potter_rollout_path: temp.path().join("potter-rollout.jsonl"),
            rounds_total: 1,
            resume_policy: ResumePolicy::ContinueUnfinishedRound,
            event_mode: PotterEventMode::Interactive,
            project_started_at: Instant::now(),
        };

        let (writer_tx, writer_rx) = unbounded_channel::<JSONRPCMessage>();
        let (_interrupt_tx, interrupt_rx) = watch::channel(false);

        run_resumed_project(config, writer_tx, plan, interrupt_rx)
            .await
            .expect("run resumed project");

        let events = drain_potter_events(writer_rx);
        assert!(
            events
                .iter()
                .any(|event| matches!(event.msg, EventMsg::Error(_))),
            "expected an Error event, got {events:?}"
        );
        let completed = events
            .iter()
            .find_map(|event| match &event.msg {
                EventMsg::PotterProjectCompleted { outcome } => Some(outcome),
                _ => None,
            })
            .expect("PotterProjectCompleted marker");

        assert!(
            matches!(completed, PotterProjectOutcome::Fatal { .. }),
            "expected fatal outcome, got {completed:?}"
        );
    }

    #[tokio::test]
    async fn interrupt_project_sets_interrupt_flag_on_first_request_and_keeps_running_state() {
        let temp = tempfile::tempdir().expect("tempdir");

        let config = PotterAppServerConfig {
            default_workdir: temp.path().to_path_buf(),
            codex_bin: "codex".to_string(),
            backend_launch: crate::app_server::AppServerLaunchConfig {
                spawn_sandbox: None,
                thread_sandbox: None,
                bypass_approvals_and_sandbox: false,
            },
            codex_compat_home: None,
            rounds: NonZeroUsize::new(1).expect("nonzero rounds"),
        };

        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        let (interrupt_tx, interrupt_rx) = watch::channel(false);

        let mut state = ServerState {
            config,
            running: Some(RunningProject {
                project_id: "project_1".to_string(),
                handle,
                interrupt_tx,
            }),
            resumed: None,
            interrupted: None,
        };

        let (writer_tx, mut writer_rx) = unbounded_channel::<JSONRPCMessage>();
        let (internal_tx, _internal_rx) = unbounded_channel::<InternalEvent>();

        handle_request(
            JSONRPCRequest {
                id: RequestId::Integer(1),
                method: "project/interrupt".to_string(),
                params: Some(serde_json::json!({
                    "projectId": "project_1",
                })),
            },
            &mut state,
            &writer_tx,
            &internal_tx,
        )
        .await
        .expect("handle request");

        let msg = writer_rx.recv().await.expect("response");
        let JSONRPCMessage::Response(response) = msg else {
            panic!("expected JSONRPC response, got {msg:?}");
        };
        assert_eq!(response.id, RequestId::Integer(1));
        assert_eq!(response.result, serde_json::json!({}));

        assert!(
            state.running.is_some(),
            "expected running project to remain active; got state.running={:?}",
            state.running
        );
        assert!(
            *interrupt_rx.borrow(),
            "expected interrupt flag to be set on first request"
        );

        let running = state.running.take().expect("running project");
        running.handle.abort();
        let _ = running.handle.await;
    }

    #[tokio::test]
    async fn interrupt_project_force_aborts_on_second_request() {
        let temp = tempfile::tempdir().expect("tempdir");

        let config = PotterAppServerConfig {
            default_workdir: temp.path().to_path_buf(),
            codex_bin: "codex".to_string(),
            backend_launch: crate::app_server::AppServerLaunchConfig {
                spawn_sandbox: None,
                thread_sandbox: None,
                bypass_approvals_and_sandbox: false,
            },
            codex_compat_home: None,
            rounds: NonZeroUsize::new(1).expect("nonzero rounds"),
        };

        struct DropNotify(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for DropNotify {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }

        let (drop_tx, drop_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let notify = DropNotify(Some(drop_tx));
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            drop(notify);
        });
        tokio::task::yield_now().await;

        let (interrupt_tx, _interrupt_rx) = watch::channel(false);

        let mut state = ServerState {
            config,
            running: Some(RunningProject {
                project_id: "project_1".to_string(),
                handle,
                interrupt_tx,
            }),
            resumed: None,
            interrupted: None,
        };

        let (writer_tx, mut writer_rx) = unbounded_channel::<JSONRPCMessage>();
        let (internal_tx, _internal_rx) = unbounded_channel::<InternalEvent>();

        for request_id in [1, 2] {
            handle_request(
                JSONRPCRequest {
                    id: RequestId::Integer(request_id),
                    method: "project/interrupt".to_string(),
                    params: Some(serde_json::json!({
                        "projectId": "project_1",
                    })),
                },
                &mut state,
                &writer_tx,
                &internal_tx,
            )
            .await
            .expect("handle request");

            let msg = writer_rx.recv().await.expect("response");
            let JSONRPCMessage::Response(response) = msg else {
                panic!("expected JSONRPC response, got {msg:?}");
            };
            assert_eq!(response.id, RequestId::Integer(request_id));
            assert_eq!(response.result, serde_json::json!({}));
        }

        assert!(
            state.running.is_none(),
            "expected running project to be force-aborted on second interrupt; got state.running={:?}",
            state.running
        );

        tokio::task::yield_now().await;
        tokio::time::timeout(std::time::Duration::from_secs(1), drop_rx)
            .await
            .expect("expected aborted task to be dropped")
            .expect("drop notify");
    }

    #[tokio::test]
    async fn interrupt_project_id_mismatch_returns_jsonrpc_error_and_preserves_state() {
        let temp = tempfile::tempdir().expect("tempdir");

        let config = PotterAppServerConfig {
            default_workdir: temp.path().to_path_buf(),
            codex_bin: "codex".to_string(),
            backend_launch: crate::app_server::AppServerLaunchConfig {
                spawn_sandbox: None,
                thread_sandbox: None,
                bypass_approvals_and_sandbox: false,
            },
            codex_compat_home: None,
            rounds: NonZeroUsize::new(1).expect("nonzero rounds"),
        };

        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        let (interrupt_tx, _interrupt_rx) = watch::channel(false);

        let mut state = ServerState {
            config,
            running: Some(RunningProject {
                project_id: "project_1".to_string(),
                handle,
                interrupt_tx,
            }),
            resumed: None,
            interrupted: None,
        };

        let (writer_tx, mut writer_rx) = unbounded_channel::<JSONRPCMessage>();
        let (internal_tx, _internal_rx) = unbounded_channel::<InternalEvent>();

        handle_request(
            JSONRPCRequest {
                id: RequestId::Integer(1),
                method: "project/interrupt".to_string(),
                params: Some(serde_json::json!({
                    "projectId": "project_2",
                })),
            },
            &mut state,
            &writer_tx,
            &internal_tx,
        )
        .await
        .expect("handle request");

        let msg = writer_rx.recv().await.expect("response");
        let JSONRPCMessage::Error(error) = msg else {
            panic!("expected JSONRPC error response, got {msg:?}");
        };
        assert_eq!(error.id, RequestId::Integer(1));
        assert_eq!(error.error.code, -32000);
        assert!(
            error.error.message.contains("mismatch"),
            "unexpected error message: {:?}",
            error.error.message
        );

        assert!(
            state
                .running
                .as_ref()
                .is_some_and(|running| running.project_id == "project_1"),
            "expected running project to be preserved; got state.running={:?}",
            state.running
        );

        let running = state.running.take().expect("running project");
        running.handle.abort();
        let _ = running.handle.await;
    }

    #[tokio::test]
    async fn clear_finished_running_project_drops_stale_state() {
        let temp = tempfile::tempdir().expect("tempdir");

        let config = PotterAppServerConfig {
            default_workdir: temp.path().to_path_buf(),
            codex_bin: "codex".to_string(),
            backend_launch: crate::app_server::AppServerLaunchConfig {
                spawn_sandbox: None,
                thread_sandbox: None,
                bypass_approvals_and_sandbox: false,
            },
            codex_compat_home: None,
            rounds: NonZeroUsize::new(1).expect("nonzero rounds"),
        };

        let handle = tokio::spawn(async {});
        let (interrupt_tx, _interrupt_rx) = watch::channel(false);

        let mut state = ServerState {
            config,
            running: Some(RunningProject {
                project_id: "project_1".to_string(),
                handle,
                interrupt_tx,
            }),
            resumed: None,
            interrupted: None,
        };

        tokio::task::yield_now().await;

        clear_finished_running_project(&mut state);

        assert!(
            state.running.is_none(),
            "expected running state to be cleared for finished tasks; got {:?}",
            state.running
        );
    }

    #[test]
    fn resolve_interrupt_continue_requires_turn_prompt_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path().to_path_buf();

        let config = PotterAppServerConfig {
            default_workdir: workdir.clone(),
            codex_bin: "codex".to_string(),
            backend_launch: crate::app_server::AppServerLaunchConfig {
                spawn_sandbox: None,
                thread_sandbox: None,
                bypass_approvals_and_sandbox: false,
            },
            codex_compat_home: None,
            rounds: NonZeroUsize::new(1).expect("nonzero rounds"),
        };

        let plan = FreshProjectPlan {
            workdir: workdir.clone(),
            user_message: "hello".to_string(),
            project_dir_rel: PathBuf::from(".codexpotter/projects/2026/03/06/1"),
            progress_file_rel: PathBuf::from(".codexpotter/projects/2026/03/06/1/MAIN.md"),
            git_commit_start: String::new(),
            potter_rollout_path: workdir.join("potter-rollout.jsonl"),
            rounds_total: 1,
            event_mode: PotterEventMode::Interactive,
            project_started_at: Instant::now(),
            round_start_index: 0,
            emit_project_started_event: true,
            initial_turn_prompt_override: None,
        };

        let interrupted_project = InterruptedProject {
            project_id: "project_1".to_string(),
            user_prompt_file: plan.progress_file_rel.clone(),
            rounds_run: 1,
            plan,
        };

        let mut state = ServerState {
            config,
            running: None,
            resumed: None,
            interrupted: Some(interrupted_project),
        };

        let (writer_tx, _writer_rx) = unbounded_channel::<JSONRPCMessage>();
        let (internal_tx, _internal_rx) = unbounded_channel::<InternalEvent>();

        let err = resolve_interrupt_project(
            &mut state,
            ProjectResolveInterruptParams {
                project_id: "project_1".to_string(),
                action: ResolveInterruptAction::Continue,
                turn_prompt_override: None,
            },
            &writer_tx,
            &internal_tx,
        )
        .expect_err("expected resolve_interrupt to fail without override");
        assert!(
            err.to_string()
                .contains("turn_prompt_override is required for continue"),
            "unexpected error: {err:#}"
        );
        assert!(
            state.interrupted.is_some(),
            "expected interrupted state to remain on validation failure"
        );
    }

    #[test]
    fn fresh_project_plan_continuation_after_interrupt_retries_same_round() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path().to_path_buf();

        let plan = FreshProjectPlan {
            workdir: workdir.clone(),
            user_message: "hello".to_string(),
            project_dir_rel: PathBuf::from(".codexpotter/projects/2026/03/06/1"),
            progress_file_rel: PathBuf::from(".codexpotter/projects/2026/03/06/1/MAIN.md"),
            git_commit_start: String::from("start"),
            potter_rollout_path: workdir.join("potter-rollout.jsonl"),
            rounds_total: 3,
            event_mode: PotterEventMode::Interactive,
            project_started_at: Instant::now(),
            round_start_index: 0,
            emit_project_started_event: true,
            initial_turn_prompt_override: Some(String::from("override")),
        };

        let continuation = plan.continuation_after_interrupt(0);
        assert_eq!(continuation.round_start_index, 0);
        assert!(!continuation.emit_project_started_event);
        assert!(continuation.initial_turn_prompt_override.is_none());
        assert_eq!(continuation.rounds_total, 3);
        assert_eq!(continuation.workdir, plan.workdir);
        assert_eq!(continuation.progress_file_rel, plan.progress_file_rel);
    }

    #[test]
    fn fresh_project_plan_continuation_after_interrupt_allows_retry_on_last_round() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path().to_path_buf();

        let plan = FreshProjectPlan {
            workdir: workdir.clone(),
            user_message: "hello".to_string(),
            project_dir_rel: PathBuf::from(".codexpotter/projects/2026/03/06/1"),
            progress_file_rel: PathBuf::from(".codexpotter/projects/2026/03/06/1/MAIN.md"),
            git_commit_start: String::from("start"),
            potter_rollout_path: workdir.join("potter-rollout.jsonl"),
            rounds_total: 1,
            event_mode: PotterEventMode::Interactive,
            project_started_at: Instant::now(),
            round_start_index: 0,
            emit_project_started_event: true,
            initial_turn_prompt_override: None,
        };

        let continuation = plan.continuation_after_interrupt(0);
        assert_eq!(continuation.round_start_index, 0);
        assert!(
            continuation.round_start_index < continuation.rounds_total,
            "expected continuation to retry the last round instead of exhausting the budget"
        );
    }

    #[test]
    fn resolve_interrupt_stop_returns_summary_and_emits_completed_marker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workdir = temp.path().to_path_buf();

        let expected_git_commit_end = crate::workflow::project::resolve_git_commit(&workdir);

        let config = PotterAppServerConfig {
            default_workdir: workdir.clone(),
            codex_bin: "codex".to_string(),
            backend_launch: crate::app_server::AppServerLaunchConfig {
                spawn_sandbox: None,
                thread_sandbox: None,
                bypass_approvals_and_sandbox: false,
            },
            codex_compat_home: None,
            rounds: NonZeroUsize::new(1).expect("nonzero rounds"),
        };

        let progress_file_rel = PathBuf::from(".codexpotter/projects/2026/03/06/1/MAIN.md");

        let plan = FreshProjectPlan {
            workdir: workdir.clone(),
            user_message: "hello".to_string(),
            project_dir_rel: PathBuf::from(".codexpotter/projects/2026/03/06/1"),
            progress_file_rel: progress_file_rel.clone(),
            git_commit_start: String::from("start"),
            potter_rollout_path: workdir.join("potter-rollout.jsonl"),
            rounds_total: 3,
            event_mode: PotterEventMode::Interactive,
            project_started_at: Instant::now(),
            round_start_index: 1,
            emit_project_started_event: false,
            initial_turn_prompt_override: None,
        };

        let interrupted_project = InterruptedProject {
            project_id: "project_1".to_string(),
            user_prompt_file: progress_file_rel.clone(),
            rounds_run: 2,
            plan,
        };

        let mut state = ServerState {
            config,
            running: None,
            resumed: None,
            interrupted: Some(interrupted_project),
        };

        let (writer_tx, writer_rx) = unbounded_channel::<JSONRPCMessage>();
        let (internal_tx, _internal_rx) = unbounded_channel::<InternalEvent>();

        let response = resolve_interrupt_project(
            &mut state,
            ProjectResolveInterruptParams {
                project_id: "project_1".to_string(),
                action: ResolveInterruptAction::Stop,
                turn_prompt_override: None,
            },
            &writer_tx,
            &internal_tx,
        )
        .expect("resolve_interrupt stop");

        assert!(
            state.interrupted.is_none(),
            "expected interrupted state cleared"
        );

        let summary = response.summary.expect("summary");
        assert_eq!(summary.rounds, 2);
        assert_eq!(summary.user_prompt_file, progress_file_rel);
        assert_eq!(summary.git_commit_start, "start");
        assert_eq!(summary.git_commit_end, expected_git_commit_end);

        let events = drain_potter_events(writer_rx);
        let completed = events
            .iter()
            .find_map(|event| match &event.msg {
                EventMsg::PotterProjectCompleted { outcome } => Some(outcome),
                _ => None,
            })
            .expect("PotterProjectCompleted marker");
        assert_eq!(*completed, PotterProjectOutcome::Interrupted);
    }

    fn drain_potter_events(mut writer_rx: UnboundedReceiver<JSONRPCMessage>) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(msg) = writer_rx.try_recv() {
            let JSONRPCMessage::Notification(notification) = msg else {
                continue;
            };
            if notification.method != POTTER_EVENT_NOTIFICATION_METHOD {
                continue;
            }
            let Some(params) = notification.params else {
                continue;
            };
            let Ok(event) = serde_json::from_value::<Event>(params) else {
                continue;
            };
            events.push(event);
        }
        events
    }
}
