use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::PotterRoundOutcome;
use codex_protocol::protocol::SessionConfiguredEvent;

#[derive(Debug, Clone)]
pub struct PotterRoundEventBridgeConfig {
    pub record_round_configured: bool,

    pub workdir: PathBuf,
    pub progress_file_rel: PathBuf,
    pub user_prompt_file: PathBuf,
    pub git_commit_start: String,
    pub potter_rollout_path: PathBuf,
    pub project_started_at: Instant,
    pub project_succeeded_rounds: u32,
}

#[derive(Debug, Clone)]
pub struct PotterRoundEventBridge {
    workdir: PathBuf,
    progress_file_rel: PathBuf,
    user_prompt_file: PathBuf,
    git_commit_start: String,
    potter_rollout_path: PathBuf,
    project_started_at: Instant,
    project_succeeded_rounds: u32,
    has_recorded_round_configured: bool,
}

impl PotterRoundEventBridge {
    pub fn new(config: PotterRoundEventBridgeConfig) -> Self {
        Self {
            has_recorded_round_configured: !config.record_round_configured,
            workdir: config.workdir,
            progress_file_rel: config.progress_file_rel,
            user_prompt_file: config.user_prompt_file,
            git_commit_start: config.git_commit_start,
            potter_rollout_path: config.potter_rollout_path,
            project_started_at: config.project_started_at,
            project_succeeded_rounds: config.project_succeeded_rounds,
        }
    }

    pub fn observe_backend_event(&mut self, event: &Event) -> anyhow::Result<Option<Event>> {
        if !self.has_recorded_round_configured
            && let EventMsg::SessionConfigured(cfg) = &event.msg
        {
            self.has_recorded_round_configured = true;
            self.record_round_configured(cfg)
                .context("record potter-rollout round_configured")?;
        }

        let mut injected: Option<Event> = None;
        if matches!(
            &event.msg,
            EventMsg::PotterRoundFinished {
                outcome: PotterRoundOutcome::Completed
            }
        ) && crate::workflow::project::progress_file_has_finite_incantatem_true(
            &self.workdir,
            &self.progress_file_rel,
        )
        .unwrap_or(false)
        {
            let git_commit_end = crate::workflow::project::resolve_git_commit(&self.workdir);
            crate::workflow::rollout::append_line(
                &self.potter_rollout_path,
                &crate::workflow::rollout::PotterRolloutLine::ProjectSucceeded {
                    rounds: self.project_succeeded_rounds,
                    duration_secs: self.project_started_at.elapsed().as_secs(),
                    user_prompt_file: self.user_prompt_file.clone(),
                    git_commit_start: self.git_commit_start.clone(),
                    git_commit_end: git_commit_end.clone(),
                },
            )
            .context("append potter-rollout project_succeeded")?;

            injected = Some(Event {
                id: "".to_string(),
                msg: EventMsg::PotterProjectSucceeded {
                    rounds: self.project_succeeded_rounds,
                    duration: self.project_started_at.elapsed(),
                    user_prompt_file: self.user_prompt_file.clone(),
                    git_commit_start: self.git_commit_start.clone(),
                    git_commit_end,
                },
            });
        }

        if let EventMsg::PotterRoundFinished { outcome } = &event.msg {
            crate::workflow::rollout::append_line(
                &self.potter_rollout_path,
                &crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                    outcome: outcome.clone(),
                },
            )
            .context("append potter-rollout round_finished")?;
        }

