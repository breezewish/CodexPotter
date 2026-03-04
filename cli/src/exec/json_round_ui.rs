use std::io::Write;
use std::path::PathBuf;
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
    processor: crate::exec::ExecJsonlEventProcessor,
    json_turn_open: bool,
    token_usage: TokenUsage,
    thread_id: Option<ThreadId>,
    saw_round_finished: bool,
}

impl<W: Write> ExecJsonRoundUi<W> {
    pub fn new(output: W, workdir: PathBuf) -> Self {
        Self {
            output,
            processor: crate::exec::ExecJsonlEventProcessor::with_workdir(workdir),
            json_turn_open: false,
            token_usage: TokenUsage::default(),
            thread_id: None,
            saw_round_finished: false,
        }
    }

    pub fn into_output(self) -> W {
        self.output
    }

    fn write_jsonl_event(&mut self, event: &crate::exec::ExecJsonlEvent) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.output, event).context("serialize exec jsonl event")?;
        self.output
            .write_all(b"\n")
            .context("write exec jsonl newline")?;
        self.output.flush().context("flush exec jsonl output")?;
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

    fn fail_fast_with_error(&mut self, message: String) -> anyhow::Result<AppExitInfo> {
        self.write_jsonl_event(&crate::exec::ExecJsonlEvent::Error(
            crate::exec::ThreadErrorEvent {
                message: message.clone(),
            },
        ))?;
        self.synthesize_round_fatal_closure(&message)?;
        Ok(AppExitInfo {
            token_usage: self.token_usage.clone(),
            thread_id: self.thread_id,
            exit_reason: ExitReason::Fatal(message),
        })
    }

    fn process_event(&mut self, event: &Event) -> anyhow::Result<Option<AppExitInfo>> {
        match &event.msg {
            EventMsg::RequestUserInput(ev) => {
                let message = format!(
                    "unsupported interactive request: RequestUserInput call_id={}",
                    ev.call_id
                );
                return Ok(Some(self.fail_fast_with_error(message)?));
            }
            EventMsg::ElicitationRequest(ev) => {
                let message = format!(
                    "unsupported interactive request: ElicitationRequest server_name={} request_id={}",
                    ev.server_name, ev.id
                );
                return Ok(Some(self.fail_fast_with_error(message)?));
            }
            _ => {}
        }

        let exit_reason = match &event.msg {
            EventMsg::PotterRoundFinished { outcome } => Some(exit_reason_from_outcome(outcome)),
            _ => None,
        };

        self.handle_codex_event(event)?;

        let Some(exit_reason) = exit_reason else {
            return Ok(None);
        };

        Ok(Some(AppExitInfo {
            token_usage: self.token_usage.clone(),
            thread_id: self.thread_id,
            exit_reason,
        }))
    }

    fn synthesize_round_fatal_closure(&mut self, message: &str) -> anyhow::Result<()> {
        if self.json_turn_open {
            let event = crate::exec::ExecJsonlEvent::TurnFailed(crate::exec::TurnFailedEvent {
                error: crate::exec::ThreadErrorEvent {
                    message: message.to_string(),
                },
            });
            self.observe_json_turn_state(&event);
            self.write_jsonl_event(&event)?;
        }

        if !self.saw_round_finished {
            self.write_jsonl_event(&crate::exec::ExecJsonlEvent::PotterRoundCompleted(
                crate::exec::PotterRoundCompletedEvent {
                    outcome: crate::exec::PotterRoundCompletedOutcome::Fatal,
                    message: Some(message.to_string()),
                },
            ))?;
            self.saw_round_finished = true;
        }

        Ok(())
    }
}

impl<W: Write> crate::workflow::round_runner::PotterRoundUi for ExecJsonRoundUi<W> {
    fn set_project_started_at(&mut self, _started_at: Instant) {}

