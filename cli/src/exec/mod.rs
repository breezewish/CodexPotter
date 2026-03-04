mod jsonl;

#[cfg(test)]
mod json_round_ui;

pub use jsonl::*;

use std::io::Read as _;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::PotterProjectOutcome;

pub async fn run_exec_json(
    workdir: &Path,
    prompt: Option<String>,
    rounds: NonZeroUsize,
    codex_bin: String,
    backend_launch: crate::app_server::AppServerLaunchConfig,
) -> i32 {
    let prompt = match prompt {
        Some(prompt) => prompt,
        None => match read_prompt_from_stdin() {
            Ok(prompt) => prompt,
            Err(err) => {
                let _ = write_exec_json_preflight_error(&format!("{err:#}"));
                return 1;
            }
        },
    };

    if prompt.trim().is_empty() {
        let _ = write_exec_json_preflight_error("prompt is empty");
        return 1;
    }

    let rounds_total_u32 = u32::try_from(rounds.get()).unwrap_or(u32::MAX);

    let mut client = match crate::app_server::potter::PotterAppServerClient::spawn(
        workdir.to_path_buf(),
        codex_bin,
        rounds,
        backend_launch,
    )
    .await
    {
        Ok(client) => client,
        Err(err) => {
            let _ = write_exec_json_preflight_error(&format!("{err:#}"));
            return 1;
        }
    };

    if let Err(err) = client.initialize().await {
        let _ = write_exec_json_preflight_error(&format!("{err:#}"));
        return 1;
    }

    let mut buffered_events = Vec::new();
    let start_response = match client
        .project_start(
            crate::app_server::potter::ProjectStartParams {
                user_message: prompt.clone(),
                cwd: Some(workdir.to_path_buf()),
                rounds: Some(rounds_total_u32),
                event_mode: Some(crate::app_server::potter::PotterEventMode::ExecJson),
            },
            &mut buffered_events,
        )
        .await
    {
        Ok(response) => response,
        Err(err) => {
            let _ = write_exec_json_preflight_error(&format!("{err:#}"));
            let _ = client.shutdown().await;
            return 1;
        }
    };

    let stdout = std::io::stdout();
    let mut emitter = ExecJsonlEmitter::new(stdout.lock(), start_response.working_dir.clone());

    if emitter
        .write_jsonl_event(&crate::exec::ExecJsonlEvent::PotterProjectStarted(
            crate::exec::PotterProjectStartedEvent {
                working_dir: start_response.working_dir.to_string_lossy().to_string(),
                project_dir: start_response.project_dir.to_string_lossy().to_string(),
                progress_file: start_response.progress_file.to_string_lossy().to_string(),
                user_message: prompt.clone(),
                git_commit_start: start_response.git_commit_start.clone(),
                git_branch: start_response.git_branch.clone(),
            },
        ))
        .is_err()
    {
        let _ = client.shutdown().await;
        return 1;
    }

    let project_started_at = Instant::now();

    let mut final_outcome: Option<PotterProjectOutcome> = None;
    let mut should_interrupt_project = false;

    let mut buffered_iter = buffered_events.into_iter();
    while final_outcome.is_none() {
        let next = if let Some(event) = buffered_iter.next() {
            Some(event)
        } else {
            match client.read_next_event().await {
                Ok(event) => event,
                Err(err) => {
                    let message = format!("{err:#}");
                    should_interrupt_project = true;
                    if emitter.fail_fast_with_error(message.clone()).is_err() {
                        let _ = client.shutdown().await;
                        return 1;
                    }
                    final_outcome = Some(PotterProjectOutcome::Fatal { message });
                    break;
                }
            }
        };

        let Some(event) = next else {
            let message = "potter app-server event stream closed unexpectedly".to_string();
            should_interrupt_project = true;
            if emitter.fail_fast_with_error(message.clone()).is_err() {
                let _ = client.shutdown().await;
                return 1;
            }
            final_outcome = Some(PotterProjectOutcome::Fatal { message });
            break;
        };

        match emitter.process_event_msg(&event.msg) {
            Ok(ExecEventProgress::Continue) => {}
            Ok(ExecEventProgress::ProjectCompleted { outcome }) => {
                final_outcome = Some(outcome);
                break;
            }
            Ok(ExecEventProgress::FailFast { message }) => {
                should_interrupt_project = true;
                if emitter.fail_fast_with_error(message.clone()).is_err() {
                    let _ = client.shutdown().await;
                    return 1;
                }
                final_outcome = Some(PotterProjectOutcome::Fatal { message });
                break;
            }
            Err(err) => {
                let message = format!("{err:#}");
                should_interrupt_project = true;
                if emitter.fail_fast_with_error(message.clone()).is_err() {
                    let _ = client.shutdown().await;
                    return 1;
                }
                final_outcome = Some(PotterProjectOutcome::Fatal { message });
                break;
            }
        }
    }

    let rounds_run = emitter.rounds_run();

    if should_interrupt_project {
        let _ = client
            .project_interrupt(
                crate::app_server::potter::ProjectInterruptParams {
                    project_id: start_response.project_id.clone(),
                },
                &mut Vec::new(),
            )
            .await;
    }

    let final_outcome = final_outcome.unwrap_or_else(|| PotterProjectOutcome::Fatal {
        message: "missing PotterProjectCompleted marker".to_string(),
    });
    let (final_outcome_json, final_message) = exec_project_outcome(&final_outcome);

    let git_commit_end = crate::workflow::project::resolve_git_commit(&start_response.working_dir);
    let project_completed = crate::exec::ExecJsonlEvent::PotterProjectCompleted(
        crate::exec::PotterProjectCompletedEvent {
            outcome: final_outcome_json.clone(),
            message: final_message.clone(),
            rounds_run,
            rounds_total: start_response.rounds_total,
            duration_secs: project_started_at.elapsed().as_secs(),
            progress_file: start_response.progress_file.to_string_lossy().to_string(),
            git_commit_start: start_response.git_commit_start.clone(),
            git_commit_end,
            git_branch: start_response.git_branch.clone(),
        },
    );

    if emitter.write_jsonl_event(&project_completed).is_err() {
        let _ = client.shutdown().await;
        return 1;
    }

    let exit_code = if matches!(
        final_outcome_json,
        crate::exec::PotterProjectCompletedOutcome::Succeeded
    ) {
        0
    } else {
        1
    };

    let _ = client.shutdown().await;
    exit_code
}

