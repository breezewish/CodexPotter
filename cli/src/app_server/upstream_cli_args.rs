//! Utilities for forwarding Codex CLI flags to the upstream `codex` backend.
//!
//! CodexPotter spawns the upstream `codex app-server` process as a subprocess. Unlike the normal
//! interactive `codex` CLI, the `codex app-server` entrypoint only consumes a subset of the
//! top-level CLI flags. In particular, flags like `--profile` and `--search` are not wired
//! through to the app-server by upstream Codex today, so CodexPotter must translate them into
//! `--config key=value` overrides for them to take effect.
//!
//! `--model` is also ignored at the app-server CLI layer upstream, but CodexPotter can still
//! honor it by setting the typesafe `model` field on `thread/start` and `thread/resume` requests.

use clap::ArgAction;
use clap::Args;

/// Flags that should be forwarded to the upstream `codex` CLI when launching `codex app-server`.
///
/// Note that [`UpstreamCodexCliArgs::to_upstream_codex_args`] intentionally **does not** forward
/// `--model`, `--profile`, and `--search` directly, because upstream Codex ignores these flags for
/// `codex app-server`.
///
/// - `--profile` and `--search` are translated into `--config` overrides.
/// - `--model` is applied via upstream JSON-RPC `thread/*` params (`model` field).
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub struct UpstreamCodexCliArgs {
    /// Override a configuration value that would otherwise be loaded
    /// from `~/.codex/config.toml`. Use a dotted path (`foo.bar.baz`)
    /// to override nested values. The `value` portion is parsed as
    /// TOML. If it fails to parse as TOML, the raw string is used as a
    /// literal.
    ///
    /// Examples:
    /// - `-c model="o3"`
    /// - `-c 'sandbox_permissions=["disk-full-read-access"]'`
    /// - `-c shell_environment_policy.inherit=all`
    #[arg(
        short = 'c',
        long = "config",
        value_name = "key=value",
        action = ArgAction::Append,
        global = true,
    )]
    pub config_overrides: Vec<String>,

    /// Enable a feature (repeatable). Equivalent to `-c features.<name>=true`.
    #[arg(
        long = "enable",
        value_name = "FEATURE",
        action = ArgAction::Append,
        global = true
    )]
    pub enable_features: Vec<String>,

    /// Disable a feature (repeatable). Equivalent to `-c features.<name>=false`.
    #[arg(
        long = "disable",
        value_name = "FEATURE",
        action = ArgAction::Append,
        global = true
    )]
    pub disable_features: Vec<String>,

    /// Model the agent should use.
    #[arg(long = "model", short = 'm', value_name = "MODEL", global = true)]
    pub model: Option<String>,

    /// Configuration profile from config.toml to specify default options.
    #[arg(
        long = "profile",
        short = 'p',
        value_name = "CONFIG_PROFILE",
        global = true
    )]
    pub profile: Option<String>,

    /// Enable live web search. When enabled, the native Responses `web_search` tool is available
    /// to the model (no per‑call approval).
    #[arg(long = "search", default_value_t = false, global = true)]
    pub web_search: bool,
}

impl UpstreamCodexCliArgs {
    /// Render CLI args for launching `codex-potter app-server` as a subprocess.
    ///
    /// These args are the "user-facing" ones, preserving flags like `--model` and `--profile`.
    pub fn to_potter_app_server_args(&self) -> Vec<String> {
        let mut out = Vec::new();

        for override_kv in &self.config_overrides {
            out.push("--config".to_string());
            out.push(override_kv.clone());
        }

        for feature in &self.enable_features {
            out.push("--enable".to_string());
            out.push(feature.clone());
        }

        for feature in &self.disable_features {
            out.push("--disable".to_string());
            out.push(feature.clone());
        }

        if let Some(model) = &self.model {
            out.push("--model".to_string());
            out.push(model.clone());
        }

        if let Some(profile) = &self.profile {
            out.push("--profile".to_string());
            out.push(profile.clone());
        }

        if self.web_search {
            out.push("--search".to_string());
        }

        out
    }