        Ok(injected)
    }

    fn record_round_configured(&self, cfg: &SessionConfiguredEvent) -> anyhow::Result<()> {
        let (rollout_path, rollout_path_raw, rollout_base_dir) =
            crate::workflow::rollout::resolve_rollout_path_for_recording(
                cfg.rollout_path.clone(),
                &self.workdir,
            );
        crate::workflow::rollout::append_line(
            &self.potter_rollout_path,
            &crate::workflow::rollout::PotterRolloutLine::RoundConfigured {
                thread_id: cfg.session_id,
                rollout_path,
                rollout_path_raw,
                rollout_base_dir,
            },
        )
        .context("append potter-rollout round_configured")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use std::path::Path;

    fn write_progress_file(workdir: &Path, progress_file_rel: &Path, finite: bool) {
        let progress_file = workdir.join(progress_file_rel);
        std::fs::create_dir_all(progress_file.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &progress_file,
            format!(
                r#"---
finite_incantatem: {finite}
---

# Overall Goal
"#
            ),
        )
        .expect("write progress file");
    }

    fn session_configured_event(workdir: &Path, rollout_path: PathBuf) -> Event {
        Event {
            id: "event_1".to_string(),
            msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                session_id: codex_protocol::ThreadId::from_string(
                    "019ca423-63d9-7641-ae83-db060ad3c000",
                )
                .expect("thread id"),
                forked_from_id: None,
                model: "test".to_string(),
                model_provider_id: "test".to_string(),
                cwd: workdir.to_path_buf(),
                reasoning_effort: None,
                history_log_id: 0,
                history_entry_count: 0,
                initial_messages: None,
                rollout_path,
            }),
        }
    }

    #[test]
    fn observe_backend_event_records_round_configured_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workdir = dir.path();
        let potter_rollout_path = workdir.join("potter-rollout.jsonl");

        let rollout_path = workdir.join("upstream.jsonl");
        std::fs::write(&rollout_path, "").expect("write upstream rollout");

        let mut bridge = PotterRoundEventBridge::new(PotterRoundEventBridgeConfig {
            record_round_configured: true,
            workdir: workdir.to_path_buf(),
            progress_file_rel: PathBuf::from(".codexpotter/projects/2026/03/04/1/MAIN.md"),
            user_prompt_file: PathBuf::from(".codexpotter/projects/2026/03/04/1/MAIN.md"),
            git_commit_start: "start".to_string(),
            potter_rollout_path: potter_rollout_path.clone(),
            project_started_at: Instant::now(),
            project_succeeded_rounds: 1,
        });

        let ev = session_configured_event(workdir, PathBuf::from("upstream.jsonl"));
        bridge.observe_backend_event(&ev).expect("observe #1");
        bridge.observe_backend_event(&ev).expect("observe #2");

        let lines = crate::workflow::rollout::read_lines(&potter_rollout_path).expect("read");
        assert_eq!(lines.len(), 1);
        assert!(matches!(
            &lines[0],
            crate::workflow::rollout::PotterRolloutLine::RoundConfigured { .. }
        ));
    }

    #[test]
    fn observe_backend_event_injects_project_succeeded_before_round_finished_when_finite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workdir = dir.path();
        let progress_file_rel = PathBuf::from(".codexpotter/projects/2026/03/04/1/MAIN.md");
        write_progress_file(workdir, &progress_file_rel, true);

        let potter_rollout_path = workdir.join("potter-rollout.jsonl");
        let mut bridge = PotterRoundEventBridge::new(PotterRoundEventBridgeConfig {
            record_round_configured: false,
            workdir: workdir.to_path_buf(),
            progress_file_rel: progress_file_rel.clone(),
            user_prompt_file: progress_file_rel.clone(),
            git_commit_start: "start".to_string(),
            potter_rollout_path: potter_rollout_path.clone(),
            project_started_at: Instant::now(),
            project_succeeded_rounds: 3,
        });

        let finished = Event {
            id: "event_2".to_string(),
            msg: EventMsg::PotterRoundFinished {
                outcome: PotterRoundOutcome::Completed,
            },
        };

        let injected = bridge
            .observe_backend_event(&finished)
            .expect("observe finished");
        assert!(matches!(
            injected.as_ref().map(|event| &event.msg),
            Some(EventMsg::PotterProjectSucceeded { rounds: 3, .. })
        ));

        let lines = crate::workflow::rollout::read_lines(&potter_rollout_path).expect("read");
        assert_eq!(lines.len(), 2);
        assert!(matches!(
            &lines[0],
            crate::workflow::rollout::PotterRolloutLine::ProjectSucceeded { rounds: 3, .. }
        ));
        assert!(matches!(
            &lines[1],
            crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed
            }
        ));
    }

    #[test]
    fn observe_backend_event_does_not_inject_project_succeeded_when_not_finite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workdir = dir.path();
        let progress_file_rel = PathBuf::from(".codexpotter/projects/2026/03/04/1/MAIN.md");
        write_progress_file(workdir, &progress_file_rel, false);

        let potter_rollout_path = workdir.join("potter-rollout.jsonl");
        let mut bridge = PotterRoundEventBridge::new(PotterRoundEventBridgeConfig {
            record_round_configured: false,
            workdir: workdir.to_path_buf(),
            progress_file_rel: progress_file_rel.clone(),
            user_prompt_file: progress_file_rel.clone(),
            git_commit_start: "start".to_string(),
            potter_rollout_path: potter_rollout_path.clone(),
            project_started_at: Instant::now(),
            project_succeeded_rounds: 3,
        });

        let finished = Event {
            id: "event_2".to_string(),
            msg: EventMsg::PotterRoundFinished {
                outcome: PotterRoundOutcome::Completed,
            },
        };

        let injected = bridge
            .observe_backend_event(&finished)
            .expect("observe finished");
        assert!(injected.is_none());

        let lines = crate::workflow::rollout::read_lines(&potter_rollout_path).expect("read");
        assert_eq!(lines.len(), 1);
        assert!(matches!(
            &lines[0],
            crate::workflow::rollout::PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed
            }
        ));
    }
}
