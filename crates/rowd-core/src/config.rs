use crate::{model::validate_path, random_id, storage::atomic_json};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    #[default]
    Bidirectional,
    ToAndroid,
    ToPc,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShareConfig {
    pub share_id: String,
    pub name: String,
    pub root: PathBuf,
    // Compatibility hint for the local device simulator. Android binds each
    // share_id to its own private SAF URI and never derives a folder from this.
    pub android_path: String,
    pub mode: SyncMode,
    #[serde(default)]
    pub ignore: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareRequest {
    pub request_id: String,
    pub name: String,
    pub mode: SyncMode,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub version: u32,
    pub address: String,
    pub listen: String,
    pub pair_id: String,
    // The V1 folder identity becomes the device's authentication namespace.
    pub folder_id: String,
    pub cert: String,
    pub key: String,
    pub secret: String,
    #[serde(default, alias = "peer_root")]
    pub peer_device: Option<String>,
    pub shares: Vec<ShareConfig>,
    pub removed: Vec<String>,
    #[serde(default)]
    pub share_requests: Vec<ShareRequest>,
}

impl DeviceConfig {
    pub fn load(home: &Path) -> Result<Self> {
        let cfg: Self = serde_json::from_reader(
            File::open(home.join(".rowd/device.json"))
                .context("Pareie o dispositivo primeiro (rowd pair)")?,
        )?;
        ensure!(cfg.version == 2, "unsupported device configuration");
        Ok(cfg)
    }
    pub fn save(&self, home: &Path) -> Result<()> {
        atomic_json(&home.join(".rowd/device.json"), self)
    }
    pub fn put_share(&mut self, home: &Path, mut share: ShareConfig) -> Result<()> {
        crate::model::validate_hash(&share.share_id)?;
        if let Some(request_id) = &share.request_id {
            crate::model::validate_hash(request_id)?;
        }
        ensure!(
            !share.name.trim().is_empty()
                && share.name.len() <= 120
                && !share.name.chars().any(char::is_control),
            "invalid Share name"
        );
        if !share.android_path.is_empty() {
            validate_path(&share.android_path)?;
        }
        share.root = share
            .root
            .canonicalize()
            .context("Share directory does not exist")?;
        ensure!(share.root.is_dir(), "Share root must be a directory");
        ensure!(
            !share.root.components().any(|c| c.as_os_str() == ".rowd"),
            "internal directory cannot be shared"
        );
        let home = home.canonicalize()?;
        ensure!(
            !share.root.starts_with(&home) && !home.starts_with(&share.root),
            "Share overlaps Rowd configuration"
        );
        for other in &self.shares {
            if other.share_id == share.share_id {
                ensure!(
                    other.android_path == share.android_path,
                    "Android destination is immutable; remove the Share before remapping"
                );
                continue;
            }
            ensure!(
                !share.root.starts_with(&other.root) && !other.root.starts_with(&share.root),
                "overlapping Share roots: {}",
                other.name
            );
            let a = PathBuf::from(share.android_path.to_lowercase());
            let b = PathBuf::from(other.android_path.to_lowercase());
            ensure!(
                (share.android_path.is_empty() != other.android_path.is_empty())
                    || (!a.starts_with(&b) && !b.starts_with(&a)),
                "overlapping Android destinations: {}",
                other.name
            );
        }
        // A migrated V1 root keeps its files. New managed subdirectories are
        // permanently excluded from that legacy Share, even after unlinking.
        if !share.android_path.is_empty() {
            for legacy in self.shares.iter_mut().filter(|s| s.android_path.is_empty()) {
                let rule = format!("{}/", share.android_path);
                if !legacy.ignore.lines().any(|l| l == rule) {
                    ensure!(
                        !legacy.root.join(&share.android_path).exists(),
                        "destination collides with legacy content"
                    );
                    legacy.ignore.push_str(&format!("\n{rule}"));
                }
            }
        }
        if let Some(old) = self
            .shares
            .iter_mut()
            .find(|s| s.share_id == share.share_id)
        {
            *old = share;
        } else {
            self.shares.push(share);
        }
        Ok(())
    }
    pub fn add_share(
        &mut self,
        home: &Path,
        name: String,
        root: PathBuf,
        android_path: Option<String>,
        mode: SyncMode,
    ) -> Result<String> {
        let id = random_id()?;
        let android_path = android_path.unwrap_or_else(|| name.clone());
        ensure!(!android_path.is_empty(), "Android destination is required");
        self.put_share(
            home,
            ShareConfig {
                share_id: id.clone(),
                name,
                root,
                android_path,
                mode,
                ignore: String::new(),
                request_id: None,
            },
        )?;
        self.save(home)?;
        Ok(id)
    }
    pub fn remove_share(&mut self, home: &Path, id: &str) -> Result<()> {
        ensure!(
            self.shares.iter().any(|s| s.share_id == id),
            "unknown Share"
        );
        self.shares.retain(|s| s.share_id != id);
        self.removed.push(id.into());
        self.save(home)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shares_keep_identity_and_reject_overlaps() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("config");
        let a = d.path().join("Pictures");
        let b = d.path().join("Documents");
        for p in [&home, &a.join("Camera"), &b] {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut cfg = DeviceConfig {
            version: 2,
            address: String::new(),
            listen: String::new(),
            pair_id: random_id().unwrap(),
            folder_id: random_id().unwrap(),
            cert: String::new(),
            key: String::new(),
            secret: String::new(),
            peer_device: None,
            shares: vec![],
            removed: vec![],
            share_requests: vec![],
        };
        let id = cfg
            .add_share(&home, "Fotos".into(), a.clone(), None, SyncMode::default())
            .unwrap();
        assert!(cfg
            .add_share(
                &home,
                "Camera".into(),
                a.join("Camera"),
                None,
                SyncMode::default()
            )
            .is_err());
        cfg.add_share(&home, "Docs".into(), b, None, SyncMode::default())
            .unwrap();
        let mut share = cfg.shares[0].clone();
        share.name = "Imagens".into();
        cfg.put_share(&home, share).unwrap();
        cfg.save(&home).unwrap();
        let reloaded = DeviceConfig::load(&home).unwrap();
        assert_eq!(reloaded.shares.len(), 2);
        assert_eq!(reloaded.shares[0].share_id, id);
        assert_eq!(reloaded.shares[0].android_path, "Fotos");
    }

    #[test]
    fn device_identity_migrates_from_the_old_root_field() {
        let id = random_id().unwrap();
        let mut value = serde_json::json!({
            "version": 2,
            "address": "",
            "listen": "",
            "pair_id": random_id().unwrap(),
            "folder_id": random_id().unwrap(),
            "cert": "",
            "key": "",
            "secret": random_id().unwrap(),
            "peer_root": id,
            "shares": [],
            "removed": [],
            "share_requests": []
        });
        let config: DeviceConfig = serde_json::from_value(value.take()).unwrap();
        assert_eq!(config.peer_device.as_deref(), Some(id.as_str()));
        assert!(serde_json::to_value(config)
            .unwrap()
            .get("peer_root")
            .is_none());
    }
}
