//! Protocol version modules for upstream `codex app-server`.
//!
//! The upstream app-server JSON-RPC schema is versioned. This module splits the payload types
//! into:
//! - [`common`]: "method" enums and shared JSON-RPC shapes
//! - [`v1`]: initialization + approval response payloads
//! - [`v2`]: thread/turn control plane payloads
//!
//! [`crate::app_server::upstream_protocol`] re-exports the pieces CodexPotter uses.

pub mod common;
pub mod v1;
pub mod v2;