    /// Render CLI args for launching the upstream `codex app-server` backend.
    ///
    /// This method converts `--profile` and `--search` to `--config` overrides because upstream
    /// Codex does not wire those flags through to the app-server entrypoint.
    pub fn to_upstream_codex_args(&self) -> Vec<String> {
        let mut out = Vec::new();

        for override_kv in &self.config_overrides {
            out.push("--config".to_string());
            out.push(override_kv.clone());
        }

        if let Some(profile) = &self.profile {
            out.push("--config".to_string());
            out.push(format!("profile={}", toml_string_literal(profile)));
        }

        if self.web_search {
            out.push("--config".to_string());
            out.push("web_search=\"live\"".to_string());
        }

        for feature in &self.enable_features {
            out.push("--enable".to_string());
            out.push(feature.clone());
        }

        for feature in &self.disable_features {
            out.push("--disable".to_string());
            out.push(feature.clone());
        }

        out
    }

    /// Fold higher-level runtime flags into the effective `--config key=value` overrides that
    /// determine startup-banner config resolution.
    pub fn effective_runtime_config_overrides(&self) -> Vec<String> {
        let mut out = self.config_overrides.clone();

        if let Some(profile) = &self.profile {
            out.push(format!("profile={}", toml_string_literal(profile)));
        }

        if self.web_search {
            out.push("web_search=\"live\"".to_string());
        }

        for feature in &self.enable_features {
            out.push(format!("features.{feature}=true"));
        }

        for feature in &self.disable_features {
            out.push(format!("features.{feature}=false"));
        }

        out
    }
}

fn toml_string_literal(input: &str) -> String {
    // Encode as a TOML basic string so upstream parses these values as strings even if they look
    // like TOML scalars (e.g. "true", "42").
    //
    // We escape all control characters (including C1 controls) to ensure the produced value is a
    // single-line, valid TOML string literal that can safely be embedded in `--config key=value`.
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{08}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn upstream_args_translate_profile_and_search_to_config_overrides() {
        let args = UpstreamCodexCliArgs {
            config_overrides: vec!["foo=1".to_string()],
            enable_features: vec!["unified_exec".to_string()],
            disable_features: vec!["web_search_request".to_string()],
            model: Some("o3".to_string()),
            profile: Some("my-profile".to_string()),
            web_search: true,
        };

        assert_eq!(
            args.to_upstream_codex_args(),
            vec![
                "--config",
                "foo=1",
                "--config",
                "profile=\"my-profile\"",
                "--config",
                "web_search=\"live\"",
                "--enable",
                "unified_exec",
                "--disable",
                "web_search_request",
            ]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn potter_app_server_args_preserve_user_facing_flags() {
        let args = UpstreamCodexCliArgs {
            config_overrides: Vec::new(),
            enable_features: Vec::new(),
            disable_features: Vec::new(),
            model: Some("o3".to_string()),
            profile: Some("my-profile".to_string()),
            web_search: true,
        };

        assert_eq!(
            args.to_potter_app_server_args(),
            vec!["--model", "o3", "--profile", "my-profile", "--search"]
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn toml_string_literal_forces_string_values() {
        let args = UpstreamCodexCliArgs {
            profile: Some("true".to_string()),
            ..Default::default()
        };

        assert_eq!(
            args.to_upstream_codex_args(),
            vec!["--config", "profile=\"true\""]
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn toml_string_literal_escapes_control_characters() {
        assert_eq!(toml_string_literal("a\u{08}b"), "\"a\\bb\"");
        assert_eq!(toml_string_literal("a\u{0C}b"), "\"a\\fb\"");
        assert_eq!(toml_string_literal("a\u{1F}b"), "\"a\\u001Fb\"");
        assert_eq!(toml_string_literal("a\u{7F}b"), "\"a\\u007Fb\"");
        assert_eq!(toml_string_literal("a\u{85}b"), "\"a\\u0085b\"");
    }

    #[test]
    fn effective_runtime_config_overrides_fold_high_level_flags() {
        let args = UpstreamCodexCliArgs {
            config_overrides: vec!["foo=1".to_string()],
            enable_features: vec!["fast_mode".to_string()],
            disable_features: vec!["web_search_request".to_string()],
            model: Some("o3".to_string()),
            profile: Some("my-profile".to_string()),
            web_search: true,
        };

        assert_eq!(
            args.effective_runtime_config_overrides(),
            vec![
                "foo=1",
                "profile=\"my-profile\"",
                "web_search=\"live\"",
                "features.fast_mode=true",
                "features.web_search_request=false",
            ]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
        );
    }
}
