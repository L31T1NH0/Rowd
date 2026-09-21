mod config;

pub use config::{backup_config, DeviceConfig, CURRENT_DEVICE_VERSION};

use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use rowd_core::{
    config::{RemapPolicy, ShareConfig, ShareRequest, ShareRequestState, SyncMode},
    journal::{ShareState, TrackedStore},
    model::{Invitation, VERSION},
    protocol::{self, Message},
    random_id,
    storage::{atomic_json, LocalStore, Store},
    sync::{self, State},
    tls,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    num::NonZeroU32,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct App {
    home: PathBuf,
}

impl App {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    fn config(&self) -> Result<DeviceConfig> {
        DeviceConfig::load(&self.home)
    }

    pub fn pair(&self, address: &str) -> Result<()> {
        pair(&self.home, address).map(|_| ())
    }

    pub fn pairing_info(&self) -> Result<DeviceInfo> {
        device_info(&self.home)
    }

    pub fn export_invitation(&self, output: &Path) -> Result<()> {
        let config = DeviceConfig::load(&self.home)?;
        save_private(output, &invitation(&config))
    }

    pub fn device_summary(&self) -> Result<DeviceSummary> {
        device_summary(&self.home)
    }

    pub fn share_requests(&self) -> Result<Vec<ShareRequest>> {
        share_requests(&self.home)
    }

    pub fn status(&self) -> Result<Vec<Status>> {
        status(&self.home)
    }

    pub fn add_share(
        &self,
        name: String,
        root: PathBuf,
        android_path: Option<String>,
        mode: SyncMode,
    ) -> Result<String> {
        let mut id = String::new();
        update(&self.home, |cfg| {
            id = cfg.add_share(&self.home, name, root, android_path, mode)?;
            Ok(())
        })?;
        Ok(id)
    }

    pub fn edit_share(&self, id: &str, name: String, root: PathBuf, mode: SyncMode) -> Result<()> {
        update(&self.home, |cfg| {
            let mut share = cfg
                .shares
                .iter()
                .find(|share| share.share_id == id)
                .context("unknown Share")?
                .clone();
            share.name = name;
            share.root = root;
            share.mode = mode;
            cfg.put_share(&self.home, share)
        })
    }

    pub fn patch_share(
        &self,
        id: &str,
        name: Option<String>,
        root: Option<PathBuf>,
        mode: Option<SyncMode>,
    ) -> Result<()> {
        update(&self.home, |cfg| {
            let mut share = cfg
                .shares
                .iter()
                .find(|share| share.share_id == id)
                .context("unknown Share")?
                .clone();
            if let Some(name) = name {
                share.name = name;
            }
            if let Some(root) = root {
                share.root = root;
            }
            if let Some(mode) = mode {
                share.mode = mode;
            }
            cfg.put_share(&self.home, share)
        })
    }

    pub fn remove_share(&self, id: &str) -> Result<()> {
        update(&self.home, |cfg| cfg.remove_share(id))
    }

    pub fn set_share_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        update(&self.home, |cfg| {
            let share = cfg
                .shares
                .iter_mut()
                .find(|share| share.share_id == id)
                .context("unknown Share")?;
            share.enabled = enabled;
            Ok(())
        })
    }

    pub fn set_sync_paused(&self, paused: bool) -> Result<()> {
        update(&self.home, |cfg| {
            cfg.sync_paused = paused;
            Ok(())
        })
    }

    pub fn accept_share_request(&self, request_id: &str, folder: &Path) -> Result<()> {
        accept_share_request(&self.home, request_id, folder)
    }

    pub fn reject_share_request(&self, request_id: &str) -> Result<()> {
        reject_share_request(&self.home, request_id)
    }

    pub fn reindex_share(&self, id: &str) -> Result<()> {
        reindex_share(&self.home, id)
    }

    pub fn remap_share(&self, id: &str, android_path: String, policy: RemapPolicy) -> Result<()> {
        remap_share(&self.home, id, android_path, policy)
    }

    pub fn ignore_text(&self, id: &str) -> Result<String> {
        let cfg = self.config()?;
        let share = cfg
            .shares
            .iter()
            .find(|share| share.share_id == id)
            .context("unknown Share")?;
        Ok(fs::read_to_string(share.root.join(".rowdignore")).unwrap_or_default())
    }

    pub fn set_ignore_text(&self, id: &str, text: &str) -> Result<()> {
        set_ignore_text(&self.home, id, text)
    }

    pub fn request_share_sync(&self, id: &str) -> Result<()> {
        request_share_sync(&self.home, id)
    }

    pub fn unlink_device(&self) -> Result<()> {
        unlink_device(&self.home)
    }

    pub fn connection_test(&self) -> Result<ConnectionTest> {
        test_connection(&self.home)
    }

    pub fn recovery(&self) -> Result<Vec<RecoveryItem>> {
        recovery(&self.home)
    }

    pub fn restore_recovery(&self, share_id: &str, id: &str) -> Result<()> {
        resolve_recovery(&self.home, share_id, id, "restore", None)
    }

    pub fn keep_recovery(&self, share_id: &str, id: &str) -> Result<()> {
        resolve_recovery(&self.home, share_id, id, "keep", None)
    }

    pub fn export_recovery(&self, share_id: &str, id: &str, output: &Path) -> Result<()> {
        resolve_recovery(&self.home, share_id, id, "export", Some(output))
    }

    pub fn cleanup_recovery(&self, share_id: &str, id: &str) -> Result<()> {
        cleanup_recovery(&self.home, share_id, id)
    }

    pub fn export_profile(&self, output: &Path) -> Result<()> {
        export_profile(&self.home, output)
    }

    pub fn import_profile(&self, input: &Path) -> Result<()> {
        import_profile(&self.home, input)
    }

    pub fn export_backup(&self, output: &Path, passphrase: &str) -> Result<()> {
        export_backup(&self.home, output, passphrase)
    }

    pub fn import_backup(&self, input: &Path, passphrase: &str) -> Result<()> {
        import_backup(&self.home, input, passphrase)
    }

    pub fn export_diagnostic(&self, output: &Path) -> Result<()> {
        export_diagnostic(&self.home, output)
    }

    pub fn reset(&self, level: ResetLevel) -> Result<()> {
        reset(&self.home, level)
    }

    pub fn load_ui_preferences<T: serde::de::DeserializeOwned>(&self) -> Result<Option<T>> {
        let path = self.home.join(".rowd/ui.json");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_reader(File::open(path)?)?))
    }

    pub fn save_ui_preferences(&self, value: &impl Serialize) -> Result<()> {
        atomic_json(&self.home.join(".rowd/ui.json"), value)
    }
}

