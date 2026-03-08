use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::openai_models::ReasoningEffort;
use toml_edit::DocumentMut;
use toml_edit::Item as TomlItem;
use toml_edit::Table as TomlTable;
use toml_edit::value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexServiceTier {
    Fast,
    Flex,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CodexConfig {
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    service_tier: Option<CodexServiceTier>,
    fast_mode_enabled: Option<bool>,
    profile: Option<String>,
    profiles: HashMap<String, CodexProfileModelConfig>,
    project_root_markers: Option<Vec<String>>,
    tui_theme: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CodexProfileModelConfig {
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    service_tier: Option<CodexServiceTier>,
    fast_mode_enabled: Option<bool>,
}

/// Resolved model metadata for the startup banner after applying layered Codex config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCodexModelConfig {
    /// Effective model name, including profile and project-layer overrides.
    pub model: String,
    /// Effective reasoning effort for the selected model, if configured.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Whether Fast mode is effectively enabled for startup banner display.
    pub is_fast: bool,
}

/// Resolve the effective model metadata used by the startup banner from layered Codex config.
pub fn resolve_codex_model_config(cwd: &Path) -> io::Result<ResolvedCodexModelConfig> {
    resolve_codex_model_config_with_runtime_overrides(cwd, None, &[])
}

pub(crate) fn resolve_codex_model_config_with_runtime_overrides(
    cwd: &Path,
    model_override: Option<&str>,
    runtime_config_overrides: &[String],
) -> io::Result<ResolvedCodexModelConfig> {
    let raw = load_codex_config(cwd, runtime_config_overrides)?;

    let profile_config = match &raw.profile {
        Some(name) => raw.profiles.get(name).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("config profile `{name}` not found"),
            )
        })?,
        None => CodexProfileModelConfig::default(),
    };

    let model = model_override
        .map(ToOwned::to_owned)
        .or(profile_config.model)
        .or(raw.model)
        .unwrap_or_else(|| DEFAULT_FALLBACK_MODEL.to_string());
    let reasoning_effort = profile_config.reasoning_effort.or(raw.reasoning_effort);
    let service_tier = profile_config.service_tier.or(raw.service_tier);
    let fast_mode_enabled = profile_config
        .fast_mode_enabled
        .or(raw.fast_mode_enabled)
        .unwrap_or(true);

    Ok(ResolvedCodexModelConfig {
        model,
        reasoning_effort,
        is_fast: fast_mode_enabled && matches!(service_tier, Some(CodexServiceTier::Fast)),
    })
}

pub fn resolve_codex_tui_theme(cwd: &Path) -> io::Result<Option<String>> {
    let raw = load_codex_config(cwd, &[])?;
    Ok(raw.tui_theme)
}

pub fn persist_codex_tui_theme(codex_home: &Path, name: &str) -> io::Result<()> {
    let config_path = codex_home.join("config.toml");
    let write_paths = crate::path_utils::resolve_symlink_write_paths(&config_path)?;
    let serialized = match write_paths.read_path {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err),
        },
        None => String::new(),
    };

    let mut doc = if serialized.is_empty() {
        DocumentMut::new()
    } else {
        serialized.parse::<DocumentMut>().map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Error parsing config file {}: {err}", config_path.display()),
            )
        })?
    };

    let tui = ensure_table_for_write(&mut doc, "tui")?;
    tui["theme"] = value(name.to_string());

    crate::path_utils::write_atomically(&write_paths.write_path, &doc.to_string())?;
    Ok(())
}

const DEFAULT_FALLBACK_MODEL: &str = "gpt-5.2-codex";

