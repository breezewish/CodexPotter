/// Update action the CLI should perform after the TUI exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// Update via `npm install -g codex-potter`.
    NpmGlobalLatest,
    /// Update via `bun install -g codex-potter`.
    BunGlobalLatest,
}

impl UpdateAction {
    /// Returns the list of command-line arguments for invoking the update.
    pub fn command_args(self) -> (&'static str, &'static [&'static str]) {
        match self {
            UpdateAction::NpmGlobalLatest => ("npm", &["install", "-g", "codex-potter"]),
            UpdateAction::BunGlobalLatest => ("bun", &["install", "-g", "codex-potter"]),
        }
    }

    /// Returns a shell-escaped string representation of the update command.
    pub fn command_str(self) -> String {
        let (command, args) = self.command_args();
        shlex::try_join(std::iter::once(command).chain(args.iter().copied()))
            .unwrap_or_else(|_| format!("{command} {}", args.join(" ")))
    }
}

#[cfg(not(debug_assertions))]
pub fn get_update_action() -> Option<UpdateAction> {
    let managed_by_npm = std::env::var_os("CODEX_POTTER_MANAGED_BY_NPM").is_some();
    let managed_by_bun = std::env::var_os("CODEX_POTTER_MANAGED_BY_BUN").is_some();
    detect_update_action(managed_by_npm, managed_by_bun)
}

#[cfg(any(not(debug_assertions), test))]
fn detect_update_action(managed_by_npm: bool, managed_by_bun: bool) -> Option<UpdateAction> {
    if managed_by_npm {
        Some(UpdateAction::NpmGlobalLatest)
    } else if managed_by_bun {
        Some(UpdateAction::BunGlobalLatest)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_update_action_without_env_mutation() {
        assert_eq!(detect_update_action(false, false), None);
        assert_eq!(
            detect_update_action(true, false),
            Some(UpdateAction::NpmGlobalLatest)
        );
        assert_eq!(
            detect_update_action(false, true),
            Some(UpdateAction::BunGlobalLatest)
        );
    }
}
