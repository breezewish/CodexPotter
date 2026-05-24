//! Shared helpers for reconstructing session metadata during replay.
//!
//! Resume and project-level replay both need to synthesize a
//! [`SessionConfiguredEvent`](codex_protocol::protocol::SessionConfiguredEvent) from two sources:
//!
//! - Potter's own `potter-rollout.jsonl` metadata (`thread_id`, persisted `service_tier`)
//! - the upstream rollout JSONL (`cwd`, `model`, `model_provider`)
//!
//! Keeping this logic in one place ensures live resume and app-server replay stay aligned.

use std::io::BufRead as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ServiceTier;
use codex_protocol::protocol::SessionConfiguredEvent;

use crate::app_server::upstream_protocol::service_tier_from_value;

/// Resolve a recorded upstream rollout path against the original project working directory.
pub fn resolve_rollout_path_for_replay(workdir: &Path, rollout_path: &Path) -> PathBuf {
    if rollout_path.is_absolute() {
        return rollout_path.to_path_buf();
    }
    workdir.join(rollout_path)
}

/// Synthesize the session metadata needed for replay.
///
/// Potter's persisted `service_tier` takes precedence because it is the local replay source of
/// truth. The upstream rollout snapshot remains a compatibility fallback for older projects.
pub fn synthesize_session_configured_event(
    thread_id: ThreadId,
    service_tier: Option<ServiceTier>,
    rollout_path: PathBuf,
) -> anyhow::Result<Option<SessionConfiguredEvent>> {
    let Some(snapshot) = read_rollout_context_snapshot(&rollout_path)? else {
        return Ok(None);
    };

    Ok(Some(SessionConfiguredEvent {
        session_id: thread_id,
        forked_from_id: None,
        model: snapshot.model,
        model_provider_id: snapshot.model_provider_id,
        service_tier: service_tier.or(snapshot.service_tier),
        cwd: snapshot.cwd,
        reasoning_effort: None,
        history_log_id: 0,
        history_entry_count: 0,
        initial_messages: None,
        rollout_path,
    }))
}

struct RolloutContextSnapshot {
    cwd: PathBuf,
    model: String,
    model_provider_id: String,
    service_tier: Option<ServiceTier>,
}

fn read_rollout_context_snapshot(
    rollout_path: &Path,
) -> anyhow::Result<Option<RolloutContextSnapshot>> {
    let file = std::fs::File::open(rollout_path)
        .with_context(|| format!("open rollout {}", rollout_path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut cwd: Option<PathBuf> = None;
    let mut model: Option<String> = None;
    let mut model_provider_id: Option<String> = None;
    let mut service_tier: Option<ServiceTier> = None;

    for (idx, line) in reader.lines().enumerate() {
        let line_number = idx + 1;
        let line = line.with_context(|| format!("read rollout line {line_number}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("parse rollout json line {line_number}: {line}"))?;
        let Some(item_type) = value.get("type").and_then(serde_json::Value::as_str) else {
            continue;
        };
        match item_type {
            "turn_context" => {
                if cwd.is_some() && model.is_some() {
                    continue;
                }
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                if cwd.is_none()
                    && let Some(v) = payload.get("cwd")
                {
                    cwd = serde_json::from_value::<PathBuf>(v.clone()).ok();
                }
                if model.is_none() {
                    model = payload
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned);
                }
            }
            "session_meta" => {
                if model_provider_id.is_some() && service_tier.is_some() {
                    continue;
                }
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                if model_provider_id.is_none() {
                    model_provider_id = payload
                        .get("model_provider")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned);
                }
                if service_tier.is_none()
                    && let Some(v) = payload.get("service_tier")
                {
                    service_tier = service_tier_from_value(v);
                }
            }
            _ => {}
        }
    }

    let Some(cwd) = cwd else {
        return Ok(None);
    };
    let Some(model) = model else {
        return Ok(None);
    };

    Ok(Some(RolloutContextSnapshot {
        cwd,
        model,
        model_provider_id: model_provider_id.unwrap_or_default(),
        service_tier,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;

    #[test]
    fn resolve_rollout_path_for_replay_joins_relative_paths() {
        let workdir = Path::new("/tmp/project");
        assert_eq!(
            resolve_rollout_path_for_replay(workdir, Path::new("rollout.jsonl")),
            PathBuf::from("/tmp/project/rollout.jsonl")
        );
    }

    #[test]
    fn synthesize_session_configured_event_prefers_persisted_service_tier() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"2026-02-28T00:00:00.000Z","type":"turn_context","payload":{"cwd":"project","approval_policy":"never","sandbox_policy":{"type":"read_only"},"model":"test-model","summary":{"type":"auto"},"output_schema":null}}
{"timestamp":"2026-02-28T00:00:01.000Z","type":"session_meta","payload":{"model_provider":"openai","service_tier":"flex"}}
"#,
        )
        .expect("write rollout");

        let cfg = synthesize_session_configured_event(
            ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000").expect("thread id"),
            Some(ServiceTier::Fast),
            rollout_path.clone(),
        )
        .expect("synthesize")
        .expect("session configured");

        assert_eq!(cfg.model, "test-model");
        assert_eq!(cfg.model_provider_id, "openai");
        assert_eq!(cfg.service_tier, Some(ServiceTier::Fast));
        assert_eq!(cfg.cwd, PathBuf::from("project"));
        assert_eq!(cfg.rollout_path, rollout_path);
    }

    #[test]
    fn synthesize_session_configured_event_uses_snapshot_service_tier_for_compatibility() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"2026-02-28T00:00:00.000Z","type":"turn_context","payload":{"cwd":"project","approval_policy":"never","sandbox_policy":{"type":"read_only"},"model":"test-model","summary":{"type":"auto"},"output_schema":null}}
{"timestamp":"2026-02-28T00:00:01.000Z","type":"session_meta","payload":{"model_provider":"openai","service_tier":"flex"}}
"#,
        )
        .expect("write rollout");

        let cfg = synthesize_session_configured_event(
            ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000").expect("thread id"),
            None,
            rollout_path,
        )
        .expect("synthesize")
        .expect("session configured");

        assert_eq!(cfg.service_tier, Some(ServiceTier::Flex));
    }

    #[test]
    fn synthesize_session_configured_event_ignores_default_snapshot_service_tier() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            r#"{"timestamp":"2026-02-28T00:00:00.000Z","type":"turn_context","payload":{"cwd":"project","approval_policy":"never","sandbox_policy":{"type":"read_only"},"model":"test-model","summary":{"type":"auto"},"output_schema":null}}
{"timestamp":"2026-02-28T00:00:01.000Z","type":"session_meta","payload":{"model_provider":"openai","service_tier":"default"}}
"#,
        )
        .expect("write rollout");

        let cfg = synthesize_session_configured_event(
            ThreadId::from_string("019ca423-63d9-7641-ae83-db060ad3c000").expect("thread id"),
            None,
            rollout_path,
        )
        .expect("synthesize")
        .expect("session configured");

        assert_eq!(cfg.service_tier, None);
    }
}
