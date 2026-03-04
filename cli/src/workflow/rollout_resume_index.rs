//! Build an in-memory resume index from `potter-rollout.jsonl`.
//!
//! Resume needs a structured view of the append-only rollout log. This module parses the log
//! into:
//! - the initial `ProjectStarted` info
//! - a list of completed rounds with their thread ids, rollout paths and outcomes
//! - an optional unfinished round at EOF (round started/configured but no finished marker)
//!
//! Parsing is strict and validates key invariants so corrupted logs fail fast.

use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::PotterRoundOutcome;

use crate::workflow::rollout::PotterRolloutLine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PotterRolloutResumeIndex {
    pub project_started: ProjectStartedIndex,
    pub completed_rounds: Vec<CompletedRoundIndex>,
    pub unfinished_round: Option<UnfinishedRoundIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectStartedIndex {
    pub user_message: Option<String>,
    pub user_prompt_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedRoundIndex {
    pub round_current: u32,
    pub round_total: u32,
    pub thread_id: ThreadId,
    pub rollout_path: PathBuf,
    pub project_succeeded: Option<ProjectSucceededIndex>,
    pub outcome: PotterRoundOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfinishedRoundIndex {
    pub round_current: u32,
    pub round_total: u32,
    pub thread_id: ThreadId,
    pub rollout_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSucceededIndex {
    pub rounds: u32,
    pub duration_secs: u64,
    pub user_prompt_file: PathBuf,
    pub git_commit_start: String,
    pub git_commit_end: String,
}

pub fn build_resume_index(lines: &[PotterRolloutLine]) -> anyhow::Result<PotterRolloutResumeIndex> {
    let mut project_started: Option<ProjectStartedIndex> = None;
    let mut completed_rounds: Vec<CompletedRoundIndex> = Vec::new();

    struct RoundBuilder {
        round_current: u32,
        round_total: u32,
        configured: Option<(ThreadId, PathBuf)>,
        project_succeeded: Option<ProjectSucceededIndex>,
    }

    let mut current: Option<RoundBuilder> = None;

    for line in lines {
        match line {
            PotterRolloutLine::ProjectStarted {
                user_message,
                user_prompt_file,
            } => {
                if project_started.is_some() || !completed_rounds.is_empty() || current.is_some() {
                    anyhow::bail!("potter-rollout: project_started must appear once at the top");
                }
                project_started = Some(ProjectStartedIndex {
                    user_message: user_message.clone(),
                    user_prompt_file: user_prompt_file.clone(),
                });
            }
            PotterRolloutLine::RoundStarted {
                current: round_current,
                total: round_total,
            } => {
                if project_started.is_none() {
                    anyhow::bail!("potter-rollout: missing project_started before first round");
                }
                if current.is_some() {
                    anyhow::bail!("potter-rollout: round_started before previous round_finished");
                }
                current = Some(RoundBuilder {
                    round_current: *round_current,
                    round_total: *round_total,
                    configured: None,
                    project_succeeded: None,
                });
            }
            PotterRolloutLine::RoundConfigured {
                thread_id,
                rollout_path,
                ..
            } => {
                let Some(builder) = current.as_mut() else {
                    anyhow::bail!("potter-rollout: round_configured before round_started");
                };
                if builder.configured.is_some() {
                    anyhow::bail!("potter-rollout: duplicate round_configured in a single round");
                }
                builder.configured = Some((*thread_id, rollout_path.clone()));
            }
            PotterRolloutLine::ProjectSucceeded {
                rounds,
                duration_secs,
                user_prompt_file,
                git_commit_start,
                git_commit_end,
            } => {
                let Some(builder) = current.as_mut() else {
                    anyhow::bail!("potter-rollout: project_succeeded outside a round");
                };
                if builder.project_succeeded.is_some() {
                    anyhow::bail!("potter-rollout: duplicate project_succeeded in a single round");
                }
                builder.project_succeeded = Some(ProjectSucceededIndex {
                    rounds: *rounds,
                    duration_secs: *duration_secs,
                    user_prompt_file: user_prompt_file.clone(),
                    git_commit_start: git_commit_start.clone(),
                    git_commit_end: git_commit_end.clone(),
                });
            }
            PotterRolloutLine::RoundFinished { outcome } => {
                let Some(builder) = current.take() else {
                    anyhow::bail!("potter-rollout: round_finished without round_started");
                };
                let Some((thread_id, rollout_path)) = builder.configured else {
                    anyhow::bail!("potter-rollout: round_finished without round_configured");
                };
                if builder.project_succeeded.is_some()
                    && !matches!(outcome, PotterRoundOutcome::Completed)
                {
                    anyhow::bail!(
                        "potter-rollout: project_succeeded recorded but round_finished outcome is {outcome:?}"
                    );
                }
                completed_rounds.push(CompletedRoundIndex {
                    round_current: builder.round_current,
                    round_total: builder.round_total,
                    thread_id,
                    rollout_path,
                    project_succeeded: builder.project_succeeded,
                    outcome: outcome.clone(),
                });
            }
        }
    }

    let unfinished_round = match current.take() {
        Some(builder) => {
            if builder.project_succeeded.is_some() {
                anyhow::bail!("potter-rollout: project_succeeded without round_finished at EOF");
            }
            let Some((thread_id, rollout_path)) = builder.configured else {
                anyhow::bail!("potter-rollout: missing round_configured at EOF");
            };
            Some(UnfinishedRoundIndex {
                round_current: builder.round_current,
                round_total: builder.round_total,
                thread_id,
                rollout_path,
            })
        }
        None => None,
    };

    if project_started.is_some() && completed_rounds.is_empty() && unfinished_round.is_none() {
        anyhow::bail!("potter-rollout: project_started present but no rounds found");
    }

    let Some(project_started) = project_started else {
        anyhow::bail!("potter-rollout: missing project_started before first round");
    };

    Ok(PotterRolloutResumeIndex {
        project_started,
        completed_rounds,
        unfinished_round,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    fn thread_id() -> ThreadId {
        ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000").expect("thread id")
    }

    #[test]
    fn build_resume_index_records_completed_round() {
        let user_prompt_file = PathBuf::from(".codexpotter/projects/2026/02/28/1/MAIN.md");
        let lines = vec![
            PotterRolloutLine::ProjectStarted {
                user_message: Some("hello".to_string()),
                user_prompt_file: user_prompt_file.clone(),
            },
            PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
            PotterRolloutLine::RoundConfigured {
                thread_id: thread_id(),
                rollout_path: PathBuf::from("rollout.jsonl"),
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
            PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
            },
        ];

        let index = build_resume_index(&lines).expect("build resume index");
        assert_eq!(
            index,
            PotterRolloutResumeIndex {
                project_started: ProjectStartedIndex {
                    user_message: Some("hello".to_string()),
                    user_prompt_file: user_prompt_file.clone(),
                },
                completed_rounds: vec![CompletedRoundIndex {
                    round_current: 1,
                    round_total: 10,
                    thread_id: thread_id(),
                    rollout_path: PathBuf::from("rollout.jsonl"),
                    project_succeeded: None,
                    outcome: PotterRoundOutcome::Completed,
                }],
                unfinished_round: None,
            }
        );
    }

    #[test]
    fn build_resume_index_attaches_project_succeeded_to_completed_round() {
        let user_prompt_file = PathBuf::from(".codexpotter/projects/2026/02/28/1/MAIN.md");
        let lines = vec![
            PotterRolloutLine::ProjectStarted {
                user_message: None,
                user_prompt_file: user_prompt_file.clone(),
            },
            PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
            PotterRolloutLine::RoundConfigured {
                thread_id: thread_id(),
                rollout_path: PathBuf::from("rollout.jsonl"),
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
            PotterRolloutLine::ProjectSucceeded {
                rounds: 3,
                duration_secs: 42,
                user_prompt_file: user_prompt_file.clone(),
                git_commit_start: "start".to_string(),
                git_commit_end: "end".to_string(),
            },
            PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
            },
        ];

        let index = build_resume_index(&lines).expect("build resume index");
        assert_eq!(
            index,
            PotterRolloutResumeIndex {
                project_started: ProjectStartedIndex {
                    user_message: None,
                    user_prompt_file: user_prompt_file.clone(),
                },
                completed_rounds: vec![CompletedRoundIndex {
                    round_current: 1,
                    round_total: 10,
                    thread_id: thread_id(),
                    rollout_path: PathBuf::from("rollout.jsonl"),
                    project_succeeded: Some(ProjectSucceededIndex {
                        rounds: 3,
                        duration_secs: 42,
                        user_prompt_file,
                        git_commit_start: "start".to_string(),
                        git_commit_end: "end".to_string(),
                    }),
                    outcome: PotterRoundOutcome::Completed,
                }],
                unfinished_round: None,
            }
        );
    }

    #[test]
    fn build_resume_index_reports_unfinished_round_at_eof() {
        let user_prompt_file = PathBuf::from(".codexpotter/projects/2026/02/28/1/MAIN.md");
        let lines = vec![
            PotterRolloutLine::ProjectStarted {
                user_message: Some("hello".to_string()),
                user_prompt_file: user_prompt_file.clone(),
            },
            PotterRolloutLine::RoundStarted {
                current: 2,
                total: 10,
            },
            PotterRolloutLine::RoundConfigured {
                thread_id: thread_id(),
                rollout_path: PathBuf::from("rollout.jsonl"),
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
        ];

        let index = build_resume_index(&lines).expect("build resume index");
        assert_eq!(
            index,
            PotterRolloutResumeIndex {
                project_started: ProjectStartedIndex {
                    user_message: Some("hello".to_string()),
                    user_prompt_file,
                },
                completed_rounds: Vec::new(),
                unfinished_round: Some(UnfinishedRoundIndex {
                    round_current: 2,
                    round_total: 10,
                    thread_id: thread_id(),
                    rollout_path: PathBuf::from("rollout.jsonl"),
                }),
            }
        );
    }

    #[test]
    fn build_resume_index_errors_when_project_succeeded_round_outcome_is_not_completed() {
        let lines = vec![
            PotterRolloutLine::ProjectStarted {
                user_message: None,
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/02/28/1/MAIN.md"),
            },
            PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
            PotterRolloutLine::RoundConfigured {
                thread_id: thread_id(),
                rollout_path: PathBuf::from("rollout.jsonl"),
                rollout_path_raw: None,
                rollout_base_dir: None,
            },
            PotterRolloutLine::ProjectSucceeded {
                rounds: 1,
                duration_secs: 1,
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/02/28/1/MAIN.md"),
                git_commit_start: "start".to_string(),
                git_commit_end: "end".to_string(),
            },
            PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::TaskFailed {
                    message: "nope".to_string(),
                },
            },
        ];

        let err = build_resume_index(&lines).unwrap_err();
        assert!(
            err.to_string().contains("project_succeeded recorded"),
            "unexpected error: {err:#}"
        );
        assert!(
            err.to_string().contains("TaskFailed"),
            "error should include outcome: {err:#}"
        );
    }

    #[test]
    fn build_resume_index_errors_when_round_finished_missing_round_configured() {
        let lines = vec![
            PotterRolloutLine::ProjectStarted {
                user_message: None,
                user_prompt_file: PathBuf::from(".codexpotter/projects/2026/02/28/1/MAIN.md"),
            },
            PotterRolloutLine::RoundStarted {
                current: 1,
                total: 10,
            },
            PotterRolloutLine::RoundFinished {
                outcome: PotterRoundOutcome::Completed,
            },
        ];

        let err = build_resume_index(&lines).unwrap_err();
        assert!(
            err.to_string()
                .contains("round_finished without round_configured"),
            "unexpected error: {err:#}"
        );
    }
}