#[derive(Clone, Debug)]
pub enum ResetLevel {
    Interface,
    Share(String),
    Unlink,
    Initial,
    AllData,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    pub paired: bool,
    pub address: String,
    pub identity: String,
    pub fingerprint: String,
    pub qr: String,
    pub qr_image: PathBuf,
    pub sync_paused: bool,
    pub last_connection: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceSummary {
    pub configured: bool,
    pub paired: bool,
    pub address: String,
    pub identity: String,
    pub fingerprint: String,
    pub sync_paused: bool,
    pub last_connection: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredError {
    pub at: u64,
    pub operation: String,
    pub message: String,
    pub resolved_at: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ShareRuntime {
    last_error: Option<StoredError>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DeviceRuntime {
    last_connection: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecoveryItem {
    pub share_id: String,
    pub share_name: String,
    pub id: String,
    pub path: String,
    pub finished: bool,
    pub backup_available: bool,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConnectionCheck {
    pub label: &'static str,
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConnectionTest {
    pub checks: Vec<ConnectionCheck>,
    pub available_shares: usize,
    pub total_shares: usize,
}

#[derive(Serialize, Deserialize)]
struct ProfileShare {
    share_id: String,
    name: String,
    root: PathBuf,
    android_path: String,
    mode: SyncMode,
    enabled: bool,
    ignore: String,
}

#[derive(Serialize, Deserialize)]
struct PublicProfile {
    version: u32,
    shares: Vec<ProfileShare>,
    sync_paused: bool,
    ui: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct FullBackup {
    version: u32,
    config: DeviceConfig,
    ui: Option<serde_json::Value>,
    state: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct EncryptedBackup {
    format: u32,
    salt: String,
    nonce: String,
    ciphertext: String,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn runtime_path(home: &Path, share_id: &str) -> PathBuf {
    home.join(".rowd/shares")
        .join(share_id)
        .join("runtime.json")
}

fn device_runtime_path(home: &Path) -> PathBuf {
    home.join(".rowd/device-runtime.json")
}

fn session_guard(home: &Path) -> Result<File> {
    let directory = home.join(".rowd");
    fs::create_dir_all(&directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("session.lock"))?;
    lock.lock_exclusive()?;
    Ok(lock)
}

fn config_guard(home: &Path) -> Result<File> {
    let directory = home.join(".rowd");
    fs::create_dir_all(&directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("config.lock"))?;
    lock.lock_exclusive()?;
    Ok(lock)
}

fn device_runtime(home: &Path) -> DeviceRuntime {
    File::open(device_runtime_path(home))
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_default()
}

fn record_share_error(home: &Path, share_id: &str, operation: &str, error: &anyhow::Error) {
    let runtime = ShareRuntime {
        last_error: Some(StoredError {
            at: now(),
            operation: operation.into(),
            message: format!("{error:#}"),
            resolved_at: None,
        }),
    };
    let _ = atomic_json(&runtime_path(home, share_id), &runtime);
}

fn resolve_share_error(home: &Path, share_id: &str) {
    let path = runtime_path(home, share_id);
    let Ok(file) = File::open(&path) else { return };
    let Ok(mut runtime) = serde_json::from_reader::<_, ShareRuntime>(file) else {
        return;
    };
    if let Some(error) = &mut runtime.last_error {
        error.resolved_at = Some(now());
        let _ = atomic_json(&path, &runtime);
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(!path.exists(), "destination already exists");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct LegacyConfig {
    version: u32,
    root: String,
    pair_id: String,
    folder_id: String,
    cert: String,
    key: String,
    secret: String,
}

fn legacy_config(store: &LocalStore) -> Result<LegacyConfig> {
    let cfg: LegacyConfig = serde_json::from_reader(
        fs::File::open(store.private().join("server.json")).context("execute rowd init first")?,
    )?;
    ensure!(
        cfg.version == VERSION && cfg.root == store.root().to_string_lossy(),
        "folder moved: configuration root mismatch"
    );
    Ok(cfg)
}

pub fn default_home() -> PathBuf {
    std::env::var_os("ROWD_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/rowd")
        })
}
pub fn invitation(cfg: &DeviceConfig) -> Invitation {
    Invitation {
        version: VERSION,
        address: cfg.address.clone(),
        pair_id: cfg.pair_id.clone(),
        folder_id: cfg.folder_id.clone(),
        cert_der: cfg.cert.clone(),
        secret: cfg.secret.clone(),
    }
}

pub fn pairing_fingerprint(cfg: &DeviceConfig) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(Sha256::digest(hex::decode(&cfg.cert)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

pub fn device_info(home: &Path) -> Result<DeviceInfo> {
    let config = DeviceConfig::load(home)?;
    Ok(DeviceInfo {
        paired: config.peer_device.is_some(),
        address: config.address.clone(),
        identity: config
            .peer_device
            .clone()
            .unwrap_or_else(|| "aguardando primeiro vínculo".into()),
        fingerprint: pairing_fingerprint(&config)?,
        qr: qr(&config)?,
        qr_image: export_qr(home, &config)?,
        sync_paused: config.sync_paused,
        last_connection: device_runtime(home).last_connection,
    })
}

pub fn device_summary(home: &Path) -> Result<DeviceSummary> {
    let path = home.join(".rowd/device.json");
    if !path.exists() {
        return Ok(DeviceSummary {
            configured: false,
            paired: false,
            address: "não configurado".into(),
            identity: "aguardando configuração".into(),
            fingerprint: "indisponível".into(),
            sync_paused: false,
            last_connection: None,
        });
    }
    let config = DeviceConfig::load(home)?;
    let fingerprint = pairing_fingerprint(&config)?;
    let last_connection = device_runtime(home).last_connection;
    Ok(DeviceSummary {
        configured: true,
        paired: config.peer_device.is_some(),
        address: config.address,
        identity: config
            .peer_device
            .unwrap_or_else(|| "aguardando primeiro vínculo".into()),
        fingerprint,
        sync_paused: config.sync_paused,
        last_connection,
    })
}

/// Keep the server wildcard bind private; invitations must contain an address
/// reachable from the Android device on the local network.
pub fn published_address(address: &str) -> Result<String> {
    let socket: std::net::SocketAddr = address
        .parse()
        .with_context(|| format!("invalid address: {address}"))?;
    if !socket.ip().is_unspecified() {
        return Ok(address.to_owned());
    }
    let probe = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    probe.connect((std::net::Ipv4Addr::new(8, 8, 8, 8), 80))?;
    let ip = match probe.local_addr()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() => ip,
        _ => anyhow::bail!("não foi possível descobrir o IP local da rede"),
    };
    Ok(format!("{ip}:{}", socket.port()))
}

pub fn pair(home: &Path, address: &str) -> Result<DeviceConfig> {
    let _lock = config_guard(home)?;
    if home.join(".rowd/device.json").exists() {
        let mut cfg = DeviceConfig::load(home)?;
        backup_config(home, "pairing-address")?;
        cfg.address = published_address(address)?;
        cfg.save(home)?;
        return Ok(cfg);
    }
    let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()])?;
    let cfg = DeviceConfig {
        version: CURRENT_DEVICE_VERSION,
        address: published_address(address)?,
        listen: "0.0.0.0:43821".into(),
        pair_id: random_id()?,
        folder_id: random_id()?,
        cert: hex::encode(cert.cert.der()),
        key: hex::encode(cert.key_pair.serialize_der()),
        secret: random_id()?,
        peer_device: None,
        shares: vec![],
        removed: vec![],
        share_requests: vec![],
        sync_paused: false,
    };
    invitation(&cfg).validate()?;
    cfg.save(home)?;
    Ok(cfg)
}
pub fn migrate(home: &Path, folder: &Path, address: &str) -> Result<()> {
    let _lock = config_guard(home)?;
    ensure!(
        !home.join(".rowd/device.json").exists(),
        "device already configured"
    );
    let store = LocalStore::open(folder)?;
    let old = legacy_config(&store)?;
    let state = State::load(
        &store.private().join("sync-state.json"),
        &old.pair_id,
        &old.folder_id,
    )?;
    let mut cfg = DeviceConfig {
        version: CURRENT_DEVICE_VERSION,
        address: published_address(address)?,
        listen: "0.0.0.0:43821".into(),
        pair_id: old.pair_id,
        folder_id: old.folder_id.clone(),
        cert: old.cert,
        key: old.key,
        secret: old.secret,
        peer_device: state.peer_root.clone(),
        shares: vec![],
        removed: vec![],
        share_requests: vec![],
        sync_paused: false,
    };
    cfg.put_share(
        home,
        ShareConfig {
            share_id: old.folder_id,
            name: store
                .root()
                .file_name()
                .context("invalid root")?
                .to_string_lossy()
                .into(),
            root: store.root().into(),
            android_path: String::new(),
            mode: SyncMode::Bidirectional,
            enabled: true,
            ignore: String::new(),
            request_id: None,
            remap_policy: None,
        },
    )?;
    atomic_json(&state_path(home, &cfg.shares[0]), &state)?;
    cfg.save(home)
}
pub fn update(home: &Path, action: impl FnOnce(&mut DeviceConfig) -> Result<()>) -> Result<()> {
    let _lock = config_guard(home)?;
    let mut cfg = DeviceConfig::load(home)?;
    backup_config(home, "mutation")?;
    action(&mut cfg)?;
    cfg.save(home)
}

fn update_runtime(home: &Path, action: impl FnOnce(&mut DeviceConfig) -> Result<()>) -> Result<()> {
    let _lock = config_guard(home)?;
    let mut cfg = DeviceConfig::load(home)?;
    action(&mut cfg)?;
    cfg.save(home)
}

pub fn accept_share_request(home: &Path, request_id: &str, folder: &Path) -> Result<()> {
    let _session = session_guard(home)?;
    update(home, |cfg| {
        let request = cfg
            .share_requests
            .iter()
            .find(|request| {
                request.request_id == request_id && request.state == ShareRequestState::Pending
            })
            .cloned()
            .context("solicitação de Share não encontrada")?;
        cfg.put_share(
            home,
            ShareConfig {
                share_id: random_id()?,
                name: request.name.clone(),
                root: folder.to_path_buf(),
                android_path: request.name.clone(),
                mode: request.mode,
                enabled: true,
                ignore: String::new(),
                request_id: Some(request.request_id.clone()),
                remap_policy: None,
            },
        )?;
        cfg.share_requests
            .retain(|pending| pending.request_id != request.request_id);
        Ok(())
    })
}

pub fn pending_share_requests(home: &Path) -> Result<Vec<ShareRequest>> {
    Ok(DeviceConfig::load(home)?
        .share_requests
        .into_iter()
        .filter(|request| request.state == ShareRequestState::Pending)
        .collect())
}

pub fn share_requests(home: &Path) -> Result<Vec<ShareRequest>> {
    Ok(DeviceConfig::load(home)?.share_requests)
}

pub fn reject_share_request(home: &Path, request_id: &str) -> Result<()> {
    let _session = session_guard(home)?;
    update(home, |cfg| {
        let request = cfg
            .share_requests
            .iter_mut()
            .find(|request| request.request_id == request_id)
            .context("solicitação de Share não encontrada")?;
        ensure!(
            request.state == ShareRequestState::Pending,
            "solicitação não está pendente"
        );
        request.state = ShareRequestState::Rejected;
        Ok(())
    })
}

fn archive_derived_state(home: &Path, share_id: &str, reason: &str) -> Result<()> {
    rowd_core::model::validate_hash(share_id)?;
    let directory = home.join(".rowd/shares").join(share_id);
    if !directory.exists() {
        return Ok(());
    }
    let stamp = now();
    for name in ["base.json", "journal.json", "runtime.json"] {
        let path = directory.join(name);
        if path.exists() {
            fs::rename(&path, directory.join(format!("{name}.{reason}-{stamp}")))?;
        }
    }
    Ok(())
}

pub fn reindex_share(home: &Path, id: &str) -> Result<()> {
    let _session = session_guard(home)?;
    let _lock = config_guard(home)?;
    let cfg = DeviceConfig::load(home)?;
    let share = cfg
        .shares
        .iter()
        .find(|share| share.share_id == id)
        .context("unknown Share")?
        .clone();
    backup_config(home, "reindex")?;
    archive_derived_state(home, id, "reindex")?;
    scan_share(home, &share, true, None)
}

pub fn remap_share(home: &Path, id: &str, android_path: String, policy: RemapPolicy) -> Result<()> {
    let _session = session_guard(home)?;
    let _config = config_guard(home)?;
    ensure!(
        !android_path.trim().is_empty(),
        "Android destination is required"
    );
    rowd_core::model::validate_path(&android_path)?;
    let mut config = DeviceConfig::load(home)?;
    let mut share = config
        .shares
        .iter()
        .find(|share| share.share_id == id)
        .context("unknown Share")?
        .clone();
    ensure!(
        share.android_path != android_path,
        "new Android destination is unchanged"
    );
    share.android_path = android_path;
    share.remap_policy = Some(policy);
    config.put_share(home, share)?;
    backup_config(home, "remap")?;
    archive_derived_state(home, id, "remap")?;
    config.save(home)
}

fn validate_ignore(text: &str) -> Result<()> {
    ensure!(text.len() <= 64 * 1024, ".rowdignore is too large");
    for (index, raw) in text.lines().enumerate() {
        let rule = raw.trim();
        if rule.is_empty() || rule.starts_with('#') {
            continue;
        }
        ensure!(
            !rule.starts_with('!')
                && !rule.starts_with('/')
                && !rule.contains('\\')
                && !rule.split('/').any(|part| part == "..")
                && !rule.chars().any(char::is_control),
            "unsupported .rowdignore rule on line {}",
            index + 1
        );
    }
    Ok(())
}

pub fn set_ignore_text(home: &Path, id: &str, text: &str) -> Result<()> {
    let _session = session_guard(home)?;
    let _config = config_guard(home)?;
    validate_ignore(text)?;
    let cfg = DeviceConfig::load(home)?;
    let share = cfg
        .shares
        .iter()
        .find(|share| share.share_id == id)
        .context("unknown Share")?;
    rowd_core::storage::atomic_write(&share.root.join(".rowdignore"), text.as_bytes())?;
    scan_share(home, share, true, None)
}

pub fn request_share_sync(home: &Path, id: &str) -> Result<()> {
    let cfg = DeviceConfig::load(home)?;
    let share = cfg
        .shares
        .iter()
        .find(|share| share.share_id == id)
        .context("unknown Share")?;
    ensure!(share.enabled, "Share is paused");
    atomic_json(&home.join(".rowd/next-share.json"), &id)
}

fn unlink_device_unlocked(home: &Path) -> Result<()> {
    let _lock = config_guard(home)?;
    let mut cfg = DeviceConfig::load(home)?;
    backup_config(home, "unlink")?;
    for share in &cfg.shares {
        archive_derived_state(home, &share.share_id, "unlink")?;
    }
    let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()])?;
    cfg.pair_id = random_id()?;
    cfg.folder_id = random_id()?;
    cfg.cert = hex::encode(cert.cert.der());
    cfg.key = hex::encode(cert.key_pair.serialize_der());
    cfg.secret = random_id()?;
    cfg.peer_device = None;
    cfg.share_requests.clear();
    cfg.removed.clear();
    cfg.save(home)
}

pub fn unlink_device(home: &Path) -> Result<()> {
    let _session = session_guard(home)?;
    unlink_device_unlocked(home)
}

pub fn reset(home: &Path, level: ResetLevel) -> Result<()> {
    match level {
        ResetLevel::Interface => {
            let path = home.join(".rowd/ui.json");
            if path.exists() {
                fs::remove_file(path)?;
            }
            Ok(())
        }
        ResetLevel::Share(id) => reindex_share(home, &id),
        ResetLevel::Unlink => unlink_device(home),
        ResetLevel::Initial => {
            let _session = session_guard(home)?;
            let _lock = config_guard(home)?;
            let old = DeviceConfig::load(home)?;
            backup_config(home, "initial-reset")?;
            let state = home.join(".rowd/shares");
            if state.exists() {
                fs::rename(
                    &state,
                    home.join(".rowd")
                        .join(format!("shares-before-reset-{}", now())),
                )?;
            }
            let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()])?;
            DeviceConfig {
                version: CURRENT_DEVICE_VERSION,
                address: old.address,
                listen: old.listen,
                pair_id: random_id()?,
                folder_id: random_id()?,
                cert: hex::encode(cert.cert.der()),
                key: hex::encode(cert.key_pair.serialize_der()),
                secret: random_id()?,
                peer_device: None,
                shares: Vec::new(),
                removed: Vec::new(),
                share_requests: Vec::new(),
                sync_paused: false,
            }
            .save(home)
        }
        ResetLevel::AllData => {
            let _session = session_guard(home)?;
            let directory = home.join(".rowd");
            ensure!(
                directory.file_name().is_some_and(|name| name == ".rowd"),
                "refusing broad reset target"
            );
            if directory.exists() {
                fs::rename(&directory, home.join(format!(".rowd-erased-{}", now())))?;
            }
            Ok(())
        }
    }
}

pub fn recovery(home: &Path) -> Result<Vec<RecoveryItem>> {
    let cfg = DeviceConfig::load(home)?;
    let mut items = Vec::new();
    for share in cfg.shares {
        let store = LocalStore::open_recovery(&share.root)?;
        for entry in store.recovery_entries()? {
            let backup = store.private().join("recovery").join(&entry.id);
            items.push(RecoveryItem {
                share_id: share.share_id.clone(),
                share_name: share.name.clone(),
                id: entry.id,
                path: entry.path,
                finished: entry.finished,
                backup_available: entry.backup_available,
                bytes: backup
                    .metadata()
                    .map(|metadata| metadata.len())
                    .unwrap_or(0),
            });
        }
    }
    items.sort_by(|a, b| {
        a.share_name
            .cmp(&b.share_name)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(items)
}

fn share_by_id(home: &Path, share_id: &str) -> Result<ShareConfig> {
    DeviceConfig::load(home)?
        .shares
        .into_iter()
        .find(|share| share.share_id == share_id)
        .context("unknown Share")
}

pub fn resolve_recovery(
    home: &Path,
    share_id: &str,
    id: &str,
    action: &str,
    output: Option<&Path>,
) -> Result<()> {
    let _session = session_guard(home)?;
    let share = share_by_id(home, share_id)?;
    LocalStore::open_recovery(&share.root)?.resolve_recovery(id, action, output)
}

pub fn cleanup_recovery(home: &Path, share_id: &str, id: &str) -> Result<()> {
    let _session = session_guard(home)?;
    let share = share_by_id(home, share_id)?;
    LocalStore::open_recovery(&share.root)?.cleanup_recovery(id)
}

pub fn test_connection(home: &Path) -> Result<ConnectionTest> {
    let cfg = DeviceConfig::load(home)?;
    let address = cfg.address.parse::<SocketAddr>();
    let address_ok = address
        .as_ref()
        .is_ok_and(|address| !address.ip().is_unspecified());
    let mut checks = vec![ConnectionCheck {
        label: "PC alcançável",
        ok: address_ok,
        detail: cfg.address.clone(),
    }];
    let tcp = address
        .ok()
        .and_then(|address| TcpStream::connect_timeout(&address, Duration::from_millis(350)).ok());
    checks.push(ConnectionCheck {
        label: "TCP",
        ok: tcp.is_some(),
        detail: if tcp.is_some() {
            "porta respondeu".into()
        } else {
            "porta não respondeu; mantenha o servidor Rowd aberto".into()
        },
    });
    let tls_ok = tls::server_config(&cfg.cert, &cfg.key).is_ok();
    checks.push(ConnectionCheck {
        label: "TLS",
        ok: tls_ok,
        detail: if tls_ok {
            "certificado e chave válidos".into()
        } else {
            "identidade TLS inválida".into()
        },
    });
    checks.push(ConnectionCheck {
        label: "Autenticação",
        ok: cfg.peer_device.is_some(),
        detail: if cfg.peer_device.is_some() {
            "dispositivo autenticado anteriormente".into()
        } else {
            "aguardando primeiro pareamento".into()
        },
    });
    checks.push(ConnectionCheck {
        label: "Dispositivo reconhecido",
        ok: cfg.peer_device.is_some(),
        detail: cfg
            .peer_device
            .as_deref()
            .map(|id| format!("{}…", &id[..id.len().min(12)]))
            .unwrap_or_else(|| "nenhum".into()),
    });
    let available_shares = cfg
        .shares
        .iter()
        .filter(|share| share.enabled && share.root.is_dir())
        .count();
    Ok(ConnectionTest {
        checks,
        available_shares,
        total_shares: cfg.shares.len(),
    })
}

pub fn export_profile(home: &Path, output: &Path) -> Result<()> {
    let cfg = DeviceConfig::load(home)?;
    let profile = PublicProfile {
        version: 1,
        shares: cfg
            .shares
            .into_iter()
            .map(|share| ProfileShare {
                share_id: share.share_id,
                name: share.name,
                root: share.root,
                android_path: share.android_path,
                mode: share.mode,
                enabled: share.enabled,
                ignore: share.ignore,
            })
            .collect(),
        sync_paused: cfg.sync_paused,
        ui: File::open(home.join(".rowd/ui.json"))
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok()),
    };
    write_private(output, &serde_json::to_vec_pretty(&profile)?)
}

pub fn import_profile(home: &Path, input: &Path) -> Result<()> {
    let _session = session_guard(home)?;
    let profile: PublicProfile = serde_json::from_reader(File::open(input)?)?;
    ensure!(profile.version == 1, "unsupported profile version");
    update(home, |cfg| {
        let mut candidate = cfg.clone();
        candidate.shares.clear();
        for share in &profile.shares {
            candidate.put_share(
                home,
                ShareConfig {
                    share_id: share.share_id.clone(),
                    name: share.name.clone(),
                    root: share.root.clone(),
                    android_path: share.android_path.clone(),
                    mode: share.mode,
                    enabled: share.enabled,
                    ignore: share.ignore.clone(),
                    request_id: None,
                    remap_policy: None,
                },
            )?;
        }
        candidate.sync_paused = profile.sync_paused;
        *cfg = candidate;
        Ok(())
    })?;
    if let Some(ui) = profile.ui {
        atomic_json(&home.join(".rowd/ui.json"), &ui)?;
    }
    Ok(())
}

fn collect_state(
    base: &Path,
    directory: &Path,
    output: &mut BTreeMap<String, String>,
) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.file_type()?;
        ensure!(!metadata.is_symlink(), "symlink in application state");
        if metadata.is_dir() {
            collect_state(base, &entry.path(), output)?;
        } else if metadata.is_file() {
            let relative = entry
                .path()
                .strip_prefix(base)?
                .to_string_lossy()
                .into_owned();
            let bytes = fs::read(entry.path())?;
            ensure!(bytes.len() <= 16 * 1024 * 1024, "state file too large");
            output.insert(relative, hex::encode(bytes));
        }
    }
    Ok(())
}

fn crypt_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32]> {
    ensure!(
        passphrase.chars().count() >= 8,
        "passphrase must contain at least 8 characters"
    );
    let mut key = [0u8; 32];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(150_000).unwrap(),
        salt,
        passphrase.as_bytes(),
        &mut key,
    );
    Ok(key)
}

fn encrypt_backup(value: &FullBackup, passphrase: &str) -> Result<EncryptedBackup> {
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
    use ring::rand::{SecureRandom, SystemRandom};
    let random = SystemRandom::new();
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    random
        .fill(&mut salt)
        .map_err(|_| anyhow::anyhow!("random salt"))?;
    random
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("random nonce"))?;
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, &crypt_key(passphrase, &salt)?)
            .map_err(|_| anyhow::anyhow!("invalid backup key"))?,
    );
    let mut bytes = serde_json::to_vec(value)?;
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(b"rowd-backup-v1"),
        &mut bytes,
    )
    .map_err(|_| anyhow::anyhow!("backup encryption failed"))?;
    Ok(EncryptedBackup {
        format: 1,
        salt: hex::encode(salt),
        nonce: hex::encode(nonce),
        ciphertext: hex::encode(bytes),
    })
}

