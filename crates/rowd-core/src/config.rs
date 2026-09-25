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
    #[serde(default)]
    pub binding_revision: u64,
    pub mode: SyncMode,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    // V1-V6 migration input only. New configurations use root/.rowdignore.
    #[serde(default, rename = "ignore", skip_serializing_if = "String::is_empty")]
    pub legacy_ignore: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remap_policy: Option<RemapPolicy>,
}

/// PC-owned Share definition sent to Android. Local desktop paths never cross the wire.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareDefinition {
    pub share_id: String,
    pub name: String,
    pub mode: SyncMode,
    pub enabled: bool,
    #[serde(default)]
    pub binding_revision: u64,
    pub ignore_rules: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remap_policy: Option<RemapPolicy>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareRequest {
    pub request_id: String,
    pub name: String,
    pub mode: SyncMode,
}
