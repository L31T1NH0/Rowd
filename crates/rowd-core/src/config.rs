use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    #[default]
    Bidirectional,
    ToAndroid,
    ToPc,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemapPolicy {
    Pc,
    Android,
    Compare,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShareConfig {
    pub share_id: String,
    pub name: String,
    pub root: PathBuf,
    // Desktop-only when running on Linux. Android binds share_id to a SAF URI.
    pub android_path: String,
    pub mode: SyncMode,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub ignore: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remap_policy: Option<RemapPolicy>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShareRequestState {
    #[default]
    Pending,
    Accepted,
    Rejected,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareRequest {
    pub request_id: String,
    pub name: String,
    pub mode: SyncMode,
    #[serde(default)]
    pub state: ShareRequestState,
}
