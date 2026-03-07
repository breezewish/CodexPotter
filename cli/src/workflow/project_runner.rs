//! Interactive project queue runner.
//!
//! This module runs one or more CodexPotter projects in a loop:
//! - Collect the next user prompt either from the UI composer or from queued prompts emitted by
//!   the UI during round execution (see [`crate::workflow::prompt_queue`]).
//! - Start a new server-side project via `project/start`.
//! - Render the project by delegating to [`crate::workflow::project_render_loop`].
//!
//! Exiting the UI triggers a best-effort `project/interrupt` so the server does not keep a
//! dangling running project.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use codex_protocol::protocol::Event;
use codex_tui::InterruptedProjectAction;

use crate::workflow::round_runner::UiFuture;

const PROGRESS_FILE_CHANGED_TURN_PROMPT_OVERRIDE: &str = "Progress file has been changed by user";

/// Runtime configuration for running one or more CodexPotter projects.
#[derive(Debug, Clone)]
pub struct ProjectQueueOptions {
    /// Round budget per project (passed to `project/start`).
    pub rounds: NonZeroUsize,
    /// Per-round prompt passed to the TUI renderer.
    pub turn_prompt: String,
}

/// Outcome of running the project queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectQueueExit {
    /// The queue was exhausted (or the user cancelled the composer prompt).
    Completed,
    /// The user requested exit while a project was running.
    UserRequestedExit {
        /// The project directory relative path (e.g. `.codexpotter/projects/.../N`).
        project_dir: PathBuf,
    },
    /// Fatal error exit requested.
    FatalExitRequested,
}

/// Run CodexPotter projects until the queue is exhausted.
pub async fn run_project_queue(
    ui: &mut codex_tui::CodexPotterTui,
    app_server: &mut crate::app_server::potter::PotterAppServerClient,
    workdir: PathBuf,
    options: ProjectQueueOptions,
) -> anyhow::Result<ProjectQueueExit> {
    run_project_queue_with_deps(ui, app_server, workdir, options, &SystemProjectClock).await
}

trait ProjectRunnerUi: crate::workflow::round_runner::PotterRoundUi {
    fn clear(&mut self) -> anyhow::Result<()>;

    fn prompt_user<'a>(
        &'a mut self,
        prompt_footer: codex_tui::PromptFooterContext,
    ) -> UiFuture<'a, Option<String>>;

    fn prompt_interrupted_project_action<'a>(
        &'a mut self,
        progress_file_rel: PathBuf,
    ) -> UiFuture<'a, Option<InterruptedProjectAction>>;

    fn insert_interrupted_project_summary_block(
        &mut self,
        summary: crate::app_server::potter::InterruptedProjectSummary,
    );

    fn pop_queued_user_prompt(&mut self) -> Option<String>;
}

impl ProjectRunnerUi for codex_tui::CodexPotterTui {
    fn clear(&mut self) -> anyhow::Result<()> {
        codex_tui::CodexPotterTui::clear(self)
    }

    fn prompt_user<'a>(
        &'a mut self,
        prompt_footer: codex_tui::PromptFooterContext,
    ) -> UiFuture<'a, Option<String>> {
        Box::pin(codex_tui::CodexPotterTui::prompt_user(self, prompt_footer))
    }

    fn prompt_interrupted_project_action<'a>(
        &'a mut self,
        progress_file_rel: PathBuf,
    ) -> UiFuture<'a, Option<InterruptedProjectAction>> {
        Box::pin(
            codex_tui::CodexPotterTui::prompt_interrupted_project_action(self, progress_file_rel),
        )
    }

    fn insert_interrupted_project_summary_block(
        &mut self,
        summary: crate::app_server::potter::InterruptedProjectSummary,
    ) {
        codex_tui::CodexPotterTui::insert_interrupted_project_summary(
            self,
            summary.rounds,
            summary.duration,
            summary.user_prompt_file,
            summary.git_commit_start,
            summary.git_commit_end,
        );
    }

    fn pop_queued_user_prompt(&mut self) -> Option<String> {
        codex_tui::CodexPotterTui::pop_queued_user_prompt(self)
    }
}

