//! App-server related modules.
//!
//! This contains both:
//! - the upstream `codex app-server` backend driver (one process per round)
//! - the long-lived `codex-potter app-server` implementation (project control plane)

pub mod codex_backend;
pub mod potter;
pub mod stream_recovery;
pub mod upstream_protocol;

pub use codex_backend::AppServerBackendConfig;
pub use codex_backend::AppServerEventMode;
pub use codex_backend::AppServerLaunchConfig;
pub use codex_backend::run_app_server_backend;

fn sandbox_mode_cli_arg(mode: upstream_protocol::SandboxMode) -> &'static str {
    match mode {
        upstream_protocol::SandboxMode::ReadOnly => "read-only",
        upstream_protocol::SandboxMode::WorkspaceWrite => "workspace-write",
        upstream_protocol::SandboxMode::DangerFullAccess => "danger-full-access",
    }
}