fn load_codex_config(cwd: &Path, runtime_config_overrides: &[String]) -> io::Result<CodexConfig> {
    let codex_home = find_codex_home()?;
    let mut base_config = CodexConfig::default();

    // Match codex config layering order (subset):
    // - system: /etc/codex/config.toml
    // - user:   $CODEX_HOME/config.toml (default ~/.codex/config.toml)
    // - project layers: ./.../.codex/config.toml from project root to cwd
    apply_config_layer_from_file(&mut base_config, &default_system_config_path())?;
    apply_config_layer_from_file(&mut base_config, &codex_home.join("config.toml"))?;

    let mut discovery_config = base_config.clone();
    // Runtime overrides can change `project_root_markers`, so they must participate in project
    // root discovery before we know which `.codex/config.toml` layers to load. We still reapply
    // them after loading project layers because runtime overrides have the highest precedence.
    apply_runtime_config_overrides(&mut discovery_config, runtime_config_overrides)?;

    let project_root_markers = discovery_config
        .project_root_markers
        .clone()
        .unwrap_or_else(default_project_root_markers);
    let project_root = find_project_root(cwd, &project_root_markers)?;

    let mut config = base_config;
    for dir in project_dirs_between(&project_root, cwd) {
        let dot_codex = dir.join(".codex");
        if !dot_codex.is_dir() {
            continue;
        }
        apply_config_layer_from_file(&mut config, &dot_codex.join("config.toml"))?;
    }

    apply_runtime_config_overrides(&mut config, runtime_config_overrides)?;

    Ok(config)
}

fn default_project_root_markers() -> Vec<String> {
    vec![".git".to_string()]
}

fn default_system_config_path() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/etc/codex/config.toml")
    }
    #[cfg(not(unix))]
    {
        PathBuf::new()
    }
}

pub fn find_codex_home() -> io::Result<PathBuf> {
    if let Ok(val) = std::env::var("CODEX_HOME")
        && !val.is_empty()
    {
        let path = PathBuf::from(&val);
        let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("CODEX_HOME points to {val:?}, but that path does not exist"),
            ),
            _ => std::io::Error::new(
                err.kind(),
                format!("failed to read CODEX_HOME {val:?}: {err}"),
            ),
        })?;

        if !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("CODEX_HOME points to {val:?}, but that path is not a directory"),
            ));
        }

        return path.canonicalize().map_err(|err| {
            std::io::Error::new(
                err.kind(),
                format!("failed to canonicalize CODEX_HOME {val:?}: {err}"),
            )
        });
    }

    let mut p = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Could not find home directory"))?;
    p.push(".codex");
    Ok(p)
}

fn find_project_root(cwd: &Path, project_root_markers: &[String]) -> io::Result<PathBuf> {
    if project_root_markers.is_empty() {
        return Ok(cwd.to_path_buf());
    }

    for ancestor in cwd.ancestors() {
        for marker in project_root_markers {
            let marker_path = ancestor.join(marker);
            if std::fs::metadata(&marker_path).is_ok() {
                return Ok(ancestor.to_path_buf());
            }
        }
    }

    Ok(cwd.to_path_buf())
}

fn project_dirs_between<'a>(project_root: &'a Path, cwd: &'a Path) -> Vec<&'a Path> {
    let mut dirs = cwd
        .ancestors()
        .scan(false, |done, ancestor| {
            if *done {
                None
            } else {
                if ancestor == project_root {
                    *done = true;
                }
                Some(ancestor)
            }
        })
        .collect::<Vec<_>>();
    dirs.reverse();
    dirs
}

fn apply_config_layer_from_file(config: &mut CodexConfig, path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }

    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!("Failed to read config file {}: {err}", path.display()),
            ));
        }
    };

    let doc = contents.parse::<DocumentMut>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Error parsing config file {}: {err}", path.display()),
        )
    })?;

    apply_config_layer_from_doc(config, &doc)
}