fn decrypt_backup(value: EncryptedBackup, passphrase: &str) -> Result<FullBackup> {
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
    ensure!(value.format == 1, "unsupported backup format");
    let salt = hex::decode(value.salt)?;
    ensure!(salt.len() == 16, "invalid backup salt");
    let nonce: [u8; 12] = hex::decode(value.nonce)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid backup nonce"))?;
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, &crypt_key(passphrase, &salt)?)
            .map_err(|_| anyhow::anyhow!("invalid backup key"))?,
    );
    let mut bytes = hex::decode(value.ciphertext)?;
    let plain = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(b"rowd-backup-v1"),
            &mut bytes,
        )
        .map_err(|_| anyhow::anyhow!("invalid passphrase or damaged backup"))?;
    Ok(serde_json::from_slice(plain)?)
}

pub fn export_backup(home: &Path, output: &Path, passphrase: &str) -> Result<()> {
    let _session = session_guard(home)?;
    let _config = config_guard(home)?;
    let config = DeviceConfig::load(home)?;
    let mut state = BTreeMap::new();
    let base = home.join(".rowd/shares");
    collect_state(&base, &base, &mut state)?;
    ensure!(
        state.len() <= 4096,
        "application state contains too many files"
    );
    ensure!(
        state
            .values()
            .map(|encoded| encoded.len() / 2)
            .sum::<usize>()
            <= 256 * 1024 * 1024,
        "application state is too large to export"
    );
    let backup = FullBackup {
        version: 1,
        config,
        ui: File::open(home.join(".rowd/ui.json"))
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok()),
        state,
    };
    write_private(
        output,
        &serde_json::to_vec_pretty(&encrypt_backup(&backup, passphrase)?)?,
    )
}

