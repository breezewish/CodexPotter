use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
#[cfg(not(debug_assertions))]
use chrono::Duration;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

pub const CODEX_POTTER_RELEASE_NOTES_URL: &str =
    "https://github.com/breezewish/CodexPotter/releases/latest";

const VERSION_FILENAME: &str = "version.json";
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/breezewish/CodexPotter/releases/latest";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateCheckResult {
    /// Latest version to show as a non-blocking notice (does not respect dismissal).
    pub upgrade_version: Option<String>,
    /// Latest version to show in a popup (respects dismissal).
    pub popup_version: Option<String>,
}

#[cfg(not(debug_assertions))]
pub fn check_for_updates(
    check_for_update_on_startup: bool,
    current_version: &str,
) -> UpdateCheckResult {
    if !check_for_update_on_startup {
        return UpdateCheckResult::default();
    }

    let version_file = match version_filepath() {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!("failed to resolve version cache path: {err}");
            return UpdateCheckResult::default();
        }
    };

    let info = read_version_info(&version_file).ok();

    if match &info {
        None => true,
        Some(info) => info.last_checked_at < Utc::now() - Duration::hours(20),
    } {
        // Refresh the cached latest version in the background so startup isn't blocked by a
        // network call.
        tokio::spawn({
            let version_file = version_file.clone();
            async move {
                check_for_update(&version_file)
                    .await
                    .inspect_err(|err| tracing::error!("Failed to update version: {err}"))
            }
        });
    }

    let upgrade_version = info.as_ref().and_then(|info| {
        if is_newer(&info.latest_version, current_version).unwrap_or(false) {
            Some(info.latest_version.clone())
        } else {
            None
        }
    });

    let popup_version = upgrade_version.clone().and_then(|version| {
        if info
            .as_ref()
            .and_then(|info| info.dismissed_version.as_deref())
            == Some(version.as_str())
        {
            None
        } else {
            Some(version)
        }
    });

    UpdateCheckResult {
        upgrade_version,
        popup_version,
    }
}

#[cfg(debug_assertions)]
pub fn check_for_updates(
    _check_for_update_on_startup: bool,
    _current_version: &str,
) -> UpdateCheckResult {
    UpdateCheckResult::default()
}

#[cfg(not(debug_assertions))]
pub async fn dismiss_version(version: &str) -> anyhow::Result<()> {
    let version_file = match version_filepath() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };

    let mut info = match read_version_info(&version_file) {
        Ok(info) => info,
        Err(_) => return Ok(()),
    };

    info.dismissed_version = Some(version.to_string());

    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

#[cfg(debug_assertions)]
pub async fn dismiss_version(_version: &str) -> anyhow::Result<()> {
    Ok(())
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct VersionInfo {
    latest_version: String,
    // ISO-8601 timestamp (RFC3339)
    last_checked_at: DateTime<Utc>,
    #[serde(default)]
    dismissed_version: Option<String>,
}

fn version_filepath() -> anyhow::Result<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        anyhow::bail!("cannot determine home directory for config path");
    };
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let base = xdg_config_home.unwrap_or_else(|| home.join(".config"));
    Ok(base.join("codexpotter").join(VERSION_FILENAME))
}

fn read_version_info(version_file: &Path) -> anyhow::Result<VersionInfo> {
    let contents = std::fs::read_to_string(version_file)?;
    Ok(serde_json::from_str(&contents)?)
}

#[derive(Deserialize, Debug, Clone)]
struct ReleaseInfo {
    tag_name: String,
}

#[cfg(not(debug_assertions))]
async fn check_for_update(version_file: &Path) -> anyhow::Result<()> {
    let ReleaseInfo { tag_name } = create_client()
        .get(LATEST_RELEASE_URL)
        .send()
        .await?
        .error_for_status()?
        .json::<ReleaseInfo>()
        .await?;

    let latest_version = extract_version_from_latest_tag(&tag_name)?;

    // Preserve any previously dismissed version if present.
    let prev_info = read_version_info(version_file).ok();
    let info = VersionInfo {
        latest_version,
        last_checked_at: Utc::now(),
        dismissed_version: prev_info.and_then(|p| p.dismissed_version),
    };

    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

#[cfg(not(debug_assertions))]
fn create_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!(
            "codex-potter/{version}",
            version = env!("CARGO_PKG_VERSION")
        ))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn extract_version_from_latest_tag(latest_tag_name: &str) -> anyhow::Result<String> {
    latest_tag_name
        .trim()
        .strip_prefix('v')
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse latest tag name '{latest_tag_name}'"))
}

fn is_newer(latest: &str, current: &str) -> Option<bool> {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => Some(l > c),
        _ => None,
    }
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut iter = v.trim().split('.');
    let maj = iter.next()?.parse::<u64>().ok()?;
    let min = iter.next()?.parse::<u64>().ok()?;
    let pat = iter.next()?.parse::<u64>().ok()?;
    Some((maj, min, pat))
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn extracts_version_from_latest_tag() {
        assert_eq!(
            extract_version_from_latest_tag("v1.5.0").expect("failed to parse version"),
            "1.5.0"
        );
    }

    #[test]
    fn latest_tag_without_prefix_is_invalid() {
        assert!(extract_version_from_latest_tag("1.5.0").is_err());
    }

    #[test]
    fn prerelease_version_is_not_considered_newer() {
        assert_eq!(is_newer("0.11.0-beta.1", "0.11.0"), None);
        assert_eq!(is_newer("1.0.0-rc.1", "1.0.0"), None);
    }

    #[test]
    fn plain_semver_comparisons_work() {
        assert_eq!(is_newer("0.11.1", "0.11.0"), Some(true));
        assert_eq!(is_newer("0.11.0", "0.11.1"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.9", "1.0.0"), Some(false));
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(parse_version(" 1.2.3 \n"), Some((1, 2, 3)));
        assert_eq!(is_newer(" 1.2.3 ", "1.2.2"), Some(true));
    }
}