fn apply_config_layer_from_doc(config: &mut CodexConfig, doc: &DocumentMut) -> io::Result<()> {
    if let Some(item) = doc.get("model") {
        config.model = Some(read_string(item, "model")?);
    }
    if let Some(item) = doc.get("model_reasoning_effort") {
        config.reasoning_effort = Some(read_reasoning_effort(item, "model_reasoning_effort")?);
    }
    if let Some(item) = doc.get("service_tier") {
        config.service_tier = Some(read_service_tier(item, "service_tier")?);
    }
    if let Some(item) = doc.get("features") {
        let features = item.as_table().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "config field `features` must be a table",
            )
        })?;
        if let Some(fast_mode) = features.get("fast_mode") {
            config.fast_mode_enabled = Some(read_bool(fast_mode, "features.fast_mode")?);
        }
    }
    if let Some(item) = doc.get("profile") {
        config.profile = Some(read_string(item, "profile")?);
    }
    if let Some(item) = doc.get("project_root_markers") {
        config.project_root_markers = Some(read_string_array(item, "project_root_markers")?);
    }
    if let Some(item) = doc.get("tui") {
        let tui = item.as_table().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "config field `tui` must be a table",
            )
        })?;
        if let Some(theme) = tui.get("theme") {
            config.tui_theme = Some(read_string(theme, "tui.theme")?);
        }
    }

    if let Some(profiles_item) = doc.get("profiles") {
        let profiles_table = profiles_item.as_table().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "config field `profiles` must be a table",
            )
        })?;
        for (profile_name, profile_item) in profiles_table.iter() {
            let profile_table = profile_item.as_table().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config field `profiles.{profile_name}` must be a table"),
                )
            })?;

            let mut profile = config.profiles.remove(profile_name).unwrap_or_default();
            if let Some(item) = profile_table.get("model") {
                profile.model = Some(read_string(
                    item,
                    &format!("profiles.{profile_name}.model"),
                )?);
            }
            if let Some(item) = profile_table.get("model_reasoning_effort") {
                profile.reasoning_effort = Some(read_reasoning_effort(
                    item,
                    &format!("profiles.{profile_name}.model_reasoning_effort"),
                )?);
            }
            if let Some(item) = profile_table.get("service_tier") {
                profile.service_tier = Some(read_service_tier(
                    item,
                    &format!("profiles.{profile_name}.service_tier"),
                )?);
            }
            if let Some(item) = profile_table.get("features") {
                let features = item.as_table().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("config field `profiles.{profile_name}.features` must be a table"),
                    )
                })?;
                if let Some(fast_mode) = features.get("fast_mode") {
                    profile.fast_mode_enabled = Some(read_bool(
                        fast_mode,
                        &format!("profiles.{profile_name}.features.fast_mode"),
                    )?);
                }
            }
            config.profiles.insert(profile_name.to_string(), profile);
        }
    }

    Ok(())
}

fn apply_runtime_config_overrides(
    config: &mut CodexConfig,
    runtime_config_overrides: &[String],
) -> io::Result<()> {
    for override_kv in runtime_config_overrides {
        let doc = parse_runtime_config_override(override_kv)?;
        apply_config_layer_from_doc(config, &doc)?;
    }

    Ok(())
}

fn parse_runtime_config_override(override_kv: &str) -> io::Result<DocumentMut> {
    let Some((key, raw_value)) = override_kv.split_once('=') else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config override `{override_kv}` must be in key=value form"),
        ));
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config override `{override_kv}` must have a non-empty key"),
        ));
    }

    let raw_value = raw_value.trim();
    let override_source = format!("{key} = {raw_value}");
    if let Ok(doc) = override_source.parse::<DocumentMut>() {
        return Ok(doc);
    }

    let fallback_source = format!("{key} = {}", toml_string_literal(raw_value));
    fallback_source.parse::<DocumentMut>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config override `{override_kv}` is invalid: {err}"),
        )
    })
}

fn read_string(item: &TomlItem, field: &str) -> io::Result<String> {
    item.as_value()
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config field `{field}` must be a string"),
            )
        })
}

fn read_bool(item: &TomlItem, field: &str) -> io::Result<bool> {
    item.as_value()
        .and_then(toml_edit::Value::as_bool)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config field `{field}` must be a boolean"),
            )
        })
}