fn safe_state_path(base: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "invalid backup state path"
    );
    Ok(base.join(path))
}

pub fn import_backup(home: &Path, input: &Path, passphrase: &str) -> Result<()> {
    let _session = session_guard(home)?;
    ensure!(
        input.metadata()?.len() <= 256 * 1024 * 1024,
        "backup file is too large"
    );
    let encrypted: EncryptedBackup = serde_json::from_reader(File::open(input)?)?;
    let mut backup = decrypt_backup(encrypted, passphrase)?;
    ensure!(backup.version == 1, "unsupported full backup version");
    ensure!(
        backup.config.version == CURRENT_DEVICE_VERSION,
        "backup configuration requires migration by its original Rowd version"
    );
    invitation(&backup.config).validate()?;
    tls::server_config(&backup.config.cert, &backup.config.key)
        .context("backup contains an invalid TLS identity")?;
    let mut validated = backup.config.clone();
    validated.shares.clear();
    for share in backup.config.shares.clone() {
        validated.put_share(home, share)?;
    }
    ensure!(
        validated.shares.len() == backup.config.shares.len(),
        "duplicate Share identity in backup"
    );
    backup.config.shares = validated.shares;
    ensure!(
        backup.state.len() <= 4096,
        "backup contains too many state files"
    );
    let state_root = home.join(".rowd/shares");
    let mut decoded_state = Vec::with_capacity(backup.state.len());
    let mut total_state_bytes = 0usize;
    for (relative, encoded) in backup.state {
        let path = safe_state_path(&state_root, &relative)?;
        let bytes = hex::decode(encoded)?;
        ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "backup state file is too large"
        );
        total_state_bytes = total_state_bytes
            .checked_add(bytes.len())
            .context("backup state size overflow")?;
        ensure!(
            total_state_bytes <= 256 * 1024 * 1024,
            "backup state is too large"
        );
        decoded_state.push((path, bytes));
    }
    let _lock = config_guard(home)?;
    backup_config(home, "before-import")?;
    if state_root.exists() {
        fs::rename(
            &state_root,
            home.join(".rowd")
                .join(format!("shares-before-import-{}", now())),
        )?;
    }
    for (path, bytes) in decoded_state {
        rowd_core::storage::atomic_write(&path, &bytes)?;
    }
    backup.config.save(home)?;
    if let Some(ui) = backup.ui.take() {
        atomic_json(&home.join(".rowd/ui.json"), &ui)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct DiagnosticReport {
    rowd_version: &'static str,
    protocol_version: u32,
    operating_system: &'static str,
    paired: bool,
    sync_paused: bool,
    shares: Vec<Status>,
    recovery_versions: usize,
    recovery_bytes: u64,
}

pub fn export_diagnostic(home: &Path, output: &Path) -> Result<()> {
    let cfg = DeviceConfig::load(home)?;
    let shares = status(home)?;
    let recovery = recovery(home)?;
    let report = DiagnosticReport {
        rowd_version: env!("CARGO_PKG_VERSION"),
        protocol_version: protocol::PROTOCOL_VERSION,
        operating_system: std::env::consts::OS,
        paired: cfg.peer_device.is_some(),
        sync_paused: cfg.sync_paused,
        shares,
        recovery_versions: recovery.len(),
        recovery_bytes: recovery.iter().map(|entry| entry.bytes).sum(),
    };
    write_private(output, &serde_json::to_vec_pretty(&report)?)
}
pub fn state_path(home: &Path, share: &ShareConfig) -> PathBuf {
    home.join(".rowd/shares")
        .join(&share.share_id)
        .join("base.json")
}
pub fn journal_path(home: &Path, share: &ShareConfig) -> PathBuf {
    home.join(".rowd/shares")
        .join(&share.share_id)
        .join("journal.json")
}
pub fn scan(home: &Path, full: bool) -> Result<()> {
    let _session = session_guard(home)?;
    let cfg = DeviceConfig::load(home)?;
    for share in &cfg.shares {
        if full {
            let _root_lock = LocalStore::open(&share.root)?;
            let journal = journal_path(home, share);
            if journal.exists() && ShareState::load(&journal, &share.share_id).is_err() {
                fs::rename(
                    &journal,
                    journal.with_extension(format!("invalid-{}.json", random_id()?)),
                )?;
            }
            let base = state_path(home, share);
            if base.exists() && State::load(&base, &cfg.pair_id, &share.share_id).is_err() {
                fs::rename(
                    &base,
                    base.with_extension(format!("invalid-{}.json", random_id()?)),
                )?;
            }
        }
        scan_share(home, share, full, None)
            .with_context(|| format!("Share {} ({})", share.name, share.root.display()))?;
    }
    Ok(())
}
fn scan_share(
    home: &Path,
    share: &ShareConfig,
    full: bool,
    dirty: Option<&BTreeSet<String>>,
) -> Result<()> {
    let mut store = LocalStore::open(&share.root)?;
    store.remote_ignore = share.ignore.clone();
    if full {
        store.invalidate();
    } else if let Some(paths) = dirty {
        for path in paths {
            if path.is_empty() || path == ".rowdignore" {
                store.invalidate();
            } else {
                store.invalidate_path(path);
            }
        }
    }
    TrackedStore::new(store, journal_path(home, share), &share.share_id)?.scan()?;
    Ok(())
}

fn scan_share_serialized(
    home: &Path,
    share: &ShareConfig,
    full: bool,
    dirty: Option<&BTreeSet<String>>,
) -> Result<()> {
    let _session = session_guard(home)?;
    scan_share(home, share, full, dirty)
}
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub share: ShareConfig,
    pub root_available: bool,
    pub pending: usize,
    pub pending_paths: Vec<String>,
    pub conflicts: Vec<String>,
    pub last_sync: Option<u64>,
    pub error: Option<String>,
    pub last_error: Option<StoredError>,
}
pub fn status(home: &Path) -> Result<Vec<Status>> {
    let cfg = DeviceConfig::load(home)?;
    cfg.shares
        .into_iter()
        .map(|share| {
            let journal = ShareState::load(&journal_path(home, &share), &share.share_id);
            let state = State::load(&state_path(home, &share), &cfg.pair_id, &share.share_id);
            let error = journal
                .as_ref()
                .err()
                .map(ToString::to_string)
                .or_else(|| state.as_ref().err().map(ToString::to_string));
            let pending_paths = journal
                .as_ref()
                .map(|s| s.pending.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let last_error = File::open(runtime_path(home, &share.share_id))
                .ok()
                .and_then(|file| serde_json::from_reader::<_, ShareRuntime>(file).ok())
                .and_then(|runtime| runtime.last_error);
            Ok(Status {
                root_available: share.root.is_dir(),
                pending: pending_paths.len(),
                pending_paths,
                conflicts: state
                    .as_ref()
                    .map(|s| s.conflicts.iter().cloned().collect())
                    .unwrap_or_default(),
                last_sync: state.as_ref().ok().and_then(|s| s.last_sync),
                error,
                last_error,
                share,
            })
        })
        .collect()
}

fn session(home: &Path, socket: TcpStream, event: &impl Fn(String)) -> Result<()> {
    let _session = session_guard(home)?;
    let mut cfg = DeviceConfig::load(home)?;
    let mut io = tls::accept(socket, tls::server_config(&cfg.cert, &cfg.key)?)?;
    let result =
        (|| -> Result<()> {
            let device = protocol::server_auth(&mut io, &cfg.pair_id, &cfg.folder_id, &cfg.secret)?;
            if let Some(peer) = &cfg.peer_device {
                ensure!(peer == &device, "another Android device is already paired");
            } else {
                update_runtime(home, |latest| {
                    ensure!(
                        latest
                            .peer_device
                            .as_ref()
                            .is_none_or(|peer| peer == &device),
                        "another Android device is already paired"
                    );
                    latest.peer_device = Some(device.clone());
                    Ok(())
                })?;
                cfg = DeviceConfig::load(home)?;
            }
            atomic_json(
                &device_runtime_path(home),
                &DeviceRuntime {
                    last_connection: Some(now()),
                },
            )?;
            event("Android conectado".into());
            let mut advertised = cfg.shares.clone();
            for share in &mut advertised {
                share.ignore.push_str(&format!(
                    "\n{}",
                    fs::read_to_string(share.root.join(".rowdignore")).unwrap_or_default()
                ));
            }
            let advertised_removed = cfg.removed.clone();
            protocol::send(
                &mut io,
                &Message::Shares {
                    shares: advertised,
                    removed: advertised_removed.clone(),
                },
            )?;
            let Message::Capabilities {
                device_id,
                managed_shares,
                share_requests,
                available_shares,
                unlink_requested,
                ..
            } = protocol::receive(&mut io)?
            else {
                anyhow::bail!("expected Android capabilities")
            };
            ensure!(
                device_id == device && managed_shares,
                "Android device/capabilities mismatch"
            );
            if unlink_requested {
                protocol::send(&mut io, &Message::DeviceUnlinked)?;
                unlink_device_unlocked(home)?;
                event("Android desvinculado; uma nova identidade de pareamento foi criada".into());
                return Ok(());
            }

            let available_shares: BTreeSet<_> = available_shares.into_iter().collect();
            for id in &available_shares {
                rowd_core::model::validate_hash(id)?;
                ensure!(
                    cfg.shares.iter().any(|share| &share.share_id == id),
                    "Android advertised an unknown Share"
                );
            }

            let mut accepted = Vec::new();
            let mut pending = Vec::new();
            let mut rejected = Vec::new();
            let mut cancelled = Vec::new();
            let incoming_request_ids = share_requests
                .iter()
                .map(|request| request.request_id.clone())
                .collect::<BTreeSet<_>>();
            update_runtime(home, |latest| {
                for request in share_requests {
                    rowd_core::model::validate_hash(&request.request_id)?;
                    ensure!(
                        !request.name.trim().is_empty()
                            && request.name.len() <= 120
                            && !request.name.chars().any(char::is_control),
                        "invalid Share request name"
                    );
                    let id = request.request_id.clone();
                    if request.state == ShareRequestState::Cancelled {
                        latest
                            .share_requests
                            .retain(|stored| stored.request_id != request.request_id);
                        cancelled.push(id);
                        continue;
                    }
                    if latest.shares.iter().any(|share| {
                        share.request_id.as_deref() == Some(request.request_id.as_str())
                    }) {
                        accepted.push(id);
                        continue;
                    }
                    match latest
                        .share_requests
                        .iter()
                        .find(|stored| stored.request_id == request.request_id)
                        .map(|stored| stored.state)
                    {
                        Some(ShareRequestState::Rejected) => rejected.push(id),
                        Some(ShareRequestState::Accepted) => accepted.push(id),
                        Some(ShareRequestState::Cancelled) => cancelled.push(id),
                        Some(ShareRequestState::Pending) => pending.push(id),
                        None => {
                            let mut request = request;
                            request.state = ShareRequestState::Pending;
                            pending.push(id);
                            latest.share_requests.push(request);
                        }
                    }
                }
                latest.share_requests.retain(|request| {
                    request.state != ShareRequestState::Rejected
                        || incoming_request_ids.contains(&request.request_id)
                });
                ensure!(
                    latest.share_requests.len() <= 512,
                    "too many Share requests"
                );
                Ok(())
            })?;
            cfg = DeviceConfig::load(home)?;
            protocol::send(
                &mut io,
                &Message::ShareRequestStatus {
                    accepted,
                    pending,
                    rejected,
                    cancelled,
                },
            )?;

            if cfg.sync_paused {
                protocol::send(&mut io, &Message::SessionDone)?;
                event("Sincronização global pausada".into());
                return Ok(());
            }

            let requested_share: Option<String> = File::open(home.join(".rowd/next-share.json"))
                .ok()
                .and_then(|file| serde_json::from_reader(file).ok());
            let ready = cfg
                .shares
                .iter()
                .filter(|share| {
                    share.enabled
                        && available_shares.contains(&share.share_id)
                        && requested_share
                            .as_ref()
                            .is_none_or(|requested| requested == &share.share_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            let waiting = cfg
                .shares
                .iter()
                .filter(|share| share.enabled && !available_shares.contains(&share.share_id))
                .map(|share| share.name.as_str())
                .collect::<Vec<_>>();
            if !waiting.is_empty() {
                event(format!(
                    "Aguardando pasta Android para: {}",
                    waiting.join(", ")
                ));
            }

            let mut completed_remaps = Vec::new();
            for (index, share) in ready.iter().enumerate() {
                event(format!(
                    "Sincronizando {} ({}/{})",
                    share.name,
                    index + 1,
                    ready.len()
                ));
                let operation = if share.remap_policy.is_some() {
                    "remap"
                } else {
                    "sync"
                };
                let result = (|| -> Result<_> {
                    let mut store = LocalStore::open(&share.root)?;
                    store.remote_ignore = share.ignore.clone();
                    let mut store =
                        TrackedStore::new(store, journal_path(home, share), &share.share_id)?;
                    let path = state_path(home, share);
                    let mut state = State::load(&path, &cfg.pair_id, &share.share_id)?;
                    protocol::send(
                        &mut io,
                        &Message::SelectShare {
                            share_id: share.share_id.clone(),
                        },
                    )?;
                    ensure!(
                        matches!(protocol::receive(&mut io)?, Message::Ready),
                        "Share not ready"
                    );
                    let mode = match share.remap_policy {
                        Some(RemapPolicy::Pc) => SyncMode::ToAndroid,
                        Some(RemapPolicy::Android) => SyncMode::ToPc,
                        Some(RemapPolicy::Compare) | None => share.mode,
                    };
                    sync::coordinate_with_progress(
                        &mut io,
                        &mut store,
                        &mut state,
                        &path,
                        mode,
                        |path, done, total| {
                            event(format!("{}: {done}/{total} caminhos · {path}", share.name))
                        },
                    )
                })()
                .with_context(|| format!("Share {} ({})", share.name, share.root.display()));
                let report = match result {
                    Ok(report) => report,
                    Err(error) => {
                        record_share_error(home, &share.share_id, operation, &error);
                        return Err(error);
                    }
                };
                resolve_share_error(home, &share.share_id);
                if share.remap_policy.is_some() {
                    completed_remaps.push(share.share_id.clone());
                }
                event(format!(
                    "{}: {} transferências, {} conflitos",
                    share.name, report.transferred, report.conflicts
                ));
            }
            protocol::send(&mut io, &Message::SessionDone)?;
            update_runtime(home, |latest| {
                latest
                    .removed
                    .retain(|removed| !advertised_removed.contains(removed));
                for share in &mut latest.shares {
                    if completed_remaps.contains(&share.share_id) {
                        share.remap_policy = None;
                    }
                }
                Ok(())
            })?;
            if requested_share.is_some() && !ready.is_empty() {
                let _ = fs::remove_file(home.join(".rowd/next-share.json"));
            }
            event("Android: última rodada concluída; aguardando conexão".into());
            Ok(())
        })();
    if let Err(ref error) = result {
        let _ = protocol::send(
            &mut io,
            &Message::Error {
                message: format!("{error:#}"),
            },
        );
    }
    result
}

pub fn serve(
    home: &Path,
    listen: Option<&str>,
    once: bool,
    stop: Arc<AtomicBool>,
    event: impl Fn(String),
) -> Result<()> {
    let _runtime_lock = LocalStore::open(&home.join(".rowd/runtime"))?;
    let cfg = DeviceConfig::load(home)?;
    let listener = TcpListener::bind(listen.unwrap_or(&cfg.listen))?;
    listener.set_nonblocking(true)?;
    event(format!("Rowd ouvindo em {}", listener.local_addr()?));
    let (tx, rx) = mpsc::sync_channel(1024);
    let overflow = Arc::new(AtomicBool::new(false));
    let watcher_overflow = overflow.clone();
    let mut watcher: Option<RecommendedWatcher> = match notify::recommended_watcher(move |e| {
        if tx.try_send(e).is_err() {
            watcher_overflow.store(true, Ordering::Relaxed);
        }
    }) {
        Ok(watcher) => Some(watcher),
        Err(e) => {
            event(format!("Watcher indisponível: {e}; usando scan periódico"));
            None
        }
    };
    let mut watched: BTreeMap<String, ShareConfig> = BTreeMap::new();
    let mut dirty: BTreeMap<String, (Instant, BTreeSet<String>)> = BTreeMap::new();
    let mut full = false;
    let mut last_fallback = Instant::now();
    let mut last_full = Instant::now();
    let mut last_config = Instant::now() - Duration::from_secs(2);
    while !stop.load(Ordering::Relaxed) {
        if last_config.elapsed() >= Duration::from_secs(1) {
            let cfg = DeviceConfig::load(home)?;
            for (id, old) in &watched {
                if !cfg
                    .shares
                    .iter()
                    .any(|s| s.share_id == *id && s.root == old.root)
                {
                    if let Some(watcher) = &mut watcher {
                        let _ = watcher.unwatch(&old.root);
                    }
                }
            }
            for share in cfg.shares.iter().filter(|share| share.enabled) {
                if !watched
                    .get(&share.share_id)
                    .is_some_and(|s| s.root == share.root)
                {
                    if let Some(watcher) = &mut watcher {
                        if let Err(e) = watcher.watch(&share.root, RecursiveMode::Recursive) {
                            event(format!(
                                "{}: watcher indisponível; usando scan periódico: {e}",
                                share.name
                            ));
                        }
                    }
                    if let Err(e) = scan_share_serialized(
                        home,
                        share,
                        !journal_path(home, share).exists(),
                        None,
                    ) {
                        event(format!("{}: {e:#}", share.name));
                    }
                }
            }
            watched = cfg
                .shares
                .into_iter()
                .filter(|share| share.enabled)
                .map(|s| (s.share_id.clone(), s))
                .collect();
            last_config = Instant::now();
        }
        if overflow.swap(false, Ordering::Relaxed) {
            full = true;
        }
        for result in rx.try_iter() {
            match result {
                Ok(Event {
                    kind, paths, attrs, ..
                }) => {
                    if attrs.flag() == Some(notify::event::Flag::Rescan) {
                        full = true;
                    }
                    if matches!(kind, notify::EventKind::Access(_)) {
                        continue;
                    }
                    for share in watched.values() {
                        let ignore = rowd_core::ignore::Ignore::parse(
                            &fs::read_to_string(share.root.join(".rowdignore")).unwrap_or_default(),
                        );
                        for path in &paths {
                            if let Ok(relative) = path.strip_prefix(&share.root) {
                                let relative = relative.to_string_lossy().to_string();
                                if relative.is_empty()
                                    || relative == ".rowdignore"
                                    || !ignore.matches(&relative, path.is_dir())
                                {
                                    let entry = dirty
                                        .entry(share.share_id.clone())
                                        .or_insert_with(|| (Instant::now(), BTreeSet::new()));
                                    entry.0 = Instant::now();
                                    entry.1.insert(relative);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    full = true;
                    event(format!("Watcher: {e}; reconstruindo manifesto"));
                }
            }
        }
        let fallback = last_fallback.elapsed() >= Duration::from_secs(60);
        if last_full.elapsed() >= Duration::from_secs(15 * 60) {
            full = true;
        }
        for share in watched.values() {
            if full
                || fallback
                || dirty
                    .get(&share.share_id)
                    .is_some_and(|(t, _)| t.elapsed() >= Duration::from_millis(350))
            {
                if let Err(e) = scan_share_serialized(
                    home,
                    share,
                    full,
                    dirty.get(&share.share_id).map(|(_, paths)| paths),
                ) {
                    event(format!("{}: {e:#}", share.name));
                }
                dirty.remove(&share.share_id);
            }
        }
        if full || fallback {
            last_fallback = Instant::now();
            if full {
                last_full = Instant::now();
            }
            full = false;
        }
        match listener.accept() {
            Ok((socket, _)) => {
                let result = session(home, socket, &event);
                if let Err(ref e) = result {
                    event(format!("Rodada interrompida: {e:#}"));
                }
                if once {
                    return result;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn qr(cfg: &DeviceConfig) -> Result<String> {
    let text = pairing_payload(cfg)?;
    let code = qrcode::QrCode::new(text.as_bytes())?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build())
}

pub fn export_qr(home: &Path, cfg: &DeviceConfig) -> Result<PathBuf> {
    let code = qrcode::QrCode::new(pairing_payload(cfg)?.as_bytes())?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(768, 768)
        .build();
    let path = home.join(".rowd/pairing.svg");
    rowd_core::storage::atomic_write(&path, svg.as_bytes())?;
    Ok(path)
}

pub fn pairing_payload(cfg: &DeviceConfig) -> Result<String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let invite = invitation(cfg);
    invite.validate()?;
    let address = invite.address.as_bytes();
    let certificate = hex::decode(&invite.cert_der)?;
    ensure!(
        address.len() <= u16::MAX as usize && certificate.len() <= u16::MAX as usize,
        "pairing payload is too large"
    );
    let mut bytes = Vec::with_capacity(1 + 4 + 2 + address.len() + 96 + 2 + certificate.len());
    bytes.push(1);
    bytes.extend_from_slice(&invite.version.to_be_bytes());
    bytes.extend_from_slice(&(address.len() as u16).to_be_bytes());
    bytes.extend_from_slice(address);
    for value in [&invite.pair_id, &invite.folder_id, &invite.secret] {
        bytes.extend_from_slice(&hex::decode(value)?);
    }
    bytes.extend_from_slice(&(certificate.len() as u16).to_be_bytes());
    bytes.extend_from_slice(&certificate);
    Ok(format!("rowd1:{}", URL_SAFE_NO_PAD.encode(bytes)))
}

pub fn save_private(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_private(path, &bytes)
}

pub fn legacy_init(folder: &Path, address: &str, invite: &Path) -> Result<()> {
    let store = LocalStore::open(folder)?;
    ensure!(
        !store.private().join("server.json").exists(),
        "folder already paired; existing identity preserved"
    );
    ensure!(!invite.exists(), "invitation file already exists");
    let invite_parent = invite
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    ensure!(
        !invite_parent.starts_with(store.root()),
        "save private invitation OUTSIDE the shared folder"
    );
    let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()])?;
    let cfg = LegacyConfig {
        version: VERSION,
        root: store.root().to_string_lossy().into(),
        pair_id: random_id()?,
        folder_id: random_id()?,
        secret: random_id()?,
        cert: hex::encode(cert.cert.der()),
        key: hex::encode(cert.key_pair.serialize_der()),
    };
    let invitation = Invitation {
        version: VERSION,
        address: address.into(),
        pair_id: cfg.pair_id.clone(),
        folder_id: cfg.folder_id.clone(),
        cert_der: cfg.cert.clone(),
        secret: cfg.secret.clone(),
    };
    invitation.validate()?;
    save_private(invite, &invitation)?;
    save_private(&store.private().join("server.json"), &cfg)
}

pub fn legacy_serve(folder: &Path, listen: &str, once: bool) -> Result<()> {
    let mut store = LocalStore::open(folder)?;
    let cfg = legacy_config(&store)?;
    let tls_config = tls::server_config(&cfg.cert, &cfg.key)?;
    let state_path = store.private().join("sync-state.json");
    let mut state = State::load(&state_path, &cfg.pair_id, &cfg.folder_id)?;
    let listener = TcpListener::bind(listen)?;
    println!(
        "Rowd ouvindo em {} • {}",
        listener.local_addr()?,
        store.root().display()
    );
    for socket in listener.incoming() {
        let result = (|| -> Result<sync::Report> {
            let mut stream = tls::accept(socket?, tls_config.clone())?;
            let root =
                protocol::server_auth(&mut stream, &cfg.pair_id, &cfg.folder_id, &cfg.secret)?;
            if let Some(expected) = &state.peer_root {
                ensure!(
                    expected == &root,
                    "Android folder changed; pairing belongs to a different root"
                );
            } else {
                state.peer_root = Some(root);
                atomic_json(&state_path, &state)?;
            }
            let result = sync::coordinate(&mut stream, &mut store, &mut state, &state_path);
            if let Err(ref error) = result {
                let _ = protocol::send(
                    &mut stream,
                    &Message::Error {
                        message: error.to_string(),
                    },
                );
            }
            result
        })();
        match result {
            Ok(report) => println!(
                "Sincronizado: {} transferências, {} conflitos",
                report.transferred, report.conflicts
            ),
            Err(error) => {
                eprintln!("Rodada interrompida: {error:#}");
                if once {
                    return Err(error);
                }
            }
        }
        if once {
            break;
        }
    }
    Ok(())
}

pub fn legacy_sync(folder: &Path, invite: &Path, watch: bool) -> Result<()> {
    let invitation: Invitation = serde_json::from_reader(File::open(invite)?)?;
    let mut store = LocalStore::open(folder)?;
    let id_path = store.private().join("client-id.json");
    let id: String = if id_path.exists() {
        serde_json::from_reader(File::open(&id_path)?)?
    } else {
        let id = random_id()?;
        atomic_json(&id_path, &id)?;
        id
    };
    loop {
        match sync::client_round(&invitation, &id, &mut store) {
            Ok(report) => println!("{}", serde_json::to_string(&report)?),
            Err(error) if watch => eprintln!("Aguardando próxima tentativa: {error:#}"),
            Err(error) => return Err(error),
        }
        if !watch {
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    Ok(())
}

pub fn device_sync(folder: &Path, invite: &Path, watch: bool) -> Result<()> {
    fs::create_dir_all(folder.join(".rowd"))?;
    let id_path = folder.join(".rowd/client-id.json");
    let id: String = if id_path.exists() {
        serde_json::from_reader(File::open(&id_path)?)?
    } else {
        let id = random_id()?;
        atomic_json(&id_path, &id)?;
        id
    };
    let invitation = serde_json::from_reader(File::open(invite)?)?;
    let mut device = rowd_core::managed::LocalDevice::open(folder)?;
    loop {
        match rowd_core::managed::client_round(&invitation, &id, &mut device) {
            Ok(report) => println!("{}", serde_json::to_string(&report)?),
            Err(error) if watch => eprintln!("{error:#}"),
            Err(error) => return Err(error),
        }
        if !watch {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}

pub fn legacy_status(folder: &Path) -> Result<rowd_core::model::Manifest> {
    let mut store = LocalStore::open(folder)?;
    store.invalidate();
    store.scan()
}

pub fn legacy_recovery(
    folder: &Path,
    id: Option<&str>,
    action: Option<&str>,
    output: Option<&Path>,
) -> Result<Option<Vec<rowd_core::storage::RecoveryEntry>>> {
    let mut store = LocalStore::open_recovery(folder)?;
    if let Some(id) = id {
        store.resolve_recovery(id, action.context("--action is required")?, output)?;
        Ok(None)
    } else {
        Ok(Some(store.recovery_entries()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_app() -> (tempfile::TempDir, App, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("config");
        let share = directory.path().join("share");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&share).unwrap();
        let app = App::new(&home);
        app.pair("127.0.0.1:43821").unwrap();
        (directory, app, share)
    }

    #[test]
    fn compact_pairing_payload_keeps_every_credential() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

        let (_directory, app, _share) = configured_app();
        let config = DeviceConfig::load(app.home()).unwrap();
        let payload = pairing_payload(&config).unwrap();
        let encoded = payload.strip_prefix("rowd1:").unwrap();
        let bytes = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        assert_eq!(bytes[0], 1);
        assert_eq!(u32::from_be_bytes(bytes[1..5].try_into().unwrap()), VERSION);
        assert!(payload.len() < serde_json::to_string(&invitation(&config)).unwrap().len());
        for credential in [&config.pair_id, &config.folder_id, &config.secret] {
            let credential = hex::decode(credential).unwrap();
            assert!(bytes
                .windows(credential.len())
                .any(|window| window == credential.as_slice()));
        }
    }

    #[test]
    fn encrypted_backup_round_trip_restores_administrative_state() {
        let (directory, app, share) = configured_app();
        let share_id = app
            .add_share("Docs".into(), share, None, SyncMode::Bidirectional)
            .unwrap();
        app.set_sync_paused(true).unwrap();
        let backup = directory.path().join("rowd-backup.json");
        app.export_backup(&backup, "correct horse").unwrap();

        app.set_sync_paused(false).unwrap();
        app.set_share_enabled(&share_id, false).unwrap();
        assert!(app.import_backup(&backup, "wrong passphrase").is_err());
        assert!(!DeviceConfig::load(app.home()).unwrap().sync_paused);

        app.import_backup(&backup, "correct horse").unwrap();
        let restored = DeviceConfig::load(app.home()).unwrap();
        assert!(restored.sync_paused);
        assert!(restored.shares[0].enabled);
    }

    #[test]
    fn rejected_request_remains_a_tombstone_until_acknowledged() {
        let (_directory, app, _share) = configured_app();
        let request_id = random_id().unwrap();
        update(app.home(), |config| {
            config.share_requests.push(ShareRequest {
                request_id: request_id.clone(),
                name: "Camera".into(),
                mode: SyncMode::ToPc,
                state: ShareRequestState::Pending,
            });
            Ok(())
        })
        .unwrap();
        app.reject_share_request(&request_id).unwrap();
        assert_eq!(
            app.share_requests().unwrap()[0].state,
            ShareRequestState::Rejected
        );
    }
}