trait ProjectClock {
    fn now_instant(&self) -> Instant;
}

struct SystemProjectClock;

impl ProjectClock for SystemProjectClock {
    fn now_instant(&self) -> Instant {
        Instant::now()
    }
}

trait ProjectAppServer:
    crate::workflow::project_render_loop::PotterEventSource
    + crate::workflow::project_render_loop::PotterProjectController
{
    fn project_start<'a>(
        &'a mut self,
        params: crate::app_server::potter::ProjectStartParams,
    ) -> UiFuture<'a, (crate::app_server::potter::ProjectStartResponse, Vec<Event>)>;

    fn project_interrupt<'a>(&'a mut self, project_id: String) -> UiFuture<'a, ()>;

    fn project_resolve_interrupt<'a>(
        &'a mut self,
        params: crate::app_server::potter::ProjectResolveInterruptParams,
    ) -> UiFuture<
        'a,
        (
            crate::app_server::potter::ProjectResolveInterruptResponse,
            Vec<Event>,
        ),
    >;
}

impl ProjectAppServer for crate::app_server::potter::PotterAppServerClient {
    fn project_start<'a>(
        &'a mut self,
        params: crate::app_server::potter::ProjectStartParams,
    ) -> UiFuture<'a, (crate::app_server::potter::ProjectStartResponse, Vec<Event>)> {
        Box::pin(async move {
            let mut buffered_events = Vec::new();
            let response = self.project_start(params, &mut buffered_events).await?;
            Ok((response, buffered_events))
        })
    }

    fn project_interrupt<'a>(&'a mut self, project_id: String) -> UiFuture<'a, ()> {
        Box::pin(async move {
            let mut buffered_events = Vec::new();
            self.project_interrupt(
                crate::app_server::potter::ProjectInterruptParams { project_id },
                &mut buffered_events,
            )
            .await?;
            Ok(())
        })
    }

    fn project_resolve_interrupt<'a>(
        &'a mut self,
        params: crate::app_server::potter::ProjectResolveInterruptParams,
    ) -> UiFuture<
        'a,
        (
            crate::app_server::potter::ProjectResolveInterruptResponse,
            Vec<Event>,
        ),
    > {
        Box::pin(async move {
            let mut buffered_events = Vec::new();
            let response = self
                .project_resolve_interrupt(params, &mut buffered_events)
                .await?;
            Ok((response, buffered_events))
        })
    }
}

