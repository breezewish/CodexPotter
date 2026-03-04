//! CodexPotter's project-level app-server.

pub mod client;
pub mod protocol;
pub mod server;

pub use client::PotterAppServerClient;
pub use protocol::*;
pub use server::PotterAppServerConfig;
pub use server::run_potter_app_server;
