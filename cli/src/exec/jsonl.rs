use std::collections::HashMap;
use std::path::PathBuf;

use codex_protocol::protocol::EventMsg;
use serde::Deserialize;
use serde::Serialize;

/// Exec-compatible JSONL events emitted by `codex-potter exec --json`.
///
/// This is intentionally a strict superset of upstream `codex exec --json` events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ExecJsonlEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted(ThreadStartedEvent),
    #[serde(rename = "turn.started")]
    TurnStarted(TurnStartedEvent),
    #[serde(rename = "turn.completed")]
    TurnCompleted(TurnCompletedEvent),
    #[serde(rename = "turn.failed")]
    TurnFailed(TurnFailedEvent),
    #[serde(rename = "item.started")]
    ItemStarted(ItemStartedEvent),
    #[serde(rename = "item.updated")]
    ItemUpdated(ItemUpdatedEvent),
    #[serde(rename = "item.completed")]
    ItemCompleted(ItemCompletedEvent),
    #[serde(rename = "error")]
    Error(ThreadErrorEvent),

    #[serde(rename = "potter.project.started")]
    PotterProjectStarted(PotterProjectStartedEvent),
    #[serde(rename = "potter.round.started")]
    PotterRoundStarted(PotterRoundStartedEvent),
    #[serde(rename = "potter.round.completed")]
    PotterRoundCompleted(PotterRoundCompletedEvent),
    #[serde(rename = "potter.project.succeeded")]
    PotterProjectSucceeded(PotterProjectSucceededEvent),
    #[serde(rename = "potter.project.completed")]
    PotterProjectCompleted(PotterProjectCompletedEvent),
    #[serde(rename = "potter.stream_recovery.update")]
    PotterStreamRecoveryUpdate(PotterStreamRecoveryUpdateEvent),
    #[serde(rename = "potter.stream_recovery.recovered")]
    PotterStreamRecoveryRecovered(PotterStreamRecoveryRecoveredEvent),
    #[serde(rename = "potter.stream_recovery.gave_up")]
    PotterStreamRecoveryGaveUp(PotterStreamRecoveryGaveUpEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThreadStartedEvent {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TurnStartedEvent {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnCompletedEvent {
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnFailedEvent {
    pub error: ThreadErrorEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItemStartedEvent {
    pub item: ThreadItem,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItemUpdatedEvent {
    pub item: ThreadItem,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItemCompletedEvent {
    pub item: ThreadItem,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThreadErrorEvent {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThreadItem {
    pub id: String,
    #[serde(flatten)]
    pub details: ThreadItemDetails,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadItemDetails {
    AgentMessage(AgentMessageItem),
    Reasoning(ReasoningItem),
    CommandExecution(CommandExecutionItem),
    FileChange(FileChangeItem),
    CollabToolCall(CollabToolCallItem),
    WebSearch(WebSearchItem),
    TodoList(TodoListItem),
    Error(ErrorItem),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentMessageItem {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningItem {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandExecutionItem {
    pub command: String,
    pub aggregated_output: String,
    pub exit_code: Option<i32>,
    pub status: CommandExecutionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PatchChangeKind {
    Add,
    Delete,
    Update,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PatchApplyStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileChangeItem {
    pub changes: Vec<FileUpdateChange>,
    pub status: PatchApplyStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollabTool {
    SpawnAgent,
    SendInput,
    Wait,
    CloseAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CollabToolCallStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollabAgentStatus {
    PendingInit,
    Running,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollabAgentState {
    pub status: CollabAgentStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollabToolCallItem {
    pub tool: CollabTool,
    pub sender_thread_id: String,
    pub receiver_thread_ids: Vec<String>,
    pub prompt: Option<String>,
    pub agents_states: HashMap<String, CollabAgentState>,
    pub status: CollabToolCallStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebSearchAction {
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queries: Option<Vec<String>>,
    },
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    FindInPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebSearchItem {
    pub query: String,
    pub action: WebSearchAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorItem {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub text: String,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TodoListItem {
    pub items: Vec<TodoItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PotterProjectStartedEvent {
    pub working_dir: String,
    pub project_dir: String,
    pub progress_file: String,
    pub user_message: String,
    pub git_commit_start: String,
    pub git_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PotterRoundStartedEvent {
    pub current: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PotterRoundCompletedOutcome {
    Completed,
    TaskFailed,
    Fatal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PotterRoundCompletedEvent {
    pub outcome: PotterRoundCompletedOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PotterProjectSucceededEvent {
    pub rounds: u32,
    pub duration_secs: u64,
    pub git_commit_start: String,
    pub git_commit_end: String,
    pub progress_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PotterProjectCompletedOutcome {
    Succeeded,
    BudgetExhausted,
    TaskFailed,
    Fatal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PotterProjectCompletedEvent {
    pub outcome: PotterProjectCompletedOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub rounds_run: u32,
    pub rounds_total: u32,
    pub duration_secs: u64,
    pub progress_file: String,
    pub git_commit_start: String,
    pub git_commit_end: String,
    pub git_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PotterStreamRecoveryUpdateEvent {
    pub attempt: u32,
    pub max_attempts: u32,
    pub error_message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PotterStreamRecoveryRecoveredEvent {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PotterStreamRecoveryGaveUpEvent {
    pub error_message: String,
    pub attempts: u32,
    pub max_attempts: u32,
}

#[derive(Debug, Clone)]
struct RunningCommand {
    command: String,
    item_id: String,
    aggregated_output: String,
}

#[derive(Debug, Clone)]
struct RunningTodoList {
    item_id: String,
    items: Vec<TodoItem>,
}

#[derive(Debug, Clone)]
struct RunningCollabToolCall {
    tool: CollabTool,
    item_id: String,
}

#[derive(Debug, Clone)]
struct CollabToolCallCompletion {
    tool: CollabTool,
    sender_thread_id: String,
    receiver_thread_ids: Vec<String>,
    prompt: Option<String>,
    agents_states: HashMap<String, CollabAgentState>,
    status: CollabToolCallStatus,
}

#[derive(Debug, Default)]
pub struct ExecJsonlEventProcessor {
    workdir: Option<PathBuf>,
    next_item_id: u64,
    running_commands: HashMap<String, RunningCommand>,
    running_patch_applies: HashMap<String, codex_protocol::protocol::PatchApplyBeginEvent>,
    running_todo_list: Option<RunningTodoList>,
    last_total_token_usage: Option<codex_protocol::protocol::TokenUsage>,
    last_critical_error: Option<ThreadErrorEvent>,
    running_collab_tool_calls: HashMap<String, RunningCollabToolCall>,
}

impl ExecJsonlEventProcessor {
    pub fn with_workdir(workdir: PathBuf) -> Self {
        Self {
            workdir: Some(workdir),
            ..Self::default()
        }
    }

    pub fn collect_event(&mut self, msg: &EventMsg) -> Vec<ExecJsonlEvent> {
        match msg {
            EventMsg::SessionConfigured(ev) => {
                vec![ExecJsonlEvent::ThreadStarted(ThreadStartedEvent {
                    thread_id: ev.session_id.to_string(),
                })]
            }
            EventMsg::TokenCount(ev) => {
                if let Some(info) = &ev.info {
                    self.last_total_token_usage = Some(info.total_token_usage.clone());
                }
                Vec::new()
            }
            EventMsg::TurnStarted(_) => {
                self.last_critical_error = None;
                vec![ExecJsonlEvent::TurnStarted(TurnStartedEvent {})]
            }
            EventMsg::TurnComplete(_) => self.handle_turn_complete(),
            EventMsg::TurnAborted(ev) => self.handle_turn_aborted(ev),
            EventMsg::AgentMessage(ev) => vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: self.next_item_id(),
                    details: ThreadItemDetails::AgentMessage(AgentMessageItem {
                        text: ev.message.clone(),
                    }),
                },
            })],
            EventMsg::AgentReasoning(ev) => {
                vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                    item: ThreadItem {
                        id: self.next_item_id(),
                        details: ThreadItemDetails::Reasoning(ReasoningItem {
                            text: ev.text.clone(),
                        }),
                    },
                })]
            }
            EventMsg::ExecCommandBegin(ev) => self.handle_exec_command_begin(ev),
            EventMsg::ExecCommandEnd(ev) => self.handle_exec_command_end(ev),
            EventMsg::PatchApplyBegin(ev) => {
                self.running_patch_applies
                    .insert(ev.call_id.clone(), ev.clone());
                Vec::new()
            }
            EventMsg::PatchApplyEnd(ev) => self.handle_patch_apply_end(ev),
            EventMsg::Warning(ev) => vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: self.next_item_id(),
                    details: ThreadItemDetails::Error(ErrorItem {
                        message: ev.message.clone(),
                    }),
                },
            })],
            EventMsg::Error(ev) => {
                let error = ThreadErrorEvent {
                    message: ev.message.clone(),
                };
                self.last_critical_error = Some(error.clone());
                vec![ExecJsonlEvent::Error(error)]
            }
            EventMsg::StreamError(ev) => {
                let message = match &ev.additional_details {
                    Some(details) if !details.trim().is_empty() => {
                        format!("{} ({details})", ev.message)
                    }
                    _ => ev.message.clone(),
                };
                vec![ExecJsonlEvent::Error(ThreadErrorEvent { message })]
            }
            EventMsg::PlanUpdate(args) => self.handle_plan_update(args),
            EventMsg::WebSearchEnd(ev) => vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: ev.call_id.clone(),
                    details: ThreadItemDetails::WebSearch(WebSearchItem {
                        query: ev.query.clone(),
                        action: WebSearchAction::Other,
                    }),
                },
            })],
            EventMsg::CollabAgentSpawnBegin(ev) => self.handle_collab_spawn_begin(ev),
            EventMsg::CollabAgentSpawnEnd(ev) => self.handle_collab_spawn_end(ev),
            EventMsg::CollabAgentInteractionBegin(ev) => self.handle_collab_interaction_begin(ev),
            EventMsg::CollabAgentInteractionEnd(ev) => self.handle_collab_interaction_end(ev),
            EventMsg::CollabWaitingBegin(ev) => self.handle_collab_wait_begin(ev),
            EventMsg::CollabWaitingEnd(ev) => self.handle_collab_wait_end(ev),
            EventMsg::CollabCloseBegin(ev) => self.handle_collab_close_begin(ev),
            EventMsg::CollabCloseEnd(ev) => self.handle_collab_close_end(ev),
            EventMsg::PotterRoundStarted { current, total } => {
                vec![ExecJsonlEvent::PotterRoundStarted(
                    PotterRoundStartedEvent {
                        current: *current,
                        total: *total,
                    },
                )]
            }
            EventMsg::PotterRoundFinished { outcome } => {
                vec![ExecJsonlEvent::PotterRoundCompleted(
                    potter_round_completed_from_outcome(outcome),
                )]
            }
            EventMsg::PotterProjectSucceeded {
                rounds,
                duration,
                user_prompt_file,
                git_commit_start,
                git_commit_end,
            } => vec![ExecJsonlEvent::PotterProjectSucceeded(
                PotterProjectSucceededEvent {
                    rounds: *rounds,
                    duration_secs: duration.as_secs(),
                    git_commit_start: git_commit_start.clone(),
                    git_commit_end: git_commit_end.clone(),
                    progress_file: self.resolve_path(user_prompt_file),
                },
            )],
            EventMsg::PotterStreamRecoveryUpdate {
                attempt,
                max_attempts,
                error_message,
            } => vec![ExecJsonlEvent::PotterStreamRecoveryUpdate(
                PotterStreamRecoveryUpdateEvent {
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                    error_message: error_message.clone(),
                },
            )],
            EventMsg::PotterStreamRecoveryRecovered => {
                vec![ExecJsonlEvent::PotterStreamRecoveryRecovered(
                    PotterStreamRecoveryRecoveredEvent {},
                )]
            }
            EventMsg::PotterStreamRecoveryGaveUp {
                error_message,
                attempts,
                max_attempts,
            } => vec![ExecJsonlEvent::PotterStreamRecoveryGaveUp(
                PotterStreamRecoveryGaveUpEvent {
                    error_message: error_message.clone(),
                    attempts: *attempts,
                    max_attempts: *max_attempts,
                },
            )],
            _ => Vec::new(),
        }
    }

    pub fn reset_round_state(&mut self) {
        self.running_commands.clear();
        self.running_patch_applies.clear();
        self.running_todo_list = None;
        self.last_total_token_usage = None;
        self.last_critical_error = None;
        self.running_collab_tool_calls.clear();
    }

    fn next_item_id(&mut self) -> String {
        let id = self.next_item_id;
        self.next_item_id = self.next_item_id.saturating_add(1);
        format!("item_{id}")
    }

    fn resolve_path(&self, path: &PathBuf) -> String {
        if path.is_absolute() {
            return path.to_string_lossy().to_string();
        }
        match &self.workdir {
            Some(workdir) => workdir.join(path).to_string_lossy().to_string(),
            None => path.to_string_lossy().to_string(),
        }
    }

    fn handle_turn_complete(&mut self) -> Vec<ExecJsonlEvent> {
        let usage = if let Some(u) = &self.last_total_token_usage {
            Usage {
                input_tokens: u.input_tokens,
                cached_input_tokens: u.cached_input_tokens,
                output_tokens: u.output_tokens,
            }
        } else {
            Usage::default()
        };

        let mut out = Vec::new();

        if let Some(running) = self.running_todo_list.take() {
            out.push(ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: running.item_id,
                    details: ThreadItemDetails::TodoList(TodoListItem {
                        items: running.items,
                    }),
                },
            }));
        }

        if !self.running_commands.is_empty() {
            for (_, running) in self.running_commands.drain() {
                out.push(ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                    item: ThreadItem {
                        id: running.item_id,
                        details: ThreadItemDetails::CommandExecution(CommandExecutionItem {
                            command: running.command,
                            aggregated_output: running.aggregated_output,
                            exit_code: None,
                            status: CommandExecutionStatus::Completed,
                        }),
                    },
                }));
            }
        }

        if let Some(error) = self.last_critical_error.take() {
            out.push(ExecJsonlEvent::TurnFailed(TurnFailedEvent { error }));
        } else {
            out.push(ExecJsonlEvent::TurnCompleted(TurnCompletedEvent { usage }));
        }

        out
    }

    fn handle_turn_aborted(
        &mut self,
        ev: &codex_protocol::protocol::TurnAbortedEvent,
    ) -> Vec<ExecJsonlEvent> {
        let mut out = Vec::new();

        if let Some(running) = self.running_todo_list.take() {
            out.push(ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: running.item_id,
                    details: ThreadItemDetails::TodoList(TodoListItem {
                        items: running.items,
                    }),
                },
            }));
        }

        if !self.running_commands.is_empty() {
            for (_, running) in self.running_commands.drain() {
                out.push(ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                    item: ThreadItem {
                        id: running.item_id,
                        details: ThreadItemDetails::CommandExecution(CommandExecutionItem {
                            command: running.command,
                            aggregated_output: running.aggregated_output,
                            exit_code: None,
                            status: CommandExecutionStatus::Completed,
                        }),
                    },
                }));
            }
        }

        let error = self
            .last_critical_error
            .take()
            .unwrap_or_else(|| ThreadErrorEvent {
                message: format!("turn aborted: {:?}", &ev.reason),
            });

        out.push(ExecJsonlEvent::TurnFailed(TurnFailedEvent { error }));

        // Keep upstream behavior: still emit turn.completed/failed only once. `turn.aborted` has
        // no dedicated JSON event in the exec protocol. Consumers can observe it via `turn.failed`
        // along with any top-level `error` events that occurred during the turn.
        out
    }

    fn handle_exec_command_begin(
        &mut self,
        ev: &codex_protocol::protocol::ExecCommandBeginEvent,
    ) -> Vec<ExecJsonlEvent> {
        let item_id = self.next_item_id();

        let command_string = shlex::try_join(ev.command.iter().map(String::as_str))
            .unwrap_or_else(|_| ev.command.join(" "));

        self.running_commands.insert(
            ev.call_id.clone(),
            RunningCommand {
                command: command_string.clone(),
                item_id: item_id.clone(),
                aggregated_output: String::new(),
            },
        );

        vec![ExecJsonlEvent::ItemStarted(ItemStartedEvent {
            item: ThreadItem {
                id: item_id,
                details: ThreadItemDetails::CommandExecution(CommandExecutionItem {
                    command: command_string,
                    aggregated_output: String::new(),
                    exit_code: None,
                    status: CommandExecutionStatus::InProgress,
                }),
            },
        })]
    }

    fn handle_exec_command_end(
        &mut self,
        ev: &codex_protocol::protocol::ExecCommandEndEvent,
    ) -> Vec<ExecJsonlEvent> {
        let Some(RunningCommand {
            command,
            item_id,
            aggregated_output,
        }) = self.running_commands.remove(&ev.call_id)
        else {
            return Vec::new();
        };

        let status = if ev.exit_code == 0 {
            CommandExecutionStatus::Completed
        } else {
            CommandExecutionStatus::Failed
        };
        let aggregated_output = if ev.aggregated_output.is_empty() {
            aggregated_output
        } else {
            ev.aggregated_output.clone()
        };

        vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: item_id,
                details: ThreadItemDetails::CommandExecution(CommandExecutionItem {
                    command,
                    aggregated_output,
                    exit_code: Some(ev.exit_code),
                    status,
                }),
            },
        })]
    }

    fn handle_patch_apply_end(
        &mut self,
        ev: &codex_protocol::protocol::PatchApplyEndEvent,
    ) -> Vec<ExecJsonlEvent> {
        let Some(begin) = self.running_patch_applies.remove(&ev.call_id) else {
            return Vec::new();
        };

        let status = if ev.success {
            PatchApplyStatus::Completed
        } else {
            PatchApplyStatus::Failed
        };

        let changes = begin
            .changes
            .iter()
            .map(|(path, change)| FileUpdateChange {
                path: path.to_string_lossy().to_string(),
                kind: match change {
                    codex_protocol::protocol::FileChange::Add { .. } => PatchChangeKind::Add,
                    codex_protocol::protocol::FileChange::Delete { .. } => PatchChangeKind::Delete,
                    codex_protocol::protocol::FileChange::Update { .. } => PatchChangeKind::Update,
                },
            })
            .collect();

        vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: self.next_item_id(),
                details: ThreadItemDetails::FileChange(FileChangeItem { changes, status }),
            },
        })]
    }

    fn todo_items_from_plan(
        &self,
        args: &codex_protocol::plan_tool::UpdatePlanArgs,
    ) -> Vec<TodoItem> {
        args.plan
            .iter()
            .map(|p| TodoItem {
                text: p.step.clone(),
                completed: matches!(p.status, codex_protocol::plan_tool::StepStatus::Completed),
            })
            .collect()
    }

    fn handle_plan_update(
        &mut self,
        args: &codex_protocol::plan_tool::UpdatePlanArgs,
    ) -> Vec<ExecJsonlEvent> {
        let items = self.todo_items_from_plan(args);

        if let Some(running) = &mut self.running_todo_list {
            running.items = items.clone();
            return vec![ExecJsonlEvent::ItemUpdated(ItemUpdatedEvent {
                item: ThreadItem {
                    id: running.item_id.clone(),
                    details: ThreadItemDetails::TodoList(TodoListItem { items }),
                },
            })];
        }

        let item_id = self.next_item_id();
        self.running_todo_list = Some(RunningTodoList {
            item_id: item_id.clone(),
            items: items.clone(),
        });

        vec![ExecJsonlEvent::ItemStarted(ItemStartedEvent {
            item: ThreadItem {
                id: item_id,
                details: ThreadItemDetails::TodoList(TodoListItem { items }),
            },
        })]
    }

    fn start_collab_tool_call(
        &mut self,
        call_id: &str,
        tool: CollabTool,
        sender_thread_id: codex_protocol::ThreadId,
        receiver_thread_ids: Vec<codex_protocol::ThreadId>,
        prompt: Option<String>,
    ) -> Vec<ExecJsonlEvent> {
        let item_id = self.next_item_id();
        self.running_collab_tool_calls.insert(
            call_id.to_string(),
            RunningCollabToolCall {
                tool: tool.clone(),
                item_id: item_id.clone(),
            },
        );

        vec![ExecJsonlEvent::ItemStarted(ItemStartedEvent {
            item: ThreadItem {
                id: item_id,
                details: ThreadItemDetails::CollabToolCall(CollabToolCallItem {
                    tool,
                    sender_thread_id: sender_thread_id.to_string(),
                    receiver_thread_ids: receiver_thread_ids
                        .into_iter()
                        .map(|id| id.to_string())
                        .collect(),
                    prompt,
                    agents_states: HashMap::new(),
                    status: CollabToolCallStatus::InProgress,
                }),
            },
        })]
    }

    fn complete_collab_tool_call(
        &mut self,
        call_id: &str,
        completion: CollabToolCallCompletion,
    ) -> Vec<ExecJsonlEvent> {
        let CollabToolCallCompletion {
            tool,
            sender_thread_id,
            receiver_thread_ids,
            prompt,
            agents_states,
            status,
        } = completion;

        let (tool, item_id) = self
            .running_collab_tool_calls
            .remove(call_id)
            .map(|running| (running.tool, running.item_id))
            .unwrap_or_else(|| (tool, self.next_item_id()));

        vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: item_id,
                details: ThreadItemDetails::CollabToolCall(CollabToolCallItem {
                    tool,
                    sender_thread_id,
                    receiver_thread_ids,
                    prompt,
                    agents_states,
                    status,
                }),
            },
        })]
    }

    fn handle_collab_spawn_begin(
        &mut self,
        ev: &codex_protocol::protocol::CollabAgentSpawnBeginEvent,
    ) -> Vec<ExecJsonlEvent> {
        self.start_collab_tool_call(
            &ev.call_id,
            CollabTool::SpawnAgent,
            ev.sender_thread_id,
            Vec::new(),
            Some(ev.prompt.clone()),
        )
    }

    fn handle_collab_spawn_end(
        &mut self,
        ev: &codex_protocol::protocol::CollabAgentSpawnEndEvent,
    ) -> Vec<ExecJsonlEvent> {
        let mut agents_states = HashMap::new();
        let receiver_thread_ids = match ev.new_thread_id {
            Some(thread_id) => {
                agents_states.insert(thread_id.to_string(), CollabAgentState::from(&ev.status));
                vec![thread_id.to_string()]
            }
            None => Vec::new(),
        };

        let status = if receiver_thread_ids.is_empty() || is_collab_failure(&ev.status) {
            CollabToolCallStatus::Failed
        } else {
            CollabToolCallStatus::Completed
        };

        self.complete_collab_tool_call(
            &ev.call_id,
            CollabToolCallCompletion {
                tool: CollabTool::SpawnAgent,
                sender_thread_id: ev.sender_thread_id.to_string(),
                receiver_thread_ids,
                prompt: Some(ev.prompt.clone()),
                agents_states,
                status,
            },
        )
    }

    fn handle_collab_interaction_begin(
        &mut self,
        ev: &codex_protocol::protocol::CollabAgentInteractionBeginEvent,
    ) -> Vec<ExecJsonlEvent> {
        self.start_collab_tool_call(
            &ev.call_id,
            CollabTool::SendInput,
            ev.sender_thread_id,
            vec![ev.receiver_thread_id],
            Some(ev.prompt.clone()),
        )
    }

    fn handle_collab_interaction_end(
        &mut self,
        ev: &codex_protocol::protocol::CollabAgentInteractionEndEvent,
    ) -> Vec<ExecJsonlEvent> {
        let mut agents_states = HashMap::new();
        agents_states.insert(
            ev.receiver_thread_id.to_string(),
            CollabAgentState::from(&ev.status),
        );

        let status = if is_collab_failure(&ev.status) {
            CollabToolCallStatus::Failed
        } else {
            CollabToolCallStatus::Completed
        };

        self.complete_collab_tool_call(
            &ev.call_id,
            CollabToolCallCompletion {
                tool: CollabTool::SendInput,
                sender_thread_id: ev.sender_thread_id.to_string(),
                receiver_thread_ids: vec![ev.receiver_thread_id.to_string()],
                prompt: Some(ev.prompt.clone()),
                agents_states,
                status,
            },
        )
    }

    fn handle_collab_wait_begin(
        &mut self,
        ev: &codex_protocol::protocol::CollabWaitingBeginEvent,
    ) -> Vec<ExecJsonlEvent> {
        self.start_collab_tool_call(
            &ev.call_id,
            CollabTool::Wait,
            ev.sender_thread_id,
            ev.receiver_thread_ids.clone(),
            None,
        )
    }

    fn handle_collab_wait_end(
        &mut self,
        ev: &codex_protocol::protocol::CollabWaitingEndEvent,
    ) -> Vec<ExecJsonlEvent> {
        let agents_states = ev
            .statuses
            .iter()
            .map(|(thread_id, status)| (thread_id.to_string(), CollabAgentState::from(status)))
            .collect::<HashMap<_, _>>();
        let any_failure = ev.statuses.values().any(is_collab_failure);
        let status = if any_failure {
            CollabToolCallStatus::Failed
        } else {
            CollabToolCallStatus::Completed
        };

        self.complete_collab_tool_call(
            &ev.call_id,
            CollabToolCallCompletion {
                tool: CollabTool::Wait,
                sender_thread_id: ev.sender_thread_id.to_string(),
                receiver_thread_ids: ev
                    .statuses
                    .keys()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>(),
                prompt: None,
                agents_states,
                status,
            },
        )
    }

    fn handle_collab_close_begin(
        &mut self,
        ev: &codex_protocol::protocol::CollabCloseBeginEvent,
    ) -> Vec<ExecJsonlEvent> {
        self.start_collab_tool_call(
            &ev.call_id,
            CollabTool::CloseAgent,
            ev.sender_thread_id,
            vec![ev.receiver_thread_id],
            None,
        )
    }

    fn handle_collab_close_end(
        &mut self,
        ev: &codex_protocol::protocol::CollabCloseEndEvent,
    ) -> Vec<ExecJsonlEvent> {
        let mut agents_states = HashMap::new();
        agents_states.insert(
            ev.receiver_thread_id.to_string(),
            CollabAgentState::from(&ev.status),
        );

        let status = if is_collab_failure(&ev.status) {
            CollabToolCallStatus::Failed
        } else {
            CollabToolCallStatus::Completed
        };

        self.complete_collab_tool_call(
            &ev.call_id,
            CollabToolCallCompletion {
                tool: CollabTool::CloseAgent,
                sender_thread_id: ev.sender_thread_id.to_string(),
                receiver_thread_ids: vec![ev.receiver_thread_id.to_string()],
                prompt: None,
                agents_states,
                status,
            },
        )
    }
}