fn read_string_array(item: &TomlItem, field: &str) -> io::Result<Vec<String>> {
    let array = item.as_array().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{field}` must be an array of strings"),
        )
    })?;

    let mut out: Vec<String> = Vec::new();
    for value in array.iter() {
        let s = value.as_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config field `{field}` must be an array of strings"),
            )
        })?;
        out.push(s.to_string());
    }

    Ok(out)
}

fn ensure_table_for_write<'a>(
    doc: &'a mut DocumentMut,
    key: &str,
) -> io::Result<&'a mut TomlTable> {
    if doc.get(key).and_then(TomlItem::as_table).is_some() {
        return doc
            .get_mut(key)
            .and_then(TomlItem::as_table_mut)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config field `{key}` must be a table"),
                )
            });
    }

    if doc.get(key).is_none() {
        let mut table = TomlTable::new();
        table.set_implicit(false);
        doc[key] = TomlItem::Table(table);
    }

    let item = doc.get_mut(key).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be a table"),
        )
    })?;
    match item {
        TomlItem::Table(table) => Ok(table),
        TomlItem::Value(value) => {
            if let Some(inline) = value.as_inline_table() {
                let mut table = TomlTable::new();
                table.set_implicit(false);
                for (k, v) in inline.iter() {
                    table[k] = TomlItem::Value(v.clone());
                }
                *item = TomlItem::Table(table);
                item.as_table_mut().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("config field `{key}` must be a table"),
                    )
                })
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config field `{key}` must be a table"),
                ))
            }
        }
        TomlItem::None => {
            let mut table = TomlTable::new();
            table.set_implicit(false);
            *item = TomlItem::Table(table);
            item.as_table_mut().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("config field `{key}` must be a table"),
                )
            })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{key}` must be a table"),
        )),
    }
}

