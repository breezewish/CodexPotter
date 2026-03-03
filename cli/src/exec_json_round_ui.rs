use std::io::Write;
use std::time::Instant;

use anyhow::Context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::PotterRoundOutcome;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::UserInput;
use codex_tui::AppExitInfo;
use codex_tui::ExitReason;

pub struct ExecJsonRoundUi<W: Write> {
    output: W,
    processor: crate::exec_jsonl::ExecJsonlEventProcessor,
    json_turn_open: bool,
    token_usage: TokenUsage,
    thread_id: Option<ThreadId>,
    saw_round_finished: bool,
}

impl<W: Write> ExecJsonRoundUi<W> {
    pub fn new(output: W) -> Self {
        Self {
            output,
            processor: crate::exec_jsonl::ExecJsonlEventProcessor::default(),
            json_turn_open: false,
            token_usage: TokenUsage::default(),
            thread_id: None,
            saw_round_finished: false,
        }
    }

    fn write_jsonl_event(&mut self, event: &crate::exec_jsonl::ExecJsonlEvent) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.output, event).context("serialize exec jsonl event")?;
        self.output
            .write_all(b"\n")
            .context("write exec jsonl newline")?;
        self.output.flush().context("flush exec jsonl output")?;
        Ok(())
    }

    fn observe_json_turn_state(&mut self, event: &crate::exec_jsonl::ExecJsonlEvent) {
        match event {
            crate::exec_jsonl::ExecJsonlEvent::TurnStarted(_) => self.json_turn_open = true,
            crate::exec_jsonl::ExecJsonlEvent::TurnCompleted(_)
            | crate::exec_jsonl::ExecJsonlEvent::TurnFailed(_) => self.json_turn_open = false,
            _ => {}
        }
    }

    fn handle_codex_event(&mut self, event: &Event) -> anyhow::Result<()> {
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

        let mapped = self.processor.collect_event(&event.msg);
        for mapped_event in mapped {
            self.observe_json_turn_state(&mapped_event);
            self.write_jsonl_event(&mapped_event)?;
        }
        Ok(())
    }

    fn synthesize_round_fatal_closure(&mut self, message: &str) -> anyhow::Result<()> {
        if self.json_turn_open {
            let event = crate::exec_jsonl::ExecJsonlEvent::TurnFailed(
                crate::exec_jsonl::TurnFailedEvent {
                    error: crate::exec_jsonl::ThreadErrorEvent {
                        message: message.to_string(),
                    },
                },
            );
            self.observe_json_turn_state(&event);
            self.write_jsonl_event(&event)?;
        }

        if !self.saw_round_finished {
            self.write_jsonl_event(&crate::exec_jsonl::ExecJsonlEvent::PotterRoundCompleted(
                crate::exec_jsonl::PotterRoundCompletedEvent {
                    outcome: crate::exec_jsonl::PotterRoundCompletedOutcome::Fatal,
                    message: Some(message.to_string()),
                },
            ))?;
            self.saw_round_finished = true;
        }

        Ok(())
    }
}

impl<W: Write> crate::round_runner::PotterRoundUi for ExecJsonRoundUi<W> {
    fn set_project_started_at(&mut self, _started_at: Instant) {}

    fn render_round<'a>(
        &'a mut self,
        params: codex_tui::RenderRoundParams,
    ) -> crate::round_runner::UiFuture<'a, AppExitInfo> {
        Box::pin(async move {
            let codex_tui::RenderRoundParams {
                prompt,
                codex_op_tx,
                mut codex_event_rx,
                mut fatal_exit_rx,
                ..
            } = params;

            codex_op_tx
                .send(Op::UserInput {
                    items: vec![UserInput::Text {
                        text: prompt,
                        text_elements: Vec::new(),
                    }],
                    final_output_json_schema: None,
                })
                .map_err(|_| anyhow::anyhow!("codex op channel closed"))?;

            loop {
                tokio::select! {
                    Some(message) = fatal_exit_rx.recv() => {
                        self.synthesize_round_fatal_closure(&message)?;
                        return Ok(AppExitInfo {
                            token_usage: self.token_usage.clone(),
                            thread_id: self.thread_id,
                            exit_reason: ExitReason::Fatal(message),
                        });
                    }
                    maybe_event = codex_event_rx.recv() => {
                        let Some(event) = maybe_event else {
                            let message = "codex event stream closed unexpectedly".to_string();
                            self.synthesize_round_fatal_closure(&message)?;
                            return Ok(AppExitInfo {
                                token_usage: self.token_usage.clone(),
                                thread_id: self.thread_id,
                                exit_reason: ExitReason::Fatal(message),
                            });
                        };

                        let exit_reason = match &event.msg {
                            EventMsg::PotterRoundFinished { outcome } => Some(exit_reason_from_outcome(outcome)),
                            _ => None,
                        };

                        self.handle_codex_event(&event)?;

                        if let Some(exit_reason) = exit_reason {
                            return Ok(AppExitInfo {
                                token_usage: self.token_usage.clone(),
                                thread_id: self.thread_id,
                                exit_reason,
                            });
                        }
                    }
                }
            }
        })
    }
}

fn exit_reason_from_outcome(outcome: &PotterRoundOutcome) -> ExitReason {
    match outcome {
        PotterRoundOutcome::Completed => ExitReason::Completed,
        PotterRoundOutcome::UserRequested => ExitReason::UserRequested,
        PotterRoundOutcome::TaskFailed { message } => ExitReason::TaskFailed(message.clone()),
        PotterRoundOutcome::Fatal { message } => ExitReason::Fatal(message.clone()),
    }
}
