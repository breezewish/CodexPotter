//! Upstream app-server protocol v1 payloads.
//!
//! This module contains the request/response payloads for the initial `initialize` request and
//! the approval response payloads used by certain server-initiated requests.

use codex_protocol::protocol::ReviewDecision;
use serde::Deserialize;
use serde::Serialize;

/// Parameters for the `initialize` request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
}

/// Identifies the client for display/telemetry purposes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
}

/// Response payload for an `applyPatch` approval request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPatchApprovalResponse {
    pub decision: ReviewDecision,
}

/// Response payload for an `execCommand` approval request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ExecCommandApprovalResponse {
    pub decision: ReviewDecision,
}
