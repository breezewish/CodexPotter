use std::collections::VecDeque;

use anyhow::Context;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::PotterProjectOutcome;
use codex_tui::ExitReason;
use tokio::sync::mpsc::unbounded_channel;

use crate::workflow::round_runner::PotterRoundUi;
use crate::workflow::round_runner::UiFuture;

/// Event source for a running Potter project hosted by `codex-potter app-server`.
pub trait PotterEventSource {
    /// Read the next event from the server stream.
    fn read_next_event<'a>(&'a mut self) -> UiFuture<'a, Option<Event>>;
}

/// Options for rendering a running Potter project (multi-round) from an event stream.
#[derive(Debug, Clone)]
pub struct PotterProjectRenderOptions {
    /// Per-round prompt string passed to the round renderer.
    pub turn_prompt: String,
    /// Footer context (working dir + git branch) used by the TUI.
    pub prompt_footer: codex_tui::PromptFooterContext,
    /// Whether to pad the transcript before the first round.
    pub pad_before_first_cell: bool,
    /// Optional status header prefix to use for the first round when no `PotterRoundStarted`
    /// boundary event is expected (for example, continuing an unfinished round on resume).
    pub initial_status_header_prefix: Option<String>,
}

/// Outcome of rendering a Potter project from a server event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PotterProjectRenderExit {
    /// The project ended normally, and the completion marker was observed.
    Completed { outcome: PotterProjectOutcome },
    /// The user requested exit while a round UI was running.
    UserRequested,
    /// The UI requested a fatal exit.
    FatalExitRequested,
}

/// Render a running Potter project by consuming `EventMsg` notifications from a server stream.
pub async fn run_potter_project_render_loop<U, S>(
    ui: &mut U,
    event_source: &mut S,
    project_id: &str,
    options: PotterProjectRenderOptions,
    buffered_events: Vec<Event>,
) -> anyhow::Result<PotterProjectRenderExit>
where
    U: PotterRoundUi,
    S: PotterEventSource,
{
    let PotterProjectRenderOptions {
        turn_prompt,
        prompt_footer,
        pad_before_first_cell,
        initial_status_header_prefix,
    } = options;

    let mut pending_events = VecDeque::from(buffered_events);
    let mut project_outcome: Option<PotterProjectOutcome> = None;
    let mut rendered_rounds: u32 = 0;
    let mut pending_initial_status_header_prefix = initial_status_header_prefix;

    loop {
        if let Some(outcome) = project_outcome.clone() {
            return Ok(PotterProjectRenderExit::Completed { outcome });
        }

        let status_header_prefix = if let Some(prefix) = pending_initial_status_header_prefix.take()
        {
            Some(prefix)
        } else {
            wait_for_next_round_prefix(
                project_id,
                event_source,
                &mut pending_events,
                &mut project_outcome,
            )
            .await?
        };

        let Some(status_header_prefix) = status_header_prefix else {
            let outcome = project_outcome
                .clone()
                .unwrap_or(PotterProjectOutcome::Fatal {
                    message: format!(
                        "missing PotterProjectCompleted marker (project_id={project_id})"
                    ),
                });
            return Ok(PotterProjectRenderExit::Completed { outcome });
        };

        let pad_before_first_cell = pad_before_first_cell || rendered_rounds != 0;
        let (op_tx, mut op_rx) = unbounded_channel::<Op>();
        tokio::spawn(async move { while op_rx.recv().await.is_some() {} });

        let (event_tx, event_rx) = unbounded_channel::<Event>();
        let (fatal_exit_tx, fatal_exit_rx) = unbounded_channel::<String>();

        let mut render = Box::pin(ui.render_round(codex_tui::RenderRoundParams {
            prompt: turn_prompt.clone(),
            pad_before_first_cell,
            status_header_prefix: Some(status_header_prefix),
            prompt_footer: prompt_footer.clone(),
            codex_op_tx: op_tx.clone(),
            codex_event_rx: event_rx,
            fatal_exit_rx,
        }));

        let mut waiting_for_render_exit = false;

        loop {
            if waiting_for_render_exit {
                let exit_info = render.await?;
                rendered_rounds = rendered_rounds.saturating_add(1);
                match exit_info.exit_reason {
                    ExitReason::UserRequested => return Ok(PotterProjectRenderExit::UserRequested),
                    ExitReason::Fatal(_) => {
                        return Ok(PotterProjectRenderExit::FatalExitRequested);
                    }
                    ExitReason::TaskFailed(_) | ExitReason::Completed => break,
                }
            }

            let next_event = async {
                if let Some(event) = pending_events.pop_front() {
                    return Ok(Some(event));
                }
                event_source.read_next_event().await
            };

            tokio::select! {
                exit_info = &mut render => {
                    let exit_info = exit_info?;
                    rendered_rounds = rendered_rounds.saturating_add(1);
                    match exit_info.exit_reason {
                        ExitReason::UserRequested => return Ok(PotterProjectRenderExit::UserRequested),
                        ExitReason::Fatal(_) => return Ok(PotterProjectRenderExit::FatalExitRequested),
                        ExitReason::TaskFailed(_) | ExitReason::Completed => break,
                    }
                }
                maybe_event = next_event => {
                    let maybe_event = maybe_event.context("read potter app-server event")?;
                    let Some(event) = maybe_event else {
                        let message = format!(
                            "potter app-server event stream closed unexpectedly (project_id={project_id})"
                        );
                        project_outcome = Some(PotterProjectOutcome::Fatal { message: message.clone() });
                        let _ = fatal_exit_tx.send(message);
                        continue;
                    };

                    if let EventMsg::PotterProjectCompleted { outcome } = &event.msg {
                        project_outcome = Some(outcome.clone());
                        continue;
                    }

                    let saw_round_finished = matches!(&event.msg, EventMsg::PotterRoundFinished { .. });
                    if event_tx.send(event).is_err() {
                        continue;
                    }
                    if saw_round_finished {
                        // Avoid reading events for the next round until the UI exits.
                        waiting_for_render_exit = true;
                    }
                }
            }
        }
    }
}

async fn wait_for_next_round_prefix<S>(
    project_id: &str,
    event_source: &mut S,
    pending_events: &mut VecDeque<Event>,
    project_outcome: &mut Option<PotterProjectOutcome>,
) -> anyhow::Result<Option<String>>
where
    S: PotterEventSource,
{
    loop {
        if project_outcome.is_some() {
            return Ok(None);
        }

        for event in pending_events.iter() {
            if let EventMsg::PotterRoundStarted { current, total } = &event.msg {
                return Ok(Some(format!("Round {current}/{total}")));
            }
        }

        if let Some((idx, outcome)) =
            pending_events
                .iter()
                .enumerate()
                .find_map(|(idx, event)| match &event.msg {
                    EventMsg::PotterProjectCompleted { outcome } => Some((idx, outcome.clone())),
                    _ => None,
                })
        {
            let _ = pending_events.remove(idx);
            *project_outcome = Some(outcome);
            continue;
        }

        let Some(event) = event_source.read_next_event().await? else {
            let message = format!(
                "potter app-server event stream closed unexpectedly (project_id={project_id})"
            );
            *project_outcome = Some(PotterProjectOutcome::Fatal { message });
            return Ok(None);
        };

        if let EventMsg::PotterProjectCompleted { outcome } = &event.msg {
            *project_outcome = Some(outcome.clone());
            continue;
        }

        pending_events.push_back(event);
    }
}
