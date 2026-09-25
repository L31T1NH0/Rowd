use anyhow::{ensure, Context, Result};
use rowd_core::{model::VERSION, storage::LocalStore, sync::State};
use serde::Deserialize;
use std::fs::File;

#[derive(Deserialize)]
pub(crate) struct LegacyConfig {
    version: u32,
    root: String,
    pub pair_id: String,
    pub folder_id: String,
    pub cert: String,
    pub key: String,
    pub secret: String,
}

pub(crate) fn load_config(store: &LocalStore) -> Result<LegacyConfig> {
    let config: LegacyConfig = serde_json::from_reader(
        File::open(store.private().join("server.json")).context("legacy server.json is missing")?,
    )?;
    ensure!(
        config.version == VERSION && config.root == store.root().to_string_lossy(),
        "folder moved: legacy configuration root mismatch"
    );
    Ok(config)
}

pub(crate) fn load_state(store: &LocalStore, config: &LegacyConfig) -> Result<State> {
    State::load(
        &store.private().join("sync-state.json"),
        &config.pair_id,
        &config.folder_id,
    )
}