fn is_collab_failure(status: &codex_protocol::protocol::AgentStatus) -> bool {
    matches!(
        status,
        codex_protocol::protocol::AgentStatus::Errored(_)
            | codex_protocol::protocol::AgentStatus::NotFound
    )
}

impl From<&codex_protocol::protocol::AgentStatus> for CollabAgentState {
    fn from(value: &codex_protocol::protocol::AgentStatus) -> Self {
        match value {
            codex_protocol::protocol::AgentStatus::PendingInit => Self {
                status: CollabAgentStatus::PendingInit,
                message: None,
            },
            codex_protocol::protocol::AgentStatus::Running => Self {
                status: CollabAgentStatus::Running,
                message: None,
            },
            codex_protocol::protocol::AgentStatus::Completed(message) => Self {
                status: CollabAgentStatus::Completed,
                message: message.clone(),
            },
            codex_protocol::protocol::AgentStatus::Errored(message) => Self {
                status: CollabAgentStatus::Errored,
                message: Some(message.clone()),
            },
            codex_protocol::protocol::AgentStatus::Shutdown => Self {
                status: CollabAgentStatus::Shutdown,
                message: None,
            },
            codex_protocol::protocol::AgentStatus::NotFound => Self {
                status: CollabAgentStatus::NotFound,
                message: None,
            },
        }
    }
}

