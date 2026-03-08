//! CodexPotter CLI entrypoint.
//!
//! This binary wires together three major layers:
//!
//! - `app_server`: Drives the upstream `codex app-server` process (execution plane), and also
//!   provides the long-lived `codex-potter app-server` implementation (project control plane).
//! - `workflow`: Orchestrates CodexPotter projects/rounds, persists `potter-rollout.jsonl`, and
//!   supports `resume` by replaying recorded events.
//! - `exec`: Runs CodexPotter non-interactively and emits a machine-readable JSONL stream
//!   (`codex-potter exec --json`).
//!
//! Interactive mode (default) uses the `codex-tui` crate for rendering; the TUI is kept as a pure
//! renderer that is driven by the `EventMsg` stream from the app-server.

mod app_server;
mod atomic_write;
mod codex_compat;
mod config;
mod exec;
mod global_gitignore;
mod path_utils;
mod rounds;
mod startup;
mod workflow;

use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum CliSandbox {
    #[default]
    Default,
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CliSandbox {
    fn as_protocol(self) -> Option<crate::app_server::upstream_protocol::SandboxMode> {
        match self {
            CliSandbox::Default => None,
            CliSandbox::ReadOnly => {
                Some(crate::app_server::upstream_protocol::SandboxMode::ReadOnly)
            }
            CliSandbox::WorkspaceWrite => {
                Some(crate::app_server::upstream_protocol::SandboxMode::WorkspaceWrite)
            }
            CliSandbox::DangerFullAccess => {
                Some(crate::app_server::upstream_protocol::SandboxMode::DangerFullAccess)
            }
        }
    }
}

#[derive(Parser, Debug)]
#[command(author = "Codex", version, about = "Run CodexPotter interactively")]
struct Cli {
    /// Path to the `codex` CLI binary to launch in app-server mode.
    #[arg(long, env = "CODEX_BIN", default_value = "codex", global = true)]
    codex_bin: String,

    /// Number of turns to run (each turn starts a fresh `codex app-server`; must be >= 1).
    ///
    /// For `resume`, this controls how many rounds are run when the last recorded round is
    /// complete. If the last recorded round is unfinished, the remaining budget is derived from
    /// the recorded `round_total` in `potter-rollout.jsonl`.
    #[arg(long, default_value = "10", global = true)]
    rounds: NonZeroUsize,

    /// Sandbox mode to request from Codex.
    ///
    /// `default` matches codex-cli behavior: no `--sandbox` flag is passed to the app-server and
    /// the sandbox policy is left for Codex to decide.
    #[arg(long = "sandbox", value_enum, default_value_t, global = true)]
    sandbox: CliSandbox,

    /// Pass Codex's bypass flag when launching `codex app-server`.
    ///
    /// Alias: `--yolo`.
    #[arg(
        long = "dangerously-bypass-approvals-and-sandbox",
        alias = "yolo",
        global = true
    )]
    dangerously_bypass_approvals_and_sandbox: bool,

    #[clap(flatten)]
    upstream_cli_args: crate::app_server::UpstreamCodexCliArgs,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand, Debug)]
enum CliCommand {
    /// Resume a CodexPotter project (replay history and optionally continue iterating).
    Resume {
        /// Project path to resolve to a unique `MAIN.md`. If omitted, open a picker UI.
        project_path: Option<PathBuf>,
    },
    /// Run CodexPotter non-interactively and emit a machine-readable JSONL event stream.
    Exec {
        /// Prompt to run. If omitted, read from stdin.
        prompt: Option<String>,
        /// Emit a strict JSONL event stream to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Run a long-lived JSON-RPC app-server that encapsulates CodexPotter project logic.
    ///
    /// This is primarily intended for internal use.
    AppServer,
}

fn parse_cli() -> Cli {
    let matches = Cli::command()
        .version(codex_tui::CODEX_POTTER_VERSION)
        .get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit())
}

fn resolve_codex_bin_or_exit(codex_bin: &str) -> String {
    match startup::resolve_codex_bin(codex_bin) {
        Ok(resolved) => resolved.command_for_spawn,
        Err(err) => {
            eprint!("{}", err.render_ansi());
            std::process::exit(1);
        }
    }
}