fn read_reasoning_effort(item: &TomlItem, field: &str) -> io::Result<ReasoningEffort> {
    let raw = read_string(item, field)?;
    parse_reasoning_effort(&raw).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{field}` has invalid value `{raw}`"),
        )
    })
}

fn read_service_tier(item: &TomlItem, field: &str) -> io::Result<CodexServiceTier> {
    let raw = read_string(item, field)?;
    parse_service_tier(&raw).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config field `{field}` has invalid value `{raw}`"),
        )
    })
}

fn parse_reasoning_effort(value: &str) -> Option<ReasoningEffort> {
    match value {
        "none" => Some(ReasoningEffort::None),
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::XHigh),
        _ => None,
    }
}

fn parse_service_tier(value: &str) -> Option<CodexServiceTier> {
    match value {
        "fast" => Some(CodexServiceTier::Fast),
        "flex" => Some(CodexServiceTier::Flex),
        _ => None,
    }
}

fn toml_string_literal(input: &str) -> String {
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
    use serial_test::serial;

    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let prev = std::env::var_os(key);
            // Safety: tests are serialized and restore the previous value on drop.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn write_config(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir parent");
        std::fs::write(path, contents).expect("write config");
    }

    #[test]
    #[serial]
    fn resolves_model_from_profile_when_selected() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
model = "gpt-5.2"
model_reasoning_effort = "xhigh"
service_tier = "flex"
profile = "work"

[profiles.work]
model = "gpt-5.2-codex"
model_reasoning_effort = "high"
service_tier = "fast"
"#,
        );

        let cwd = tempfile::tempdir().expect("cwd");
        let resolved = resolve_codex_model_config(cwd.path()).expect("resolve");
        assert_eq!(resolved.model, "gpt-5.2-codex");
        assert_eq!(resolved.reasoning_effort, Some(ReasoningEffort::High));
        assert!(resolved.is_fast);
    }

    #[test]
    #[serial]
    fn project_layer_overrides_user_layer() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
model = "gpt-5.2"
service_tier = "fast"
"#,
        );

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".git")).expect("mkdir .git");
        std::fs::create_dir_all(repo.path().join(".codex")).expect("mkdir .codex");
        write_config(
            &repo.path().join(".codex").join("config.toml"),
            r#"
model = "gpt-5.2-codex"
service_tier = "flex"
"#,
        );

        let resolved = resolve_codex_model_config(repo.path()).expect("resolve");
        assert_eq!(resolved.model, "gpt-5.2-codex");
        assert!(!resolved.is_fast);
    }

    #[test]
    #[serial]
    fn fast_banner_is_hidden_when_fast_feature_is_disabled() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
model = "gpt-5.2"
service_tier = "fast"

[features]
fast_mode = false
"#,
        );

        let cwd = tempfile::tempdir().expect("cwd");
        let resolved = resolve_codex_model_config(cwd.path()).expect("resolve");
        assert_eq!(
            resolved,
            ResolvedCodexModelConfig {
                model: "gpt-5.2".to_string(),
                reasoning_effort: None,
                is_fast: false,
            }
        );
    }

    #[test]
    #[serial]
    fn profile_fast_feature_override_is_applied() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
model = "gpt-5.2"
service_tier = "fast"
profile = "work"

[features]
fast_mode = false

[profiles.work.features]
fast_mode = true
"#,
        );

        let cwd = tempfile::tempdir().expect("cwd");
        let resolved = resolve_codex_model_config(cwd.path()).expect("resolve");
        assert_eq!(
            resolved,
            ResolvedCodexModelConfig {
                model: "gpt-5.2".to_string(),
                reasoning_effort: None,
                is_fast: true,
            }
        );
    }

    #[test]
    #[serial]
    fn runtime_overrides_replace_model_reasoning_and_fast_state() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
model = "gpt-5.2"
model_reasoning_effort = "low"
service_tier = "flex"

[features]
fast_mode = false
"#,
        );

        let cwd = tempfile::tempdir().expect("cwd");
        let resolved = resolve_codex_model_config_with_runtime_overrides(
            cwd.path(),
            Some("gpt-5.4"),
            &[
                "model_reasoning_effort=high".to_string(),
                "service_tier=fast".to_string(),
                "features.fast_mode=true".to_string(),
            ],
        )
        .expect("resolve");

        assert_eq!(
            resolved,
            ResolvedCodexModelConfig {
                model: "gpt-5.4".to_string(),
                reasoning_effort: Some(ReasoningEffort::High),
                is_fast: true,
            }
        );
    }

    #[test]
    #[serial]
    fn runtime_profile_override_combines_with_explicit_model_override() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
model = "gpt-5.2"
profile = "default"

[features]
fast_mode = false

[profiles.default]
model = "gpt-5.2"
model_reasoning_effort = "low"
service_tier = "flex"

[profiles.fast]
model = "gpt-5.3"
model_reasoning_effort = "high"
service_tier = "fast"

[profiles.fast.features]
fast_mode = true
"#,
        );

        let cwd = tempfile::tempdir().expect("cwd");
        let resolved = resolve_codex_model_config_with_runtime_overrides(
            cwd.path(),
            Some("gpt-5.4"),
            &["profile=\"fast\"".to_string()],
        )
        .expect("resolve");

        assert_eq!(
            resolved,
            ResolvedCodexModelConfig {
                model: "gpt-5.4".to_string(),
                reasoning_effort: Some(ReasoningEffort::High),
                is_fast: true,
            }
        );
    }

    #[test]
    #[serial]
    fn runtime_project_root_markers_override_affects_layer_discovery() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
model = "gpt-5.2"
"#,
        );

        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(repo.path().join("MARKER"), "").expect("write marker");
        std::fs::create_dir_all(repo.path().join(".codex")).expect("mkdir .codex");
        write_config(
            &repo.path().join(".codex").join("config.toml"),
            r#"
model = "gpt-5.4"
"#,
        );

        let cwd = repo.path().join("subdir");
        std::fs::create_dir_all(&cwd).expect("mkdir subdir");

        let resolved = resolve_codex_model_config_with_runtime_overrides(
            &cwd,
            None,
            &["project_root_markers=[\"MARKER\"]".to_string()],
        )
        .expect("resolve");

        assert_eq!(resolved.model, "gpt-5.4");
    }

    #[test]
    #[serial]
    fn invalid_service_tier_is_rejected() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
service_tier = "turbo"
"#,
        );

        let cwd = tempfile::tempdir().expect("cwd");
        let err = resolve_codex_model_config(cwd.path()).expect_err("expected error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string()
                .contains("config field `service_tier` has invalid value `turbo`"),
            "unexpected error: {err}",
        );
    }

    #[test]
    #[serial]
    fn resolving_selected_profile_errors_when_profile_is_missing() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
model = "gpt-5.2"
profile = "missing"
"#,
        );

        let cwd = tempfile::tempdir().expect("cwd");
        let err = resolve_codex_model_config(cwd.path()).expect_err("expected error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(
            err.to_string()
                .contains("config profile `missing` not found"),
            "unexpected error: {err}",
        );
    }

    #[test]
    #[serial]
    fn project_root_markers_can_change_project_root_discovery() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
model = "gpt-5.2"
project_root_markers = ["MARKER"]
"#,
        );

        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(repo.path().join("MARKER"), "").expect("write marker");
        std::fs::create_dir_all(repo.path().join(".codex")).expect("mkdir .codex");
        write_config(
            &repo.path().join(".codex").join("config.toml"),
            r#"
model = "gpt-5.2-codex"
"#,
        );

        let cwd = repo.path().join("subdir");
        std::fs::create_dir_all(&cwd).expect("mkdir subdir");

        let resolved = resolve_codex_model_config(&cwd).expect("resolve");
        assert_eq!(resolved.model, "gpt-5.2-codex");
    }

    #[test]
    #[serial]
    fn resolves_tui_theme_from_layered_config() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        write_config(
            &codex_home.path().join("config.toml"),
            r#"
[tui]
theme = "catppuccin-mocha"
"#,
        );

        let cwd = tempfile::tempdir().expect("cwd");
        let resolved = resolve_codex_tui_theme(cwd.path()).expect("resolve");
        assert_eq!(resolved, Some("catppuccin-mocha".to_string()));
    }

    #[test]
    #[cfg(unix)]
    fn persist_tui_theme_writes_through_symlink() {
        use std::os::unix::fs::symlink;

        let codex_home_real = tempfile::tempdir().expect("tempdir");
        let codex_home_compat = tempfile::tempdir().expect("tempdir");

        let target = codex_home_real.path().join("config.toml");
        let link = codex_home_compat.path().join("config.toml");
        symlink(&target, &link).expect("symlink");

        persist_codex_tui_theme(codex_home_compat.path(), "github").expect("persist theme");

        let link_meta = std::fs::symlink_metadata(&link).expect("symlink metadata");
        assert!(link_meta.file_type().is_symlink());

        let contents = std::fs::read_to_string(&target).expect("read target config");
        assert!(
            contents.contains("theme = \"github\""),
            "expected persisted config to contain theme selection, got: {contents}"
        );
    }

    #[test]
    #[serial]
    fn find_codex_home_env_missing_path_is_fatal() {
        let temp_home = tempfile::tempdir().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let _env = EnvVarGuard::set("CODEX_HOME", &missing);

        let err = find_codex_home().expect_err("missing CODEX_HOME");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(
            err.to_string().contains("CODEX_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[serial]
    fn find_codex_home_env_file_path_is_fatal() {
        let temp_home = tempfile::tempdir().expect("temp home");
        let file_path = temp_home.path().join("codex-home.txt");
        std::fs::write(&file_path, "not a directory").expect("write temp file");
        let _env = EnvVarGuard::set("CODEX_HOME", &file_path);

        let err = find_codex_home().expect_err("file CODEX_HOME");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[serial]
    fn find_codex_home_env_valid_directory_canonicalizes() {
        let codex_home = tempfile::tempdir().expect("temp codex home");
        let _env = EnvVarGuard::set("CODEX_HOME", codex_home.path());

        let resolved = find_codex_home().expect("valid CODEX_HOME");
        let expected = codex_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        assert_eq!(resolved, expected);
    }
}
