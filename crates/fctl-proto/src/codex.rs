//! Personal ChatGPT connections for Codex sessions.

use crate::{CredentialId, Millis, WorkspaceId};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

#[skip_serializing_none]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(optional_fields))]
pub struct CodexConnection {
    pub id: CredentialId,
    pub email: Option<String>,
    pub connected: bool,
    pub plan_enabled: bool,
    pub paused: bool,
    pub reset_at: Option<Millis>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CodexWorkspaceConnection {
    pub workspace_id: WorkspaceId,
    pub connection_id: CredentialId,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(optional_fields))]
pub struct CodexSettings {
    pub enabled: bool,
    pub connections: Vec<CodexConnection>,
    pub default_connection_id: Option<CredentialId>,
    pub workspace_connections: Vec<CodexWorkspaceConnection>,
}

#[skip_serializing_none]
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(optional_fields))]
pub struct CodexLoginStartRequest {
    pub redirect_uri: String,
    pub connection_id: Option<CredentialId>,
    /// Request consent again for a saved registration.
    pub enable_plan: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CodexLoginStart {
    pub authorize_url: String,
    pub state: String,
}

#[skip_serializing_none]
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(optional_fields))]
pub struct CodexLoginFinishRequest {
    pub state: String,
    pub code: Option<String>,
    /// The issued client ID, required after a new registration.
    pub client_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CodexSelection {
    /// Null clears the selection.
    pub connection_id: Option<CredentialId>,
}