async fn run_project_queue_with_deps<U, S, C>(
    ui: &mut U,
    app_server: &mut S,
    workdir: PathBuf,
    options: ProjectQueueOptions,
    clock: &C,
) -> anyhow::Result<ProjectQueueExit>
where
    U: ProjectRunnerUi,
    S: ProjectAppServer,
    C: ProjectClock,
{
    let mut pending_user_prompts = crate::workflow::prompt_queue::PromptQueue::empty();
    let build_prompt_footer = || {
        codex_tui::PromptFooterContext::new(
            workdir.clone(),
            crate::workflow::project::resolve_git_branch(&workdir),
        )
    };

    'project: loop {
        let next_prompt = pending_user_prompts.pop_next_prompt(|| ui.pop_queued_user_prompt());

        let next_prompt =
            crate::workflow::prompt_queue::next_prompt_or_prompt_user(next_prompt, || {
                ui.prompt_user(build_prompt_footer())
            })
            .await?;

        let Some(next_prompt) = next_prompt else {
            break 'project;
        };

        let user_prompt = match next_prompt {
            crate::workflow::prompt_queue::NextPrompt::FromQueue(prompt) => prompt,
            crate::workflow::prompt_queue::NextPrompt::FromUser(prompt) => {
                // Clear prompt UI remnants before doing any work / streaming output.
                ui.clear()?;
                prompt
            }
        };

        let project_started_at = clock.now_instant();
        ui.set_project_started_at(project_started_at);

        let rounds_total_u32 = crate::rounds::round_budget_to_u32(options.rounds)?;
        let prompt_footer = build_prompt_footer();

        let (start_response, buffered_events) = app_server
            .project_start(crate::app_server::potter::ProjectStartParams {
                user_message: user_prompt.clone(),
                cwd: Some(workdir.clone()),
                rounds: Some(rounds_total_u32),
                event_mode: Some(crate::app_server::potter::PotterEventMode::Interactive),
            })
            .await
            .context("project/start via potter app-server")?;

        let project_dir = start_response
            .progress_file_rel
            .parent()
            .context("derive project dir from progress file path")?
            .to_path_buf();

        let project_id = start_response.project_id.clone();
        let mut buffered_events = buffered_events;
        loop {
            let exit = crate::workflow::project_render_loop::run_potter_project_render_loop(
                ui,
                app_server,
                &project_id,
                crate::workflow::project_render_loop::PotterProjectRenderOptions {
                    turn_prompt: options.turn_prompt.clone(),
                    prompt_footer: prompt_footer.clone(),
                    pad_before_first_cell: false,
                    initial_status_header_prefix: None,
                },
                buffered_events,
            )
            .await?;

            match exit {
                crate::workflow::project_render_loop::PotterProjectRenderExit::Completed { .. } => {
                    break;
                }
                crate::workflow::project_render_loop::PotterProjectRenderExit::Interrupted {
                    user_prompt_file,
                } => {
                    let action = ui
                        .prompt_interrupted_project_action(user_prompt_file)
                        .await?
                        .unwrap_or(InterruptedProjectAction::StopIterate);

                    match action {
                        InterruptedProjectAction::StopIterate => {
                            let (response, _buffered_events) = app_server
                                .project_resolve_interrupt(
                                    crate::app_server::potter::ProjectResolveInterruptParams {
                                        project_id: project_id.clone(),
                                        action: crate::app_server::potter::ResolveInterruptAction::Stop,
                                        turn_prompt_override: None,
                                    },
                                )
                                .await
                                .context("project/resolve_interrupt(stop) via potter app-server")?;

                            let summary = response
                                .summary
                                .context("internal error: missing summary for resolve_interrupt(stop)")?;
                            ui.insert_interrupted_project_summary_block(summary);
                            break;
                        }
                        InterruptedProjectAction::ContinueIterate => {
                            let (response, buffered) = app_server
                                .project_resolve_interrupt(
                                    crate::app_server::potter::ProjectResolveInterruptParams {
                                        project_id: project_id.clone(),
                                        action: crate::app_server::potter::ResolveInterruptAction::Continue,
                                        turn_prompt_override: Some(
                                            PROGRESS_FILE_CHANGED_TURN_PROMPT_OVERRIDE.to_string(),
                                        ),
                                    },
                                )
                                .await
                                .context("project/resolve_interrupt(continue) via potter app-server")?;
                            anyhow::ensure!(
                                response.summary.is_none(),
                                "internal error: resolve_interrupt(continue) returned summary"
                            );
                            buffered_events = buffered;
                        }
                    }
                }
                crate::workflow::project_render_loop::PotterProjectRenderExit::UserRequested => {
                    // Best-effort: stop the server-side project before exiting.
                    let _ = app_server.project_interrupt(project_id.clone()).await;
                    return Ok(ProjectQueueExit::UserRequestedExit { project_dir });
                }
                crate::workflow::project_render_loop::PotterProjectRenderExit::FatalExitRequested => {
                    let _ = app_server.project_interrupt(project_id.clone()).await;
                    return Ok(ProjectQueueExit::FatalExitRequested);
                }
            }
        }
    }

    Ok(ProjectQueueExit::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::PotterProjectOutcome;
    use codex_protocol::protocol::PotterRoundOutcome;
    use codex_protocol::protocol::TokenUsage;
    use pretty_assertions::assert_eq;
    use std::collections::VecDeque;

    #[derive(Debug, Default)]
    struct MockUi {
        queued_prompts: VecDeque<String>,
        prompt_user_responses: VecDeque<Option<String>>,
        prompt_interrupted_project_action_responses: VecDeque<Option<InterruptedProjectAction>>,
        prompt_user_calls: usize,
        prompt_interrupted_project_action_calls: usize,
        clear_calls: usize,
        project_started_at_calls: usize,
    }

    impl MockUi {
        fn new(queued_prompts: Vec<String>, prompt_user_responses: Vec<Option<String>>) -> Self {
            Self {
                queued_prompts: VecDeque::from(queued_prompts),
                prompt_user_responses: VecDeque::from(prompt_user_responses),
                prompt_interrupted_project_action_responses: VecDeque::new(),
                prompt_user_calls: 0,
                prompt_interrupted_project_action_calls: 0,
                clear_calls: 0,
                project_started_at_calls: 0,
            }
        }
    }

    impl crate::workflow::round_runner::PotterRoundUi for MockUi {
        fn set_project_started_at(&mut self, _started_at: Instant) {
            self.project_started_at_calls += 1;
        }

        fn render_round<'a>(
            &'a mut self,
            params: codex_tui::RenderRoundParams,
        ) -> crate::workflow::round_runner::UiFuture<'a, codex_tui::AppExitInfo> {
            Box::pin(async move {
                let codex_tui::RenderRoundParams {
                    mut codex_event_rx, ..
                } = params;

                while let Some(event) = codex_event_rx.recv().await {
                    if let EventMsg::PotterRoundFinished { outcome } = &event.msg {
                        return Ok(codex_tui::AppExitInfo {
                            token_usage: TokenUsage::default(),
                            thread_id: None,
                            exit_reason: match outcome {
                                PotterRoundOutcome::Completed => codex_tui::ExitReason::Completed,
                                PotterRoundOutcome::Interrupted => {
                                    codex_tui::ExitReason::Interrupted
                                }
                                PotterRoundOutcome::UserRequested => {
                                    codex_tui::ExitReason::UserRequested
                                }
                                PotterRoundOutcome::TaskFailed { message } => {
                                    codex_tui::ExitReason::TaskFailed(message.clone())
                                }
                                PotterRoundOutcome::Fatal { message } => {
                                    codex_tui::ExitReason::Fatal(message.clone())
                                }
                            },
                        });
                    }
                }

                Ok(codex_tui::AppExitInfo {
                    token_usage: TokenUsage::default(),
                    thread_id: None,
                    exit_reason: codex_tui::ExitReason::Fatal(
                        "event stream closed unexpectedly".to_string(),
                    ),
                })
            })
        }
    }

    impl ProjectRunnerUi for MockUi {
        fn clear(&mut self) -> anyhow::Result<()> {
            self.clear_calls += 1;
            Ok(())
        }

        fn prompt_user<'a>(
            &'a mut self,
            _prompt_footer: codex_tui::PromptFooterContext,
        ) -> UiFuture<'a, Option<String>> {
            self.prompt_user_calls += 1;
            let response = self
                .prompt_user_responses
                .pop_front()
                .expect("prompt_user response");
            Box::pin(async move { Ok(response) })
        }

        fn prompt_interrupted_project_action<'a>(
            &'a mut self,
            _progress_file_rel: PathBuf,
        ) -> UiFuture<'a, Option<InterruptedProjectAction>> {
            self.prompt_interrupted_project_action_calls += 1;
            let response = self
                .prompt_interrupted_project_action_responses
                .pop_front()
                .unwrap_or(Some(InterruptedProjectAction::StopIterate));
            Box::pin(async move { Ok(response) })
        }

        fn insert_interrupted_project_summary_block(
            &mut self,
            _summary: crate::app_server::potter::InterruptedProjectSummary,
        ) {
        }

        fn pop_queued_user_prompt(&mut self) -> Option<String> {
            self.queued_prompts.pop_front()
        }
    }

    struct TestClock;

    impl ProjectClock for TestClock {
        fn now_instant(&self) -> Instant {
            Instant::now()
        }
    }

    #[derive(Debug, Default)]
    struct InterruptContinueAppServer {
        started_prompts: std::sync::Mutex<Vec<String>>,
        resolve_interrupt_calls:
            std::sync::Mutex<Vec<crate::app_server::potter::ProjectResolveInterruptParams>>,
    }

    impl InterruptContinueAppServer {
        fn started_prompts(&self) -> Vec<String> {
            self.started_prompts.lock().expect("lock").clone()
        }

        fn resolve_interrupt_calls(
            &self,
        ) -> Vec<crate::app_server::potter::ProjectResolveInterruptParams> {
            self.resolve_interrupt_calls.lock().expect("lock").clone()
        }
    }

    impl crate::workflow::project_render_loop::PotterEventSource for InterruptContinueAppServer {
        fn read_next_event<'a>(&'a mut self) -> UiFuture<'a, Option<Event>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl crate::workflow::project_render_loop::PotterProjectController for InterruptContinueAppServer {
        fn interrupt_project<'a>(&'a mut self, _project_id: String) -> UiFuture<'a, Vec<Event>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    impl ProjectAppServer for InterruptContinueAppServer {
        fn project_start<'a>(
            &'a mut self,
            params: crate::app_server::potter::ProjectStartParams,
        ) -> UiFuture<'a, (crate::app_server::potter::ProjectStartResponse, Vec<Event>)> {
            Box::pin(async move {
                self.started_prompts
                    .lock()
                    .expect("lock")
                    .push(params.user_message.clone());

                let progress_file_rel = PathBuf::from(".codexpotter/projects/2026/02/01/1/MAIN.md");
                let response = crate::app_server::potter::ProjectStartResponse {
                    project_id: String::from("project_1"),
                    working_dir: PathBuf::from("/tmp"),
                    project_dir: PathBuf::from("/tmp/project_1"),
                    progress_file_rel: progress_file_rel.clone(),
                    progress_file: PathBuf::from("/tmp/project_1/MAIN.md"),
                    git_commit_start: String::new(),
                    git_branch: None,
                    rounds_total: 2,
                };

                let buffered_events = vec![
                    Event {
                        id: String::new(),
                        msg: EventMsg::PotterRoundStarted {
                            current: 1,
                            total: 2,
                        },
                    },
                    Event {
                        id: String::new(),
                        msg: EventMsg::PotterRoundFinished {
                            outcome: PotterRoundOutcome::Interrupted,
                        },
                    },
                    Event {
                        id: String::new(),
                        msg: EventMsg::PotterProjectInterrupted {
                            project_id: String::from("project_1"),
                            user_prompt_file: progress_file_rel,
                        },
                    },
                ];

                Ok((response, buffered_events))
            })
        }

        fn project_interrupt<'a>(&'a mut self, _project_id: String) -> UiFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }

        fn project_resolve_interrupt<'a>(
            &'a mut self,
            params: crate::app_server::potter::ProjectResolveInterruptParams,
        ) -> UiFuture<
            'a,
            (
                crate::app_server::potter::ProjectResolveInterruptResponse,
                Vec<Event>,
            ),
        > {
            self.resolve_interrupt_calls
                .lock()
                .expect("lock")
                .push(params);

            Box::pin(async {
                Ok((
                    crate::app_server::potter::ProjectResolveInterruptResponse { summary: None },
                    vec![
                        Event {
                            id: String::new(),
                            msg: EventMsg::PotterRoundStarted {
                                current: 1,
                                total: 2,
                            },
                        },
                        Event {
                            id: String::new(),
                            msg: EventMsg::PotterRoundFinished {
                                outcome: PotterRoundOutcome::Completed,
                            },
                        },
                        Event {
                            id: String::new(),
                            msg: EventMsg::PotterProjectCompleted {
                                outcome: PotterProjectOutcome::BudgetExhausted,
                            },
                        },
                    ],
                ))
            })
        }
    }

    #[derive(Debug, Default)]
    struct MockAppServer {
        started_prompts: std::sync::Mutex<Vec<String>>,
        next_project: std::sync::Mutex<u32>,
    }

    impl MockAppServer {
        fn started_prompts(&self) -> Vec<String> {
            self.started_prompts.lock().expect("lock").clone()
        }
    }

    impl crate::workflow::project_render_loop::PotterEventSource for MockAppServer {
        fn read_next_event<'a>(&'a mut self) -> UiFuture<'a, Option<Event>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl crate::workflow::project_render_loop::PotterProjectController for MockAppServer {
        fn interrupt_project<'a>(&'a mut self, _project_id: String) -> UiFuture<'a, Vec<Event>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    impl ProjectAppServer for MockAppServer {
        fn project_start<'a>(
            &'a mut self,
            params: crate::app_server::potter::ProjectStartParams,
        ) -> UiFuture<'a, (crate::app_server::potter::ProjectStartResponse, Vec<Event>)> {
            Box::pin(async move {
                self.started_prompts
                    .lock()
                    .expect("lock")
                    .push(params.user_message.clone());

                let idx = {
                    let mut guard = self.next_project.lock().expect("lock");
                    *guard = guard.saturating_add(1);
                    *guard
                };

                let progress_file_rel =
                    PathBuf::from(format!(".codexpotter/projects/2026/02/01/{idx}/MAIN.md"));
                let project_dir = PathBuf::from(format!("/tmp/project_{idx}"));
                let progress_file = PathBuf::from(format!("/tmp/project_{idx}/MAIN.md"));
                let project_id = format!("project_{idx}");

                let response = crate::app_server::potter::ProjectStartResponse {
                    project_id: project_id.clone(),
                    working_dir: PathBuf::from("/tmp"),
                    project_dir: project_dir.clone(),
                    progress_file_rel,
                    progress_file,
                    git_commit_start: String::new(),
                    git_branch: None,
                    rounds_total: 1,
                };

                let buffered_events = vec![
                    Event {
                        id: String::new(),
                        msg: EventMsg::PotterRoundStarted {
                            current: 1,
                            total: 1,
                        },
                    },
                    Event {
                        id: String::new(),
                        msg: EventMsg::PotterRoundFinished {
                            outcome: PotterRoundOutcome::Completed,
                        },
                    },
                    Event {
                        id: String::new(),
                        msg: EventMsg::PotterProjectCompleted {
                            outcome: PotterProjectOutcome::BudgetExhausted,
                        },
                    },
                ];

                Ok((response, buffered_events))
            })
        }

        fn project_interrupt<'a>(&'a mut self, _project_id: String) -> UiFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }

        fn project_resolve_interrupt<'a>(
            &'a mut self,
            _params: crate::app_server::potter::ProjectResolveInterruptParams,
        ) -> UiFuture<
            'a,
            (
                crate::app_server::potter::ProjectResolveInterruptResponse,
                Vec<Event>,
            ),
        > {
            Box::pin(async {
                Ok((
                    crate::app_server::potter::ProjectResolveInterruptResponse { summary: None },
                    Vec::new(),
                ))
            })
        }
    }

    #[tokio::test]
    async fn drains_queued_prompts_before_prompting_user() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut ui = MockUi::new(vec![String::from("one"), String::from("two")], vec![None]);
        let mut app_server = MockAppServer::default();
        let clock = TestClock;

        let exit = run_project_queue_with_deps(
            &mut ui,
            &mut app_server,
            temp.path().to_path_buf(),
            ProjectQueueOptions {
                rounds: NonZeroUsize::new(1).expect("rounds"),
                turn_prompt: String::from("Continue"),
            },
            &clock,
        )
        .await
        .expect("run project queue");

        assert_eq!(exit, ProjectQueueExit::Completed);
        assert_eq!(
            app_server.started_prompts(),
            vec![String::from("one"), String::from("two")]
        );
        assert_eq!(ui.prompt_user_calls, 1);
        assert_eq!(ui.clear_calls, 0);
        assert_eq!(ui.queued_prompts, VecDeque::<String>::new());
    }

    #[tokio::test]
    async fn prompts_user_when_queue_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut ui = MockUi::new(Vec::new(), vec![Some(String::from("hello")), None]);
        let mut app_server = MockAppServer::default();
        let clock = TestClock;

        let exit = run_project_queue_with_deps(
            &mut ui,
            &mut app_server,
            temp.path().to_path_buf(),
            ProjectQueueOptions {
                rounds: NonZeroUsize::new(1).expect("rounds"),
                turn_prompt: String::from("Continue"),
            },
            &clock,
        )
        .await
        .expect("run project queue");

        assert_eq!(exit, ProjectQueueExit::Completed);
        assert_eq!(app_server.started_prompts(), vec![String::from("hello")]);
        assert_eq!(ui.prompt_user_calls, 2);
        assert_eq!(ui.clear_calls, 1);
    }

    #[tokio::test]
    async fn prompts_user_after_draining_queue() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut ui = MockUi::new(vec![String::from("one")], vec![None]);
        let mut app_server = MockAppServer::default();
        let clock = TestClock;

        let exit = run_project_queue_with_deps(
            &mut ui,
            &mut app_server,
            temp.path().to_path_buf(),
            ProjectQueueOptions {
                rounds: NonZeroUsize::new(1).expect("rounds"),
                turn_prompt: String::from("Continue"),
            },
            &clock,
        )
        .await
        .expect("run project queue");

        assert_eq!(exit, ProjectQueueExit::Completed);
        assert_eq!(app_server.started_prompts(), vec![String::from("one")]);
        assert_eq!(ui.prompt_user_calls, 1);
        assert_eq!(ui.clear_calls, 0);
    }

    #[tokio::test]
    async fn continue_iterate_sends_progress_file_prompt_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut ui = MockUi::new(Vec::new(), vec![Some(String::from("hello")), None]);
        ui.prompt_interrupted_project_action_responses
            .push_back(Some(InterruptedProjectAction::ContinueIterate));

        let mut app_server = InterruptContinueAppServer::default();
        let clock = TestClock;

        let exit = run_project_queue_with_deps(
            &mut ui,
            &mut app_server,
            temp.path().to_path_buf(),
            ProjectQueueOptions {
                rounds: NonZeroUsize::new(2).expect("rounds"),
                turn_prompt: String::from("Continue"),
            },
            &clock,
        )
        .await
        .expect("run project queue");

        assert_eq!(exit, ProjectQueueExit::Completed);
        assert_eq!(app_server.started_prompts(), vec![String::from("hello")]);

        let resolve_calls = app_server.resolve_interrupt_calls();
        assert_eq!(resolve_calls.len(), 1, "expected resolve_interrupt call");
        assert_eq!(
            resolve_calls[0],
            crate::app_server::potter::ProjectResolveInterruptParams {
                project_id: String::from("project_1"),
                action: crate::app_server::potter::ResolveInterruptAction::Continue,
                turn_prompt_override: Some(PROGRESS_FILE_CHANGED_TURN_PROMPT_OVERRIDE.to_string(),),
            }
        );
    }
}