fn read_prompt_from_stdin() -> anyhow::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

/// Emit a single `error` JSONL event to stdout for `exec --json` preflight failures.
pub fn write_exec_json_preflight_error(message: &str) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    write_jsonl_event(
        &mut out,
        &crate::exec::ExecJsonlEvent::Error(crate::exec::ThreadErrorEvent {
            message: message.to_string(),
        }),
    )
}

fn write_jsonl_event<W: Write>(
    out: &mut W,
    event: &crate::exec::ExecJsonlEvent,
) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *out, event)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecEventProgress {
    Continue,
    ProjectCompleted { outcome: PotterProjectOutcome },
    FailFast { message: String },
}

struct ExecJsonlEmitter<W: Write> {
    output: W,
    processor: crate::exec::ExecJsonlEventProcessor,
    json_turn_open: bool,
    round_in_progress: bool,
    rounds_run: u32,
}

impl<W: Write> ExecJsonlEmitter<W> {
    fn new(output: W, workdir: PathBuf) -> Self {
        Self {
            output,
            processor: crate::exec::ExecJsonlEventProcessor::with_workdir(workdir),
            json_turn_open: false,
            round_in_progress: false,
            rounds_run: 0,
        }
    }

    fn rounds_run(&self) -> u32 {
        self.rounds_run
    }

    fn write_jsonl_event(&mut self, event: &crate::exec::ExecJsonlEvent) -> anyhow::Result<()> {
        write_jsonl_event(&mut self.output, event).context("write exec jsonl event")?;
        self.observe_json_turn_state(event);
        Ok(())
    }

