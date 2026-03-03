use std::io::Read as _;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use chrono::Local;
use codex_tui::ExitReason;

pub async fn run_exec_json(
    workdir: &Path,
    prompt: Option<String>,
    rounds: NonZeroUsize,
    codex_bin: String,
    backend_launch: crate::app_server_backend::AppServerLaunchConfig,
    codex_compat_home: Option<PathBuf>,
) -> i32 {
    let prompt = match prompt {
        Some(prompt) => prompt,
        None => match read_prompt_from_stdin() {
            Ok(prompt) => prompt,
            Err(err) => {
                let _ = write_preflight_error(&format!("{err:#}"));
                return 1;
            }
        },
    };

    if prompt.trim().is_empty() {
        let _ = write_preflight_error("prompt is empty");
        return 1;
    }

    let now = Local::now();
    let init = match crate::project::init_project(workdir, &prompt, now) {
        Ok(init) => init,
        Err(err) => {
            let _ = write_preflight_error(&format!("{err:#}"));
            return 1;
        }
    };

    let project_started_at = Instant::now();
    let progress_file_abs = workdir.join(&init.progress_file_rel);
    let project_dir_rel = match init.progress_file_rel.parent() {
        Some(dir) => dir.to_path_buf(),
        None => {
            let _ = write_preflight_error("failed to derive project_dir from progress file path");
            return 1;
        }
    };
    let project_dir_abs = workdir.join(&project_dir_rel);
    let potter_rollout_path = crate::potter_rollout::potter_rollout_path(&project_dir_abs);

    let git_branch = match crate::project::progress_file_git_branch(&progress_file_abs) {
        Ok(branch) => branch,
        Err(err) => {
            let _ = write_preflight_error(&format!("{err:#}"));
            return 1;
        }
    };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if write_jsonl_event(
        &mut out,
        &crate::exec_jsonl::ExecJsonlEvent::PotterProjectStarted(
            crate::exec_jsonl::PotterProjectStartedEvent {
                working_dir: workdir.to_string_lossy().to_string(),
                project_dir: project_dir_abs.to_string_lossy().to_string(),
                progress_file: progress_file_abs.to_string_lossy().to_string(),
                user_message: prompt.clone(),
                git_commit_start: init.git_commit_start.clone(),
                git_branch: git_branch.clone(),
            },
        ),
    )
    .is_err()
    {
        return 1;
    }

    let developer_prompt = crate::project::render_developer_prompt(&init.progress_file_rel);
    let turn_prompt = crate::project::fixed_prompt().trim_end().to_string();
    let rounds_total = u32::try_from(rounds.get()).unwrap_or(u32::MAX);

    let mut ui = crate::exec_json_round_ui::ExecJsonRoundUi::new(out, workdir.to_path_buf());

    let round_context = crate::round_runner::PotterRoundContext {
        codex_bin,
        developer_prompt,
        backend_launch,
        backend_event_mode: crate::app_server_backend::AppServerEventMode::ExecJson,
        codex_compat_home,
        thread_cwd: Some(workdir.to_path_buf()),
        turn_prompt,
        workdir: workdir.to_path_buf(),
        progress_file_rel: init.progress_file_rel.clone(),
        user_prompt_file: init.progress_file_rel.clone(),
        git_commit_start: init.git_commit_start.clone(),
        potter_rollout_path,
        project_started_at,
    };

    let mut rounds_run: u32 = 0;
    let mut final_outcome: crate::exec_jsonl::PotterProjectCompletedOutcome =
        crate::exec_jsonl::PotterProjectCompletedOutcome::BudgetExhausted;
    let mut final_message: Option<String> = None;

    for round_index in 0..rounds.get() {
        let current_round = u32::try_from(round_index.saturating_add(1)).unwrap_or(u32::MAX);
        let project_started = if round_index == 0 {
            Some(crate::round_runner::PotterProjectStartedInfo {
                user_message: Some(prompt.clone()),
                working_dir: workdir.to_path_buf(),
                project_dir: project_dir_rel.clone(),
                user_prompt_file: init.progress_file_rel.clone(),
            })
        } else {
            None
        };

        let round_result = match crate::round_runner::run_potter_round(
            &mut ui,
            &round_context,
            crate::round_runner::PotterRoundOptions {
                pad_before_first_cell: round_index != 0,
                project_started,
                round_current: current_round,
                round_total: rounds_total,
                project_succeeded_rounds: current_round,
            },
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                final_outcome = crate::exec_jsonl::PotterProjectCompletedOutcome::Fatal;
                final_message = Some(format!("{err:#}"));
                break;
            }
        };

        rounds_run = rounds_run.saturating_add(1);

        match round_result.exit_reason {
            ExitReason::Completed => {
                if round_result.stop_due_to_finite_incantatem {
                    final_outcome = crate::exec_jsonl::PotterProjectCompletedOutcome::Succeeded;
                    break;
                }
            }
            ExitReason::TaskFailed(message) => {
                final_outcome = crate::exec_jsonl::PotterProjectCompletedOutcome::TaskFailed;
                final_message = Some(message);
                break;
            }
            ExitReason::Fatal(message) => {
                final_outcome = crate::exec_jsonl::PotterProjectCompletedOutcome::Fatal;
                final_message = Some(message);
                break;
            }
            ExitReason::UserRequested => {
                final_outcome = crate::exec_jsonl::PotterProjectCompletedOutcome::Fatal;
                final_message = Some(String::from("user requested"));
                break;
            }
        }
    }

    let mut out = ui.into_output();

    let git_commit_end = crate::project::resolve_git_commit(workdir);

    let project_completed = crate::exec_jsonl::ExecJsonlEvent::PotterProjectCompleted(
        crate::exec_jsonl::PotterProjectCompletedEvent {
            outcome: final_outcome.clone(),
            message: final_message.clone(),
            rounds_run,
            rounds_total,
            duration_secs: project_started_at.elapsed().as_secs(),
            progress_file: progress_file_abs.to_string_lossy().to_string(),
            git_commit_start: init.git_commit_start,
            git_commit_end,
            git_branch,
        },
    );

    if write_jsonl_event(&mut out, &project_completed).is_err() {
        return 1;
    }

    if matches!(
        final_outcome,
        crate::exec_jsonl::PotterProjectCompletedOutcome::Succeeded
    ) {
        0
    } else {
        1
    }
}

fn read_prompt_from_stdin() -> anyhow::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

fn write_preflight_error(message: &str) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    write_jsonl_event(
        &mut out,
        &crate::exec_jsonl::ExecJsonlEvent::Error(crate::exec_jsonl::ThreadErrorEvent {
            message: message.to_string(),
        }),
    )
}

fn write_jsonl_event<W: Write>(
    out: &mut W,
    event: &crate::exec_jsonl::ExecJsonlEvent,
) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *out, event)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