fn potter_round_completed_from_outcome(
    outcome: &codex_protocol::protocol::PotterRoundOutcome,
) -> PotterRoundCompletedEvent {
    match outcome {
        codex_protocol::protocol::PotterRoundOutcome::Completed => PotterRoundCompletedEvent {
            outcome: PotterRoundCompletedOutcome::Completed,
            message: None,
        },
        codex_protocol::protocol::PotterRoundOutcome::TaskFailed { message } => {
            PotterRoundCompletedEvent {
                outcome: PotterRoundCompletedOutcome::TaskFailed,
                message: Some(message.clone()),
            }
        }
        codex_protocol::protocol::PotterRoundOutcome::Fatal { message } => {
            PotterRoundCompletedEvent {
                outcome: PotterRoundCompletedOutcome::Fatal,
                message: Some(message.clone()),
            }
        }
        codex_protocol::protocol::PotterRoundOutcome::UserRequested => PotterRoundCompletedEvent {
            outcome: PotterRoundCompletedOutcome::Fatal,
            message: Some(String::from("user requested")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ThreadId;
    use codex_protocol::plan_tool::PlanItemArg;
    use codex_protocol::plan_tool::StepStatus;
    use codex_protocol::plan_tool::UpdatePlanArgs;
    use codex_protocol::protocol::AgentMessageEvent;
    use codex_protocol::protocol::AgentReasoningEvent;
    use codex_protocol::protocol::CollabAgentInteractionBeginEvent;
    use codex_protocol::protocol::CollabAgentInteractionEndEvent;
    use codex_protocol::protocol::CollabCloseBeginEvent;
    use codex_protocol::protocol::CollabCloseEndEvent;
    use codex_protocol::protocol::CollabWaitingBeginEvent;
    use codex_protocol::protocol::CollabWaitingEndEvent;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ExecCommandBeginEvent;
    use codex_protocol::protocol::ExecCommandEndEvent;
    use codex_protocol::protocol::ExecCommandSource;
    use codex_protocol::protocol::PatchApplyBeginEvent;
    use codex_protocol::protocol::PatchApplyEndEvent;
    use codex_protocol::protocol::PotterRoundOutcome;
    use codex_protocol::protocol::SessionConfiguredEvent;
    use codex_protocol::protocol::TokenCountEvent;
    use codex_protocol::protocol::TokenUsage;
    use codex_protocol::protocol::TokenUsageInfo;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::protocol::TurnStartedEvent;
    use codex_protocol::protocol::WarningEvent;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    fn dummy_session_configured() -> EventMsg {
        EventMsg::SessionConfigured(SessionConfiguredEvent {
            session_id: ThreadId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap(),
            forked_from_id: None,
            model: "test-model".to_string(),
            model_provider_id: "test-provider".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            reasoning_effort: None,
            history_log_id: 0,
            history_entry_count: 0,
            initial_messages: None,
            rollout_path: PathBuf::from("rollout.jsonl"),
        })
    }

    #[test]
    fn session_configured_maps_to_thread_started() {
        let mut ep = ExecJsonlEventProcessor::default();
        let out = ep.collect_event(&dummy_session_configured());
        assert_eq!(
            out,
            vec![ExecJsonlEvent::ThreadStarted(ThreadStartedEvent {
                thread_id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
            })]
        );
    }

    #[test]
    fn turn_lifecycle_with_error_emits_error_then_turn_failed() {
        let mut ep = ExecJsonlEventProcessor::default();

        assert_eq!(
            ep.collect_event(&EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-1".to_string(),
                model_context_window: None,
            })),
            vec![ExecJsonlEvent::TurnStarted(super::TurnStartedEvent {})]
        );

        assert_eq!(
            ep.collect_event(&EventMsg::Error(ErrorEvent {
                message: "boom".to_string(),
                codex_error_info: None,
            })),
            vec![ExecJsonlEvent::Error(ThreadErrorEvent {
                message: "boom".to_string()
            })]
        );

        let out = ep.collect_event(&EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
        }));
        assert_eq!(
            out,
            vec![ExecJsonlEvent::TurnFailed(TurnFailedEvent {
                error: ThreadErrorEvent {
                    message: "boom".to_string(),
                }
            })]
        );
    }

    #[test]
    fn agent_message_and_reasoning_emit_completed_items() {
        let mut ep = ExecJsonlEventProcessor::default();

        let out = ep.collect_event(&EventMsg::AgentMessage(AgentMessageEvent {
            message: "hello".to_string(),
        }));
        assert_eq!(
            out,
            vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::AgentMessage(AgentMessageItem {
                        text: "hello".to_string(),
                    }),
                }
            })]
        );

        let out = ep.collect_event(&EventMsg::AgentReasoning(AgentReasoningEvent {
            text: "thinking".to_string(),
        }));
        assert_eq!(
            out,
            vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: "item_1".to_string(),
                    details: ThreadItemDetails::Reasoning(ReasoningItem {
                        text: "thinking".to_string(),
                    }),
                }
            })]
        );
    }

    #[test]
    fn plan_update_emits_started_updated_and_completed_on_turn_complete() {
        let mut ep = ExecJsonlEventProcessor::default();

        let first = ep.collect_event(&EventMsg::PlanUpdate(UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItemArg {
                    step: "A".to_string(),
                    status: StepStatus::Pending,
                },
                PlanItemArg {
                    step: "B".to_string(),
                    status: StepStatus::Completed,
                },
            ],
        }));
        assert_eq!(
            first,
            vec![ExecJsonlEvent::ItemStarted(ItemStartedEvent {
                item: ThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::TodoList(TodoListItem {
                        items: vec![
                            TodoItem {
                                text: "A".to_string(),
                                completed: false
                            },
                            TodoItem {
                                text: "B".to_string(),
                                completed: true
                            },
                        ],
                    }),
                }
            })]
        );

        let updated = ep.collect_event(&EventMsg::PlanUpdate(UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg {
                step: "A".to_string(),
                status: StepStatus::Completed,
            }],
        }));
        assert_eq!(
            updated,
            vec![ExecJsonlEvent::ItemUpdated(ItemUpdatedEvent {
                item: ThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::TodoList(TodoListItem {
                        items: vec![TodoItem {
                            text: "A".to_string(),
                            completed: true,
                        }],
                    }),
                }
            })]
        );

        let completed = ep.collect_event(&EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
        }));
        assert_eq!(completed.len(), 2, "todo_list + turn.completed");
        assert!(matches!(
            &completed[0],
            ExecJsonlEvent::ItemCompleted(ItemCompletedEvent { .. })
        ));
        assert_eq!(
            completed[1],
            ExecJsonlEvent::TurnCompleted(TurnCompletedEvent {
                usage: Usage::default()
            })
        );
    }

    #[test]
    fn exec_command_begin_end_emits_started_and_completed() {
        let mut ep = ExecJsonlEventProcessor::default();

        let started = ep.collect_event(&EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
            call_id: "call-1".to_string(),
            process_id: None,
            turn_id: "turn-1".to_string(),
            command: vec!["echo".to_string(), "hi".to_string()],
            cwd: PathBuf::from("/tmp"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
        }));
        assert_eq!(started.len(), 1);
        let ExecJsonlEvent::ItemStarted(ItemStartedEvent { item }) = &started[0] else {
            panic!("expected item.started");
        };
        assert_eq!(item.id, "item_0");

        let completed = ep.collect_event(&EventMsg::ExecCommandEnd(ExecCommandEndEvent {
            call_id: "call-1".to_string(),
            process_id: None,
            turn_id: "turn-1".to_string(),
            command: vec!["echo".to_string(), "hi".to_string()],
            cwd: PathBuf::from("/tmp"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: "hi\n".to_string(),
            stderr: String::new(),
            aggregated_output: "hi\n".to_string(),
            exit_code: 0,
            duration: Duration::from_millis(1),
            formatted_output: "hi".to_string(),
        }));
        assert_eq!(
            completed,
            vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::CommandExecution(CommandExecutionItem {
                        command: "echo hi".to_string(),
                        aggregated_output: "hi\n".to_string(),
                        exit_code: Some(0),
                        status: CommandExecutionStatus::Completed,
                    }),
                }
            })]
        );
    }

    #[test]
    fn patch_apply_end_emits_file_change_item() {
        let mut ep = ExecJsonlEventProcessor::default();

        let mut changes = HashMap::new();
        changes.insert(
            PathBuf::from("hello.txt"),
            codex_protocol::protocol::FileChange::Add {
                content: "hi".to_string(),
            },
        );

        assert_eq!(
            ep.collect_event(&EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "patch-1".to_string(),
                turn_id: "turn-1".to_string(),
                auto_approved: true,
                changes: changes.clone(),
            })),
            Vec::<ExecJsonlEvent>::new()
        );

        let out = ep.collect_event(&EventMsg::PatchApplyEnd(PatchApplyEndEvent {
            call_id: "patch-1".to_string(),
            turn_id: "turn-1".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            success: true,
            changes,
        }));
        assert_eq!(
            out,
            vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::FileChange(FileChangeItem {
                        changes: vec![FileUpdateChange {
                            path: "hello.txt".to_string(),
                            kind: PatchChangeKind::Add,
                        }],
                        status: PatchApplyStatus::Completed,
                    }),
                }
            })]
        );
    }

    #[test]
    fn potter_round_markers_map_to_exec_events() {
        let mut ep = ExecJsonlEventProcessor::default();

        assert_eq!(
            ep.collect_event(&EventMsg::PotterRoundStarted {
                current: 1,
                total: 10,
            }),
            vec![ExecJsonlEvent::PotterRoundStarted(
                PotterRoundStartedEvent {
                    current: 1,
                    total: 10,
                }
            )]
        );

        assert_eq!(
            ep.collect_event(&EventMsg::PotterRoundFinished {
                outcome: PotterRoundOutcome::TaskFailed {
                    message: "nope".to_string()
                },
            }),
            vec![ExecJsonlEvent::PotterRoundCompleted(
                PotterRoundCompletedEvent {
                    outcome: PotterRoundCompletedOutcome::TaskFailed,
                    message: Some("nope".to_string()),
                }
            )]
        );
    }

    #[test]
    fn token_count_total_usage_is_used_in_turn_completed() {
        let mut ep = ExecJsonlEventProcessor::default();

        assert_eq!(
            ep.collect_event(&EventMsg::TokenCount(TokenCountEvent {
                info: Some(TokenUsageInfo {
                    total_token_usage: TokenUsage {
                        input_tokens: 10,
                        cached_input_tokens: 2,
                        output_tokens: 3,
                        reasoning_output_tokens: 0,
                        total_tokens: 0,
                    },
                    last_token_usage: TokenUsage {
                        input_tokens: 0,
                        cached_input_tokens: 0,
                        output_tokens: 0,
                        reasoning_output_tokens: 0,
                        total_tokens: 0,
                    },
                    model_context_window: None,
                }),
                rate_limits: None,
            })),
            Vec::<ExecJsonlEvent>::new()
        );

        let out = ep.collect_event(&EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
        }));
        assert_eq!(
            out,
            vec![ExecJsonlEvent::TurnCompleted(TurnCompletedEvent {
                usage: Usage {
                    input_tokens: 10,
                    cached_input_tokens: 2,
                    output_tokens: 3,
                }
            })]
        );
    }

    #[test]
    fn warning_is_mapped_to_error_item() {
        let mut ep = ExecJsonlEventProcessor::default();
        let out = ep.collect_event(&EventMsg::Warning(WarningEvent {
            message: "warn".to_string(),
        }));
        assert_eq!(
            out,
            vec![ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::Error(ErrorItem {
                        message: "warn".to_string(),
                    }),
                }
            })]
        );
    }

    #[test]
    fn collab_interaction_maps_to_item_started_and_completed() {
        let mut ep = ExecJsonlEventProcessor::default();

        let sender = ThreadId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
        let receiver = ThreadId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c9").unwrap();

        let started = ep.collect_event(&EventMsg::CollabAgentInteractionBegin(
            CollabAgentInteractionBeginEvent {
                call_id: "call-1".to_string(),
                sender_thread_id: sender,
                receiver_thread_id: receiver,
                prompt: "hi".to_string(),
            },
        ));
        assert_eq!(started.len(), 1);

        let completed = ep.collect_event(&EventMsg::CollabAgentInteractionEnd(
            CollabAgentInteractionEndEvent {
                call_id: "call-1".to_string(),
                sender_thread_id: sender,
                receiver_thread_id: receiver,
                receiver_agent_nickname: None,
                receiver_agent_role: None,
                prompt: "hi".to_string(),
                status: codex_protocol::protocol::AgentStatus::Running,
            },
        ));
        assert_eq!(completed.len(), 1);
        assert!(matches!(
            &completed[0],
            ExecJsonlEvent::ItemCompleted(ItemCompletedEvent { .. })
        ));
    }

    #[test]
    fn collab_wait_and_close_do_not_panic() {
        let mut ep = ExecJsonlEventProcessor::default();
        let sender = ThreadId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
        let receiver = ThreadId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c9").unwrap();

        let _ = ep.collect_event(&EventMsg::CollabWaitingBegin(CollabWaitingBeginEvent {
            sender_thread_id: sender,
            receiver_thread_ids: vec![receiver],
            receiver_agents: Vec::new(),
            call_id: "wait-1".to_string(),
        }));
        let _ = ep.collect_event(&EventMsg::CollabWaitingEnd(CollabWaitingEndEvent {
            sender_thread_id: sender,
            call_id: "wait-1".to_string(),
            agent_statuses: Vec::new(),
            statuses: HashMap::from([(receiver, codex_protocol::protocol::AgentStatus::Running)]),
        }));

        let _ = ep.collect_event(&EventMsg::CollabCloseBegin(CollabCloseBeginEvent {
            call_id: "close-1".to_string(),
            sender_thread_id: sender,
            receiver_thread_id: receiver,
        }));
        let _ = ep.collect_event(&EventMsg::CollabCloseEnd(CollabCloseEndEvent {
            call_id: "close-1".to_string(),
            sender_thread_id: sender,
            receiver_thread_id: receiver,
            receiver_agent_nickname: None,
            receiver_agent_role: None,
            status: codex_protocol::protocol::AgentStatus::Shutdown,
        }));
    }

    #[test]
    fn web_search_other_action_serializes_in_jsonl_schema() {
        let event = ExecJsonlEvent::ItemCompleted(ItemCompletedEvent {
            item: ThreadItem {
                id: "ws-1".to_string(),
                details: ThreadItemDetails::WebSearch(WebSearchItem {
                    query: "query".to_string(),
                    action: WebSearchAction::Other,
                }),
            },
        });

        let json = serde_json::to_string(&event).expect("serialize web_search event");
        let parsed =
            serde_json::from_str::<ExecJsonlEvent>(&json).expect("deserialize web_search event");
        assert_eq!(parsed, event);
    }
}
