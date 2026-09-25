use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use rowd_core::{
    config::{ShareConfig, ShareRequest, SyncMode},
    random_id,
    storage::atomic_json,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const CURRENT_DEVICE_VERSION: u32 = 8;

#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub version: u32,
    pub address: String,
    pub listen: String,
    pub pair_id: String,
    pub cert: String,
    pub key: String,
    pub secret: String,
    #[serde(default, alias = "peer_root")]
    pub peer_device: Option<String>,
    pub shares: Vec<ShareConfig>,
    #[serde(default)]
    pub share_requests: Vec<ShareRequest>,
    #[serde(default)]
    pub rejected_requests: Vec<ShareRequest>,
    #[serde(default)]
    pub sync_paused: bool,
    #[serde(default)]
    pub pending_unlink: bool,
    #[serde(default)]
    pub import_generation: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ImportTransaction {
    pub generation: String,
    pub had_state: bool,
}

pub(crate) fn import_transaction_path(home: &Path) -> PathBuf {
    home.join(".rowd/import-transaction.json")
}

pub(crate) fn recover_import(home: &Path) -> Result<()> {
    let marker = import_transaction_path(home);
    if !marker.exists() {
        return Ok(());
    }
    let transaction: ImportTransaction = serde_json::from_reader(File::open(&marker)?)?;
    rowd_core::model::validate_hash(&transaction.generation)?;
    let state = home.join(".rowd/shares");
    let archived = home
        .join(".rowd")
        .join(format!("shares-before-import-{}", transaction.generation));
    let configured: DeviceConfig =
        serde_json::from_reader(File::open(home.join(".rowd/device.json"))?)?;
    if configured.import_generation.as_deref() == Some(&transaction.generation) {
        ensure!(state.is_dir(), "imported Share state is missing");
    } else if archived.exists() {
        if state.exists() {
            fs::rename(
                &state,
                home.join(".rowd")
                    .join(format!("shares-aborted-import-{}", transaction.generation)),
            )?;
        }
        fs::rename(&archived, &state)?;
    } else if transaction.had_state {
        ensure!(state.is_dir(), "previous Share state is missing");
    } else if state.exists() {
        fs::rename(
            &state,
            home.join(".rowd")
                .join(format!("shares-aborted-import-{}", transaction.generation)),
        )?;
    }
    fs::remove_file(marker)?;
    #[cfg(unix)]
    File::open(home.join(".rowd"))?.sync_all()?;
    Ok(())
}

fn migrate_legacy_ignore(root: &Path, legacy: &str) -> Result<()> {
    rowd_core::ignore::Ignore::validate(legacy)?;
    ensure!(
        !fs::symlink_metadata(root)?.file_type().is_symlink(),
        "Share root changed during ignore migration"
    );
    let path = root.join(".rowdignore");
    match fs::symlink_metadata(&path) {
        Ok(meta) => ensure!(
            !meta.file_type().is_symlink(),
            "symlink .rowdignore cannot be migrated"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let current = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    rowd_core::ignore::Ignore::validate(&current)?;
    let mut combined = current.clone();
    let mut known = current
        .lines()
        .map(str::trim)
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    for rule in legacy
        .lines()
        .map(str::trim)
        .filter(|rule| !rule.is_empty() && !rule.starts_with('#'))
    {
        if known.insert(rule.to_owned()) {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(rule);
            combined.push('\n');
        }
    }
    rowd_core::ignore::Ignore::validate(&combined)?;
    if combined != current {
        let private = root.join(".rowd");
        if private.exists() {
            ensure!(
                !fs::symlink_metadata(&private)?.file_type().is_symlink(),
                "symlink metadata directory"
            );
        }
        let backup_dir = private.join("ignore-backups");
        match fs::symlink_metadata(&backup_dir) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "symlink ignore backup directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let backup = backup_dir.join(format!(
            "before-migration-{}.txt",
            hex::encode(Sha256::digest(current.as_bytes()))
        ));
        if path.exists() && !backup.exists() {
            rowd_core::storage::atomic_write(&backup, current.as_bytes())?;
        }
        rowd_core::storage::atomic_write(&path, combined.as_bytes())?;
    }
    Ok(())
}

impl DeviceConfig {
    pub fn load(home: &Path) -> Result<Self> {
        if import_transaction_path(home).exists() {
            let locks = home.join(".rowd-locks");
            fs::create_dir_all(&locks)?;
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(locks.join("config.lock"))?;
            lock.try_lock_exclusive()
                .context("backup import in progress")?;
            recover_import(home)?;
        }
        let path = home.join(".rowd/device.json");
        let mut raw: serde_json::Value = serde_json::from_reader(
            File::open(&path).context("Pareie o dispositivo primeiro (rowd pair)")?,
        )?;
        let version = raw
            .get("version")
            .and_then(|value| value.as_u64())
            .context("invalid device configuration version")?;
        let mut requests_migrated = false;
        if version <= 7 {
            let mut pending = Vec::new();
            let mut rejected = raw
                .get("rejected_requests")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            let requests = raw
                .as_object_mut()
                .context("invalid device configuration")?
                .entry("share_requests")
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .context("invalid Share request list")?;
            for mut request in std::mem::take(requests) {
                let state = request
                    .as_object_mut()
                    .context("invalid Share request")?
                    .remove("state");
                if state.is_some() {
                    requests_migrated = true;
                }
                match state.as_ref().and_then(|value| value.as_str()).unwrap_or("pending") {
                    "pending" => pending.push(request),
                    "rejected" => rejected.push(request),
                    _ => anyhow::bail!("ambiguous legacy Share request state; resolve it with the original Rowd version"),
                }
            }
            raw["share_requests"] = serde_json::Value::Array(pending);
            raw["rejected_requests"] = serde_json::Value::Array(rejected);
        }
        let mut config: Self = serde_json::from_value(raw)?;
        match config.version {
            CURRENT_DEVICE_VERSION | 2..=7 => {
                let previous_version = config.version;
                let mut changed = requests_migrated;
                for share in &mut config.shares {
                    if share.remap_policy.is_some() && share.binding_revision == 0 {
                        share.binding_revision = 1;
                        changed = true;
                    }
                    if !share.legacy_ignore.is_empty() && share.root.is_dir() {
                        migrate_legacy_ignore(&share.root, &share.legacy_ignore)?;
                        share.legacy_ignore.clear();
                        changed = true;
                    }
                }
                if config
                    .shares
                    .iter()
                    .all(|share| share.legacy_ignore.is_empty())
                {
                    config.version = CURRENT_DEVICE_VERSION;
                    changed |= previous_version != CURRENT_DEVICE_VERSION;
                }
                if changed {
                    backup_config(home, &format!("migration-v{previous_version}"))?;
                    config.save(home)?;
                }
            }
            version => anyhow::bail!("unsupported device configuration version {version}"),
        }
        Ok(config)
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        if self.version == CURRENT_DEVICE_VERSION {
            ensure!(
                self.shares
                    .iter()
                    .all(|share| share.legacy_ignore.is_empty()),
                "legacy ignore rules require migration before saving"
            );
        }
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
                continue;
            }
            ensure!(
                !share.root.starts_with(&other.root) && !other.root.starts_with(&share.root),
                "overlapping Share roots: {}",
                other.name
            );
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
        mode: SyncMode,
    ) -> Result<String> {
        let id = random_id()?;
        self.put_share(
            home,
            ShareConfig {
                share_id: id.clone(),
                name,
                root,
                binding_revision: 0,
                mode,
                enabled: true,
                legacy_ignore: String::new(),
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
            cert: String::new(),
            key: String::new(),
            secret: String::new(),
            peer_device: None,
            shares: Vec::new(),
            share_requests: Vec::new(),
            rejected_requests: Vec::new(),
            sync_paused: false,
            pending_unlink: false,
            import_generation: None,
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
            .add_share(&home, "Fotos".into(), pictures.clone(), SyncMode::default())
            .unwrap();
        assert!(config
            .add_share(
                &home,
                "Camera".into(),
                pictures.join("Camera"),
                SyncMode::default(),
            )
            .is_err());
        config
            .add_share(&home, "Docs".into(), documents, SyncMode::default())
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

    #[test]
    fn legacy_ignore_migrates_once_and_waits_for_an_offline_root() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let root = directory.path().join("share");
        fs::create_dir_all(home.join(".rowd")).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".rowdignore"), "existing/\n").unwrap();
        let mut config = config();
        config.version = 6;
        config
            .add_share(&home, "Docs".into(), root.clone(), SyncMode::Bidirectional)
            .unwrap();
        config.shares[0].legacy_ignore = "secret/\n*.tmp\n".into();
        config.save(&home).unwrap();
        let unavailable = directory.path().join("temporarily-offline");
        fs::rename(&root, &unavailable).unwrap();
        let waiting = DeviceConfig::load(&home).unwrap();
        assert_eq!(waiting.version, 6);
        assert!(!waiting.shares[0].legacy_ignore.is_empty());
        fs::rename(&unavailable, &root).unwrap();
        let migrated = DeviceConfig::load(&home).unwrap();
        assert_eq!(migrated.version, CURRENT_DEVICE_VERSION);
        assert!(migrated.shares[0].legacy_ignore.is_empty());
        let policy = fs::read_to_string(root.join(".rowdignore")).unwrap();
        assert_eq!(policy, "existing/\nsecret/\n*.tmp\n");
        assert_eq!(
            fs::read_dir(root.join(".rowd/ignore-backups"))
                .unwrap()
                .count(),
            1
        );
        // A crash after writing .rowdignore but before device.json is safe to retry.
        config.save(&home).unwrap();
        DeviceConfig::load(&home).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".rowdignore")).unwrap(),
            policy
        );
    }

    #[test]
    #[cfg(unix)]
    fn ignore_migration_rejects_symlinked_policy_and_backup_directory() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("share");
        let outside = directory.path().join("outside");
        fs::create_dir_all(root.join(".rowd")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("policy"), "private/\n").unwrap();
        symlink(outside.join("policy"), root.join(".rowdignore")).unwrap();
        assert!(migrate_legacy_ignore(&root, "*.tmp\n").is_err());
        assert_eq!(
            fs::read_to_string(outside.join("policy")).unwrap(),
            "private/\n"
        );
        fs::remove_file(root.join(".rowdignore")).unwrap();
        fs::write(root.join(".rowdignore"), "existing/\n").unwrap();
        symlink(&outside, root.join(".rowd/ignore-backups")).unwrap();
        assert!(migrate_legacy_ignore(&root, "*.tmp\n").is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[test]
    fn rejected_request_migrates_to_temporary_payload_tombstone() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        fs::create_dir_all(home.join(".rowd")).unwrap();
        let mut config = config();
        config.version = 7;
        config.share_requests.push(ShareRequest {
            request_id: random_id().unwrap(),
            name: "Camera".into(),
            mode: SyncMode::ToPc,
        });
        let mut old = serde_json::to_value(&config).unwrap();
        old["share_requests"][0]["state"] = serde_json::json!("rejected");
        atomic_json(&home.join(".rowd/device.json"), &old).unwrap();
        let migrated = DeviceConfig::load(home).unwrap();
        assert_eq!(migrated.version, CURRENT_DEVICE_VERSION);
        assert!(migrated.share_requests.is_empty());
        assert_eq!(migrated.rejected_requests[0].name, "Camera");
        old["share_requests"][0]["state"] = serde_json::json!("accepted");
        atomic_json(&home.join(".rowd/device.json"), &old).unwrap();
        assert!(DeviceConfig::load(home)
            .err()
            .unwrap()
            .to_string()
            .contains("ambiguous legacy"));
    }
}
