use anyhow::{ensure, Context, Result};
use rowd_core::{
    config::{ShareConfig, ShareRequest, SyncMode},
    model::validate_path,
    random_id,
    storage::atomic_json,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const CURRENT_DEVICE_VERSION: u32 = 3;

#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub version: u32,
    pub address: String,
    pub listen: String,
    pub pair_id: String,
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
    #[serde(default)]
    pub sync_paused: bool,
}

impl DeviceConfig {
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join(".rowd/device.json");
        let mut config: Self = serde_json::from_reader(
            File::open(&path).context("Pareie o dispositivo primeiro (rowd pair)")?,
        )?;
        match config.version {
            CURRENT_DEVICE_VERSION => {}
            2 => {
                backup_config(home, "migration-v2")?;
                config.version = CURRENT_DEVICE_VERSION;
                config.save(home)?;
            }
            version => anyhow::bail!("unsupported device configuration version {version}"),
        }
        Ok(config)
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        atomic_json(&home.join(".rowd/device.json"), self)
    }

    pub fn put_share(&mut self, home: &Path, mut share: ShareConfig) -> Result<()> {
        rowd_core::model::validate_hash(&share.share_id)?;
        if let Some(request_id) = &share.request_id {
            rowd_core::model::validate_hash(request_id)?;
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
            !share
                .root
                .components()
                .any(|component| component.as_os_str() == ".rowd"),
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
                    other.android_path == share.android_path || share.remap_policy.is_some(),
                    "choose a remap policy before changing the Android destination"
                );
                continue;
            }
            ensure!(
                !share.root.starts_with(&other.root) && !other.root.starts_with(&share.root),
                "overlapping Share roots: {}",
                other.name
            );
            let first = PathBuf::from(share.android_path.to_lowercase());
            let second = PathBuf::from(other.android_path.to_lowercase());
            ensure!(
                (share.android_path.is_empty() != other.android_path.is_empty())
                    || (!first.starts_with(&second) && !second.starts_with(&first)),
                "overlapping Android destinations: {}",
                other.name
            );
        }
        if !share.android_path.is_empty() {
            for legacy in self
                .shares
                .iter_mut()
                .filter(|item| item.android_path.is_empty())
            {
                let rule = format!("{}/", share.android_path);
                if !legacy.ignore.lines().any(|line| line == rule) {
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
            .find(|item| item.share_id == share.share_id)
        {
            *old = share;
        } else {
            ensure!(self.shares.len() < 256, "too many Shares");
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
                enabled: true,
                ignore: String::new(),
                request_id: None,
                remap_policy: None,
            },
        )?;
        Ok(id)
    }

    pub fn remove_share(&mut self, id: &str) -> Result<()> {
        ensure!(
            self.shares.iter().any(|share| share.share_id == id),
            "unknown Share"
        );
        self.shares.retain(|share| share.share_id != id);
        if !self.removed.iter().any(|removed| removed == id) {
            self.removed.push(id.into());
        }
        Ok(())
    }
}

pub fn backup_config(home: &Path, label: &str) -> Result<Option<PathBuf>> {
    let source = home.join(".rowd/device.json");
    if !source.exists() {
        return Ok(None);
    }
    ensure!(
        !label.is_empty()
            && label.len() <= 40
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "invalid backup label"
    );
    let directory = home.join(".rowd/config-backups");
    fs::create_dir_all(&directory)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let destination = directory.join(format!("{stamp}-{label}.json"));
    fs::copy(&source, &destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o600))?;
    }
    let mut backups = fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    backups.sort_by_key(|entry| entry.file_name());
    let remove_count = backups.len().saturating_sub(8);
    for entry in backups.into_iter().take(remove_count) {
        fs::remove_file(entry.path())?;
    }
    Ok(Some(destination))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DeviceConfig {
        DeviceConfig {
            version: CURRENT_DEVICE_VERSION,
            address: String::new(),
            listen: String::new(),
            pair_id: random_id().unwrap(),
            folder_id: random_id().unwrap(),
            cert: String::new(),
            key: String::new(),
            secret: String::new(),
            peer_device: None,
            shares: Vec::new(),
            removed: Vec::new(),
            share_requests: Vec::new(),
            sync_paused: false,
        }
    }

    #[test]
    fn shares_keep_identity_and_reject_overlaps() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("config");
        let pictures = directory.path().join("Pictures");
        let documents = directory.path().join("Documents");
        for path in [&home, &pictures.join("Camera"), &documents] {
            fs::create_dir_all(path).unwrap();
        }
        let mut config = config();
        let id = config
            .add_share(
                &home,
                "Fotos".into(),
                pictures.clone(),
                None,
                SyncMode::default(),
            )
            .unwrap();
        assert!(config
            .add_share(
                &home,
                "Camera".into(),
                pictures.join("Camera"),
                None,
                SyncMode::default(),
            )
            .is_err());
        config
            .add_share(&home, "Docs".into(), documents, None, SyncMode::default())
            .unwrap();
        config.save(&home).unwrap();
        let reloaded = DeviceConfig::load(&home).unwrap();
        assert_eq!(reloaded.shares.len(), 2);
        assert_eq!(reloaded.shares[0].share_id, id);
    }

    #[test]
    fn v2_configuration_migrates_and_is_backed_up() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        fs::create_dir_all(home.join(".rowd")).unwrap();
        let mut config = config();
        config.version = 2;
        atomic_json(&home.join(".rowd/device.json"), &config).unwrap();
        assert_eq!(
            DeviceConfig::load(home).unwrap().version,
            CURRENT_DEVICE_VERSION
        );
        assert_eq!(
            fs::read_dir(home.join(".rowd/config-backups"))
                .unwrap()
                .count(),
            1
        );
    }
}