fn resolve_workdir_or_exec_json_exit() -> PathBuf {
    match std::env::current_dir() {
        Ok(workdir) => workdir,
        Err(err) => {
            let message = format!("resolve current directory: {err}");
            eprintln!("error: {message}");
            let _ = crate::exec::write_exec_json_preflight_error(&message);
            std::process::exit(1);
        }
    }
}

fn resolve_codex_bin_or_exec_json_exit(codex_bin: &str) -> String {
    match startup::resolve_codex_bin(codex_bin) {
        Ok(resolved) => resolved.command_for_spawn,
        Err(err) => {
            eprint!("{}", err.render_ansi());
            let _ = crate::exec::write_exec_json_preflight_error(&err.to_string());
            std::process::exit(1);
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = parse_cli();
    let backend_launch = crate::app_server::AppServerLaunchConfig::from_cli(
        cli.sandbox,
        cli.dangerously_bypass_approvals_and_sandbox,
    );
    let upstream_cli_args = cli.upstream_cli_args.clone();

    if let Some(CliCommand::Exec { prompt, json }) = cli.command.as_ref() {
        if !json {
            eprintln!("error: currently only --json output is supported for exec");
            std::process::exit(1);
        }

        let workdir = resolve_workdir_or_exec_json_exit();
        maybe_apply_default_global_gitignore(&workdir);
        let codex_bin = resolve_codex_bin_or_exec_json_exit(&cli.codex_bin);

        let exit_code = crate::exec::run_exec_json(
            &workdir,
            prompt.clone(),
            cli.rounds,
            codex_bin,
            backend_launch,
            upstream_cli_args,
        )
        .await;
        std::process::exit(exit_code);
    }

    crate::rounds::round_budget_to_u32(cli.rounds)?;

    let workdir = std::env::current_dir().context("resolve current directory")?;
    let codex_bin = resolve_codex_bin_or_exit(&cli.codex_bin);

    if matches!(cli.command, Some(CliCommand::AppServer)) {
        maybe_apply_default_global_gitignore(&workdir);
        let codex_compat_home = match crate::codex_compat::ensure_default_codex_compat_home() {
            Ok(home) => home,
            Err(err) => {
                eprintln!("warning: failed to configure codex-compat home: {err}");
                None
            }
        };

        crate::app_server::potter::run_potter_app_server(
            crate::app_server::potter::PotterAppServerConfig {
                default_workdir: workdir,
                codex_bin,
                backend_launch,
                codex_compat_home,
                rounds: cli.rounds,
                upstream_cli_args,
            },
        )
        .await?;
        return Ok(());
    }

    let mut resume_note_project_path: Option<String> = None;

    let check_for_update_on_startup = crate::config::ConfigStore::new_default()
        .and_then(|store| store.check_for_update_on_startup())
        .unwrap_or(true);
    let turn_prompt = crate::workflow::project::fixed_prompt()
        .trim_end()
        .to_string();

    let mut ui = codex_tui::CodexPotterTui::new()?;
    ui.set_startup_banner_codex_overrides(
        &workdir,
        cli.upstream_cli_args.model.clone(),
        cli.upstream_cli_args.effective_runtime_config_overrides(),
    )
    .context("resolve startup banner Codex model config")?;

    ui.set_check_for_update_on_startup(check_for_update_on_startup);
    if let Some(update_action) = ui.prompt_update_if_needed().await? {
        drop(ui);
        run_update_action(update_action)?;
        return Ok(());
    }

    let global_gitignore_prompt_plan = prepare_global_gitignore_prompt(&workdir);
    let should_prompt_startup_verbosity = ui.should_prompt_startup_verbosity();
    let total_setup_steps = usize::from(global_gitignore_prompt_plan.is_some())
        + usize::from(should_prompt_startup_verbosity);

    let mut setup_step_index = 1;
    if let Some(plan) = global_gitignore_prompt_plan {
        let setup_step = if total_setup_steps > 1 {
            Some(codex_tui::StartupSetupStep::new(
                setup_step_index,
                total_setup_steps,
            ))
        } else {
            None
        };
        maybe_prompt_global_gitignore(&mut ui, &workdir, plan, setup_step).await;
        setup_step_index += 1;
    }

    if should_prompt_startup_verbosity {
        let setup_step = if total_setup_steps > 1 {
            Some(codex_tui::StartupSetupStep::new(
                setup_step_index,
                total_setup_steps,
            ))
        } else {
            None
        };
        maybe_prompt_startup_verbosity(&mut ui, setup_step).await;
    }

    let mut project_queue_workdir = workdir.clone();

    let mut potter_app_server = crate::app_server::potter::PotterAppServerClient::spawn(
        workdir.clone(),
        codex_bin.clone(),
        cli.rounds,
        backend_launch,
        cli.upstream_cli_args.clone(),
    )
    .await
    .context("spawn potter app-server")?;
    potter_app_server
        .initialize()
        .await
        .context("initialize potter app-server")?;

    if let Some(CliCommand::Resume { project_path }) = cli.command.as_ref() {
        let project_path = match project_path {
            Some(project_path) => Some(project_path.clone()),
            None => {
                let rows = {
                    let mut buffered_events = Vec::new();
                    let response = potter_app_server
                        .project_list(
                            crate::app_server::potter::ProjectListParams::default(),
                            &mut buffered_events,
                        )
                        .await
                        .context("project/list via potter app-server")?;
                    anyhow::ensure!(
                        buffered_events.is_empty(),
                        "internal error: unexpected events during project/list"
                    );

                    response
                        .projects
                        .into_iter()
                        .filter_map(|project| {
                            let created_at = std::time::UNIX_EPOCH.checked_add(
                                std::time::Duration::from_secs(project.created_at_unix_secs),
                            )?;
                            let updated_at = std::time::UNIX_EPOCH.checked_add(
                                std::time::Duration::from_secs(project.updated_at_unix_secs),
                            )?;
                            Some(codex_tui::ResumePickerRow {
                                project_path: project.project_path,
                                user_request: project.user_request,
                                created_at,
                                updated_at,
                                git_branch: project.git_branch,
                            })
                        })
                        .collect::<Vec<_>>()
                };
                match ui.prompt_resume_picker(rows).await? {
                    codex_tui::ResumePickerOutcome::StartFresh => None,
                    codex_tui::ResumePickerOutcome::Resume(project_path) => Some(project_path),
                    codex_tui::ResumePickerOutcome::Exit => return Ok(()),
                }
            }
        };

        if let Some(project_path) = project_path {
            let resume_exit = crate::workflow::resume::run_resume(
                &mut ui,
                &mut potter_app_server,
                &workdir,
                &project_path,
                cli.rounds,
            )
            .await
            .context("resume project")?;
            match resume_exit {
                crate::workflow::resume::ResumeExit::Completed => {}
                crate::workflow::resume::ResumeExit::UserRequested => {
                    let queued_prompts = ui
                        .take_queued_user_prompts()
                        .into_iter()
                        .collect::<Vec<_>>();
                    let _ = potter_app_server.shutdown().await;
                    drop(ui);
                    print_queued_prompts_note(&queued_prompts);
                    return Ok(());
                }
                crate::workflow::resume::ResumeExit::FatalExitRequested => {
                    // `std::process::exit` skips destructors, so explicitly drop the UI to restore
                    // terminal state before exiting.
                    drop(ui);
                    std::process::exit(1);
                }
            }
            project_queue_workdir =
                std::env::current_dir().context("resolve current directory after resume")?;
        }
    }

    let project_queue_exit = crate::workflow::project_runner::run_project_queue(
        &mut ui,
        &mut potter_app_server,
        project_queue_workdir.clone(),
        crate::workflow::project_runner::ProjectQueueOptions {
            rounds: cli.rounds,
            turn_prompt: turn_prompt.clone(),
        },
    )
    .await?;

    let mut queued_prompts_on_exit: Vec<String> = Vec::new();
    match project_queue_exit {
        crate::workflow::project_runner::ProjectQueueExit::Completed => {}
        crate::workflow::project_runner::ProjectQueueExit::UserRequestedExit { project_dir } => {
            queued_prompts_on_exit = ui
                .take_queued_user_prompts()
                .into_iter()
                .collect::<Vec<_>>();
            resume_note_project_path = Some(
                derive_resume_project_path_from_project_dir(&project_dir)
                    .unwrap_or_else(|| project_dir.to_string_lossy().to_string()),
            );
        }
        crate::workflow::project_runner::ProjectQueueExit::FatalExitRequested => {
            // `std::process::exit` skips destructors, so explicitly drop the UI to restore terminal
            // state before exiting.
            drop(ui);
            std::process::exit(1);
        }
    }

    let _ = potter_app_server.shutdown().await;

    drop(ui);
    print_queued_prompts_note(&queued_prompts_on_exit);
    if let Some(project_path) = resume_note_project_path {
        print_resume_note(&project_path);
    }

    Ok(())
}

fn run_update_action(action: codex_tui::UpdateAction) -> anyhow::Result<()> {
    println!();
    let cmd_str = action.command_str();
    println!("Updating CodexPotter via `{cmd_str}`...");

    let status = {
        #[cfg(windows)]
        {
            // On Windows, run via cmd.exe so .CMD/.BAT are correctly resolved (PATHEXT semantics).
            std::process::Command::new("cmd")
                .args(["/C", &cmd_str])
                .status()?
        }
        #[cfg(not(windows))]
        {
            let (cmd, args) = action.command_args();
            std::process::Command::new(cmd).args(args).status()?
        }
    };

    if !status.success() {
        anyhow::bail!("`{cmd_str}` failed with status {status}");
    }

    println!("Update ran successfully! Please restart CodexPotter.");
    Ok(())
}

fn derive_resume_project_path_from_project_dir(project_dir: &Path) -> Option<String> {
    let projects_root = Path::new(".codexpotter").join("projects");
    let project_path = project_dir.strip_prefix(&projects_root).ok()?;
    let parts = project_path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn print_resume_note(project_path: &str) {
    let command = format!("codex-potter resume {project_path}");
    println!("{} To continue this project, run:", ansi_bold("Note:"));
    println!("  {}", ansi_cyan(&command));
}

fn print_queued_prompts_note(queued_prompts: &[String]) {
    let Some(note) = render_queued_prompts_note(queued_prompts) else {
        return;
    };

    print!("{note}");
}

fn render_queued_prompts_note(queued_prompts: &[String]) -> Option<String> {
    if queued_prompts.is_empty() {
        return None;
    }

    let count = queued_prompts.len();
    let prompt_label = if count == 1 { "prompt" } else { "prompts" };
    let verb = if count == 1 { "was" } else { "were" };

    let mut note = String::new();
    note.push('\n');
    note.push_str(&format!(
        "{} You have {count} queued {prompt_label} that {verb} not run before exiting.\n",
        ansi_bold("Warning:")
    ));
    note.push_str("Copy/paste them to continue:\n");

    for (idx, prompt) in queued_prompts.iter().enumerate() {
        let index = idx + 1;
        note.push_str(&format!("--- queued prompt {index}/{count} ---\n"));
        note.push_str(prompt);
        if !prompt.ends_with('\n') {
            note.push('\n');
        }
        note.push_str(&format!("--- end queued prompt {index}/{count} ---\n"));
    }

    Some(note)
}

fn ansi_bold(text: &str) -> String {
    format!("\u{1b}[1m{text}\u{1b}[0m")
}

fn ansi_cyan(text: &str) -> String {
    format!("\u{1b}[36m{text}\u{1b}[0m")
}

fn maybe_apply_default_global_gitignore(workdir: &std::path::Path) {
    let status = match crate::global_gitignore::detect_global_gitignore(workdir) {
        Ok(status) => status,
        Err(err) => {
            eprintln!("warning: failed to resolve global gitignore: {err}");
            return;
        }
    };

    if status.has_codexpotter_ignore {
        return;
    }

    match crate::global_gitignore::ensure_codexpotter_ignored(workdir, &status.path) {
        Ok(()) => eprintln!(
            "{} added {} to global gitignore {}",
            ansi_bold("Notice:"),
            ansi_cyan(crate::global_gitignore::CODEXPOTTER_GITIGNORE_ENTRY),
            ansi_cyan(&status.path_display)
        ),
        Err(err) => eprintln!(
            "warning: failed to update global gitignore {}: {err}",
            status.path_display
        ),
    }
}

struct GlobalGitignorePromptPlan {
    config_store: crate::config::ConfigStore,
    status: crate::global_gitignore::GlobalGitignoreStatus,
}

fn prepare_global_gitignore_prompt(workdir: &std::path::Path) -> Option<GlobalGitignorePromptPlan> {
    let config_store = match crate::config::ConfigStore::new_default() {
        Ok(store) => store,
        Err(err) => {
            eprintln!("warning: failed to locate codexpotter config: {err}");
            return None;
        }
    };

    let hide_prompt = config_store.notice_hide_gitignore_prompt().unwrap_or(false);
    if hide_prompt {
        return None;
    }

    let status = match crate::global_gitignore::detect_global_gitignore(workdir) {
        Ok(status) => status,
        Err(err) => {
            eprintln!("warning: failed to resolve global gitignore: {err}");
            return None;
        }
    };
    if status.has_codexpotter_ignore {
        return None;
    }

    Some(GlobalGitignorePromptPlan {
        config_store,
        status,
    })
}

async fn maybe_prompt_global_gitignore(
    ui: &mut codex_tui::CodexPotterTui,
    workdir: &std::path::Path,
    plan: GlobalGitignorePromptPlan,
    setup_step: Option<codex_tui::StartupSetupStep>,
) {
    let outcome = match ui
        .prompt_global_gitignore(plan.status.path_display.clone(), setup_step)
        .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("warning: global gitignore prompt failed: {err}");
            let _ = ui.clear();
            return;
        }
    };

    match outcome {
        codex_tui::GlobalGitignorePromptOutcome::AddToGlobalGitignore => {
            if let Err(err) =
                crate::global_gitignore::ensure_codexpotter_ignored(workdir, &plan.status.path)
            {
                eprintln!("warning: failed to update global gitignore: {err}");
            }
        }
        codex_tui::GlobalGitignorePromptOutcome::No => {}
        codex_tui::GlobalGitignorePromptOutcome::NoDontAskAgain => {
            if let Err(err) = plan.config_store.set_notice_hide_gitignore_prompt(true) {
                eprintln!("warning: failed to persist config: {err}");
            }
        }
    }
}

async fn maybe_prompt_startup_verbosity(
    ui: &mut codex_tui::CodexPotterTui,
    setup_step: Option<codex_tui::StartupSetupStep>,
) {
    if let Err(err) = ui.prompt_startup_verbosity(setup_step).await {
        eprintln!("warning: startup verbosity prompt failed: {err}");
        let _ = ui.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn rounds_must_be_at_least_one() {
        assert!(Cli::try_parse_from(["codex-potter", "--rounds", "0"]).is_err());
        assert!(Cli::try_parse_from(["codex-potter", "--rounds", "1"]).is_ok());
    }

    #[test]
    fn yolo_alias_sets_bypass_flag() {
        let cli = Cli::try_parse_from(["codex-potter", "--yolo"]).expect("parse args");
        assert!(cli.dangerously_bypass_approvals_and_sandbox);
    }

    #[test]
    fn resume_allows_global_args_after_subcommand() {
        let cli = Cli::try_parse_from([
            "codex-potter",
            "resume",
            "2026/02/01/1",
            "--yolo",
            "--sandbox",
            "read-only",
            "--rounds",
            "3",
            "--codex-bin",
            "custom-codex",
            "--model",
            "o3",
            "--profile",
            "my-profile",
            "--search",
            "--config",
            "model_reasoning_effort=\"high\"",
            "--enable",
            "unified_exec",
            "--disable",
            "web_search_request",
        ])
        .expect("parse args");

        assert!(cli.dangerously_bypass_approvals_and_sandbox);
        assert_eq!(cli.sandbox, CliSandbox::ReadOnly);
        assert_eq!(cli.rounds.get(), 3);
        assert_eq!(cli.codex_bin, "custom-codex");
        assert_eq!(cli.upstream_cli_args.model.as_deref(), Some("o3"));
        assert_eq!(cli.upstream_cli_args.profile.as_deref(), Some("my-profile"));
        assert!(cli.upstream_cli_args.web_search);
        assert_eq!(
            cli.upstream_cli_args.config_overrides,
            vec!["model_reasoning_effort=\"high\"".to_string()]
        );
        assert_eq!(
            cli.upstream_cli_args.enable_features,
            vec!["unified_exec".to_string()]
        );
        assert_eq!(
            cli.upstream_cli_args.disable_features,
            vec!["web_search_request".to_string()]
        );

        let Some(CliCommand::Resume { project_path }) = cli.command else {
            panic!("expected resume command, got: {:?}", cli.command);
        };
        assert_eq!(project_path, Some(PathBuf::from("2026/02/01/1")));
    }

    #[test]
    fn resume_subcommand_parses_project_path() {
        let cli =
            Cli::try_parse_from(["codex-potter", "resume", "2026/02/01/1"]).expect("parse args");

        let Some(CliCommand::Resume { project_path }) = cli.command else {
            panic!("expected resume command, got: {:?}", cli.command);
        };
        assert_eq!(project_path, Some(PathBuf::from("2026/02/01/1")));
    }

    #[test]
    fn resume_subcommand_parses_without_project_path() {
        let cli = Cli::try_parse_from(["codex-potter", "resume"]).expect("parse args");

        let Some(CliCommand::Resume { project_path }) = cli.command else {
            panic!("expected resume command, got: {:?}", cli.command);
        };
        assert_eq!(project_path, None);
    }

    #[test]
    fn exec_subcommand_parses_prompt_and_json_flag() {
        let cli =
            Cli::try_parse_from(["codex-potter", "exec", "hello", "--json"]).expect("parse args");

        let Some(CliCommand::Exec { prompt, json }) = cli.command else {
            panic!("expected exec command, got: {:?}", cli.command);
        };
        assert_eq!(prompt, Some("hello".to_string()));
        assert!(json);
    }

    #[test]
    fn app_server_subcommand_parses() {
        let cli = Cli::try_parse_from(["codex-potter", "app-server"]).expect("parse args");

        assert!(matches!(cli.command, Some(CliCommand::AppServer)));
    }

    #[test]
    fn derive_resume_project_path_from_project_dir_strips_projects_root() {
        let project_dir = Path::new(".codexpotter/projects/2026/03/01/6");
        assert_eq!(
            derive_resume_project_path_from_project_dir(project_dir),
            Some("2026/03/01/6".to_string())
        );
    }

    #[test]
    fn derive_resume_project_path_from_project_dir_returns_none_when_unexpected() {
        let project_dir = Path::new("not-a-project-dir");
        assert_eq!(
            derive_resume_project_path_from_project_dir(project_dir),
            None
        );
    }

    #[test]
    fn render_queued_prompts_note_returns_none_when_empty() {
        assert_eq!(render_queued_prompts_note(&[]), None);
    }

    #[test]
    fn render_queued_prompts_note_preserves_prompt_whitespace() {
        let prompts = vec![
            String::from("  alpha\n\n  beta\n\n"),
            String::from("no trailing newline"),
        ];

        let output = render_queued_prompts_note(&prompts).expect("note");
        let expected = format!(
            "\n{} You have 2 queued prompts that were not run before exiting.\nCopy/paste them to continue:\n--- queued prompt 1/2 ---\n{}--- end queued prompt 1/2 ---\n--- queued prompt 2/2 ---\n{}\n--- end queued prompt 2/2 ---\n",
            ansi_bold("Warning:"),
            prompts[0],
            prompts[1],
        );

        assert_eq!(output, expected);
    }
}