    fn render_round<'a>(
        &'a mut self,
        params: codex_tui::RenderRoundParams,
    ) -> crate::workflow::round_runner::UiFuture<'a, AppExitInfo> {
        Box::pin(async move {
            let codex_tui::RenderRoundParams {
                prompt,
                codex_op_tx,
                mut codex_event_rx,
                mut fatal_exit_rx,
                ..
            } = params;

            self.processor.reset_round_state();
            self.json_turn_open = false;
            self.token_usage = TokenUsage::default();
            self.thread_id = None;
            self.saw_round_finished = false;

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
                while let Ok(event) = codex_event_rx.try_recv() {
                    if let Some(exit_info) = self.process_event(&event)? {
                        return Ok(exit_info);
                    }
                }

                if let Ok(message) = fatal_exit_rx.try_recv() {
                    self.synthesize_round_fatal_closure(&message)?;
                    return Ok(AppExitInfo {
                        token_usage: self.token_usage.clone(),
                        thread_id: self.thread_id,
                        exit_reason: ExitReason::Fatal(message),
                    });
                }

                tokio::select! {
                    Some(message) = fatal_exit_rx.recv() => {
                        while let Ok(event) = codex_event_rx.try_recv() {
                            if let Some(exit_info) = self.process_event(&event)? {
                                return Ok(exit_info);
                            }
                        }

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

                        if let Some(exit_info) = self.process_event(&event)? {
                            return Ok(exit_info);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::round_runner::PotterRoundUi;
    use codex_protocol::approvals::ElicitationRequestEvent;
    use codex_protocol::mcp::RequestId;
    use codex_protocol::protocol::TurnStartedEvent;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc::unbounded_channel;

    #[tokio::test]
    async fn elicitation_request_fails_fast_with_closure_events() {
        let (codex_op_tx, mut codex_op_rx) = unbounded_channel::<Op>();
        let (codex_event_tx, codex_event_rx) = unbounded_channel::<Event>();
        let (_fatal_exit_tx, fatal_exit_rx) = unbounded_channel::<String>();

        codex_event_tx
            .send(Event {
                id: "round-start".to_string(),
                msg: EventMsg::PotterRoundStarted {
                    current: 1,
                    total: 3,
                },
            })
            .expect("send PotterRoundStarted");
        codex_event_tx
            .send(Event {
                id: "turn-start".to_string(),
                msg: EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    model_context_window: None,
                }),
            })
            .expect("send TurnStarted");
        codex_event_tx
            .send(Event {
                id: "elicitation".to_string(),
                msg: EventMsg::ElicitationRequest(ElicitationRequestEvent {
                    server_name: "mcp-server".to_string(),
                    id: RequestId::String("req-1".to_string()),
                    message: "need input".to_string(),
                }),
            })
            .expect("send ElicitationRequest");
        drop(codex_event_tx);

        let mut ui = ExecJsonRoundUi::new(Vec::new(), PathBuf::from("/tmp"));
        let exit_info = ui
            .render_round(codex_tui::RenderRoundParams {
                prompt: "Continue working according to the WORKFLOW_INSTRUCTIONS".to_string(),
                pad_before_first_cell: false,
                status_header_prefix: None,
                prompt_footer: codex_tui::PromptFooterContext::new(PathBuf::from("."), None),
                codex_op_tx,
                codex_event_rx,
                fatal_exit_rx,
            })
            .await
            .expect("render_round");

        let op = codex_op_rx.try_recv().expect("expected Op::UserInput");
        assert!(matches!(op, Op::UserInput { .. }));

        let ExitReason::Fatal(message) = &exit_info.exit_reason else {
            panic!("expected fatal exit, got: {:?}", exit_info.exit_reason);
        };
        assert!(
            message.contains("ElicitationRequest"),
            "message should mention ElicitationRequest"
        );

        let output = String::from_utf8(ui.into_output()).expect("utf8");
        let events = output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<crate::exec::ExecJsonlEvent>)
            .collect::<Result<Vec<_>, _>>()
            .expect("parse JSONL");

        let expected_error_message = "unsupported interactive request: ElicitationRequest server_name=mcp-server request_id=req-1".to_string();
        assert_eq!(
            events,
            vec![
                crate::exec::ExecJsonlEvent::PotterRoundStarted(
                    crate::exec::PotterRoundStartedEvent {
                        current: 1,
                        total: 3
                    }
                ),
                crate::exec::ExecJsonlEvent::TurnStarted(crate::exec::TurnStartedEvent {}),
                crate::exec::ExecJsonlEvent::Error(crate::exec::ThreadErrorEvent {
                    message: expected_error_message.clone(),
                }),
                crate::exec::ExecJsonlEvent::TurnFailed(crate::exec::TurnFailedEvent {
                    error: crate::exec::ThreadErrorEvent {
                        message: expected_error_message.clone(),
                    },
                }),
                crate::exec::ExecJsonlEvent::PotterRoundCompleted(
                    crate::exec::PotterRoundCompletedEvent {
                        outcome: crate::exec::PotterRoundCompletedOutcome::Fatal,
                        message: Some(expected_error_message),
                    }
                ),
            ]
        );
    }

    #[tokio::test]
    async fn fatal_exit_drains_queued_events_before_synthesized_closure() {
        let (codex_op_tx, mut codex_op_rx) = unbounded_channel::<Op>();
        let (codex_event_tx, codex_event_rx) = unbounded_channel::<Event>();
        let (fatal_exit_tx, fatal_exit_rx) = unbounded_channel::<String>();

        codex_event_tx
            .send(Event {
                id: "round-start".to_string(),
                msg: EventMsg::PotterRoundStarted {
                    current: 1,
                    total: 3,
                },
            })
            .expect("send PotterRoundStarted");
        codex_event_tx
            .send(Event {
                id: "turn-start".to_string(),
                msg: EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    model_context_window: None,
                }),
            })
            .expect("send TurnStarted");
        fatal_exit_tx
            .send("fatal exit".to_string())
            .expect("send fatal exit");
        drop(codex_event_tx);
        drop(fatal_exit_tx);

        let mut ui = ExecJsonRoundUi::new(Vec::new(), PathBuf::from("/tmp"));
        let exit_info = ui
            .render_round(codex_tui::RenderRoundParams {
                prompt: "Continue working according to the WORKFLOW_INSTRUCTIONS".to_string(),
                pad_before_first_cell: false,
                status_header_prefix: None,
                prompt_footer: codex_tui::PromptFooterContext::new(PathBuf::from("."), None),
                codex_op_tx,
                codex_event_rx,
                fatal_exit_rx,
            })
            .await
            .expect("render_round");

        let op = codex_op_rx.try_recv().expect("expected Op::UserInput");
        assert!(matches!(op, Op::UserInput { .. }));

        let ExitReason::Fatal(message) = &exit_info.exit_reason else {
            panic!("expected fatal exit, got: {:?}", exit_info.exit_reason);
        };
        assert_eq!(message, "fatal exit");

        let output = String::from_utf8(ui.into_output()).expect("utf8");
        let events = output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<crate::exec::ExecJsonlEvent>)
            .collect::<Result<Vec<_>, _>>()
            .expect("parse JSONL");

        assert_eq!(
            events,
            vec![
                crate::exec::ExecJsonlEvent::PotterRoundStarted(
                    crate::exec::PotterRoundStartedEvent {
                        current: 1,
                        total: 3
                    }
                ),
                crate::exec::ExecJsonlEvent::TurnStarted(crate::exec::TurnStartedEvent {}),
                crate::exec::ExecJsonlEvent::TurnFailed(crate::exec::TurnFailedEvent {
                    error: crate::exec::ThreadErrorEvent {
                        message: "fatal exit".to_string(),
                    },
                }),
                crate::exec::ExecJsonlEvent::PotterRoundCompleted(
                    crate::exec::PotterRoundCompletedEvent {
                        outcome: crate::exec::PotterRoundCompletedOutcome::Fatal,
                        message: Some("fatal exit".to_string()),
                    }
                ),
            ]
        );
    }
}