    fn observe_json_turn_state(&mut self, event: &crate::exec::ExecJsonlEvent) {
        match event {
            crate::exec::ExecJsonlEvent::TurnStarted(_) => self.json_turn_open = true,
            crate::exec::ExecJsonlEvent::TurnCompleted(_)
            | crate::exec::ExecJsonlEvent::TurnFailed(_) => self.json_turn_open = false,
            _ => {}
        }
    }

    fn start_round(&mut self) {
        self.processor.reset_round_state();
        self.json_turn_open = false;
        self.round_in_progress = true;
    }

    fn process_event_msg(&mut self, msg: &EventMsg) -> anyhow::Result<ExecEventProgress> {
        match msg {
            EventMsg::RequestUserInput(ev) => {
                let message = format!(
                    "unsupported interactive request: RequestUserInput call_id={}",
                    ev.call_id
                );
                return Ok(ExecEventProgress::FailFast { message });
            }
            EventMsg::ElicitationRequest(ev) => {
                let message = format!(
                    "unsupported interactive request: ElicitationRequest server_name={} request_id={}",
                    ev.server_name, ev.id
                );
                return Ok(ExecEventProgress::FailFast { message });
            }
            EventMsg::PotterProjectCompleted { outcome } => {
                return Ok(ExecEventProgress::ProjectCompleted {
                    outcome: outcome.clone(),
                });
            }
            _ => {}
        }

        if matches!(msg, EventMsg::PotterRoundStarted { .. }) {
            self.start_round();
        }

        for mapped in self.processor.collect_event(msg) {
            self.write_jsonl_event(&mapped)?;
        }

        if matches!(msg, EventMsg::PotterRoundFinished { .. }) {
            self.round_in_progress = false;
            self.rounds_run = self.rounds_run.saturating_add(1);
        }

        Ok(ExecEventProgress::Continue)
    }

    fn fail_fast_with_error(&mut self, message: String) -> anyhow::Result<()> {
        self.write_jsonl_event(&crate::exec::ExecJsonlEvent::Error(
            crate::exec::ThreadErrorEvent {
                message: message.clone(),
            },
        ))?;
        self.synthesize_round_fatal_closure(&message)?;
        Ok(())
    }

    fn synthesize_round_fatal_closure(&mut self, message: &str) -> anyhow::Result<()> {
        if self.json_turn_open {
            self.write_jsonl_event(&crate::exec::ExecJsonlEvent::TurnFailed(
                crate::exec::TurnFailedEvent {
                    error: crate::exec::ThreadErrorEvent {
                        message: message.to_string(),
                    },
                },
            ))?;
        }

        if self.round_in_progress {
            self.write_jsonl_event(&crate::exec::ExecJsonlEvent::PotterRoundCompleted(
                crate::exec::PotterRoundCompletedEvent {
                    outcome: crate::exec::PotterRoundCompletedOutcome::Fatal,
                    message: Some(message.to_string()),
                },
            ))?;
            self.round_in_progress = false;
            self.rounds_run = self.rounds_run.saturating_add(1);
        }

        Ok(())
    }
}

fn exec_project_outcome(
    outcome: &PotterProjectOutcome,
) -> (crate::exec::PotterProjectCompletedOutcome, Option<String>) {
    match outcome {
        PotterProjectOutcome::Succeeded => {
            (crate::exec::PotterProjectCompletedOutcome::Succeeded, None)
        }
        PotterProjectOutcome::BudgetExhausted => (
            crate::exec::PotterProjectCompletedOutcome::BudgetExhausted,
            None,
        ),
        PotterProjectOutcome::TaskFailed { message } => (
            crate::exec::PotterProjectCompletedOutcome::TaskFailed,
            Some(message.clone()),
        ),
        PotterProjectOutcome::Fatal { message } => (
            crate::exec::PotterProjectCompletedOutcome::Fatal,
            Some(message.clone()),
        ),
    }
}
