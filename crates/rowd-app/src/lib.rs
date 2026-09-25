mod compat;
mod config;

pub use config::{backup_config, DeviceConfig, CURRENT_DEVICE_VERSION};

use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use rowd_core::{
    config::{RemapPolicy, ShareConfig, ShareRequest, SyncMode},
    model::{Invitation, INVITATION_VERSION},
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
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    num::NonZeroU32,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
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

    pub fn migrate(&self, folder: &Path, address: &str) -> Result<()> {
        migrate(&self.home, folder, address)
    }

    pub fn scan(&self, full: bool) -> Result<()> {
        scan(&self.home, full)
    }

    pub fn serve(
        &self,
        listen: Option<&str>,
        once: bool,
        stop: Arc<AtomicBool>,
        event: impl Fn(String) + Sync,
    ) -> Result<()> {
        serve(&self.home, listen, once, stop, event)
    }

    pub fn pairing_info(&self) -> Result<PairingInfo> {
        pairing_info(&self.home)
    }

    pub fn export_invitation(&self, output: &Path) -> Result<()> {
        let config = DeviceConfig::load(&self.home)?;
        write_private(
            output,
            format!("{}\n", invitation(&config).encode()?).as_bytes(),
        )
    }

    pub fn device_status(&self) -> Result<DeviceStatus> {
        device_status(&self.home)
    }

    pub fn snapshot(&self) -> Result<AppSnapshot> {
        if !self.home.join(".rowd/device.json").exists() {
            return Ok(AppSnapshot {
                device: device_status(&self.home)?,
                ..Default::default()
            });
        }
        let config = self.config()?;
        Ok(AppSnapshot {
            device: device_status_from_config(&self.home, &config)?,
            shares: status_from_config(&self.home, &config)?,
            requests: config.share_requests.clone(),
            recovery: recovery_from_config(&config)?,
        })
    }

    pub fn share_requests(&self) -> Result<Vec<ShareRequest>> {
        share_requests(&self.home)
    }

    pub fn status(&self) -> Result<Vec<Status>> {
        status(&self.home)
    }

    pub fn add_share(&self, name: String, root: PathBuf, mode: SyncMode) -> Result<String> {
        let mut id = String::new();
        update(&self.home, |cfg| {
            id = cfg.add_share(&self.home, name, root, mode)?;
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

    pub fn remap_share(&self, id: &str, policy: RemapPolicy) -> Result<()> {
        remap_share(&self.home, id, policy)
    }

    pub fn ignore_text(&self, id: &str) -> Result<String> {
        let cfg = self.config()?;
        let share = cfg
            .shares
            .iter()
            .find(|share| share.share_id == id)
            .context("unknown Share")?;
        read_ignore(&share.root)
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

    pub fn revoke_device(&self) -> Result<()> {
        revoke_device(&self.home)
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

    pub fn reset_device_configuration(&self) -> Result<()> {
        reset_device_configuration(&self.home)
    }
    pub fn archive_all_application_data(&self) -> Result<()> {
        archive_all_application_data(&self.home)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceStatus {
    pub configured: bool,
    pub paired: bool,
    pub address: String,
    pub identity: String,
    pub fingerprint: String,
    pub sync_paused: bool,
    pub last_connection: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PairingInfo {
    pub device: DeviceStatus,
    pub qr: String,
    pub qr_image: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AppSnapshot {
    pub device: DeviceStatus,
    pub shares: Vec<Status>,
    pub requests: Vec<ShareRequest>,
    pub recovery: Vec<RecoveryItem>,
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
    #[serde(default)]
    last_sync: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DeviceRuntime {
    last_connection: Option<u64>,
    #[serde(default)]
    last_round: Option<RoundMetrics>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RoundMetrics {
    ttfb_ms: Option<u128>,
    total_ms: u128,
    shares_processed: usize,
    shares_changed: usize,
    bytes_transferred: u64,
    per_share: BTreeMap<String, sync::ShareMetrics>,
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
    mode: SyncMode,
    enabled: bool,
    ignore: String,
}

#[derive(Serialize, Deserialize)]
struct PublicProfile {
    version: u32,
    shares: Vec<ProfileShare>,
    sync_paused: bool,
}

#[derive(Serialize, Deserialize)]
struct FullBackup {
    version: u32,
    config: DeviceConfig,
    state: BTreeMap<String, String>,
    ignore_policies: BTreeMap<String, String>,
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
    let directory = home.join(".rowd-locks");
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
    let directory = home.join(".rowd-locks");
    fs::create_dir_all(&directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("config.lock"))?;
    lock.lock_exclusive()?;
    config::recover_import(home)?;
    Ok(lock)
}

fn device_runtime(home: &Path) -> DeviceRuntime {
    File::open(device_runtime_path(home))
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_default()
}

fn record_share_error(home: &Path, share_id: &str, operation: &str, error: &anyhow::Error) {
    let path = runtime_path(home, share_id);
    let mut runtime: ShareRuntime = File::open(&path)
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_default();
    runtime.last_error = Some(StoredError {
        at: now(),
        operation: operation.into(),
        message: format!("{error:#}"),
        resolved_at: None,
    });
    let _ = atomic_json(&path, &runtime);
}

fn resolve_share_error(home: &Path, share_id: &str) {
    let path = runtime_path(home, share_id);
    let mut runtime: ShareRuntime = File::open(&path)
        .ok()
        .and_then(|file| serde_json::from_reader(file).ok())
        .unwrap_or_default();
    if let Some(error) = &mut runtime.last_error {
        error.resolved_at = Some(now());
    }
    runtime.last_sync = Some(now());
    let _ = atomic_json(&path, &runtime);
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

pub fn default_home() -> PathBuf {
    std::env::var_os("ROWD_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/rowd")
        })
}
fn invitation(cfg: &DeviceConfig) -> Invitation {
    Invitation {
        version: INVITATION_VERSION,
        address: cfg.address.clone(),
        pair_id: cfg.pair_id.clone(),
        cert_der: cfg.cert.clone(),
        secret: cfg.secret.clone(),
    }
}

fn pairing_fingerprint(cfg: &DeviceConfig) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(Sha256::digest(hex::decode(&cfg.cert)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

fn pairing_info(home: &Path) -> Result<PairingInfo> {
    let config = DeviceConfig::load(home)?;
    Ok(PairingInfo {
        device: device_status_from_config(home, &config)?,
        qr: qr(&config)?,
        qr_image: export_qr(home, &config)?,
    })
}

fn device_status(home: &Path) -> Result<DeviceStatus> {
    let path = home.join(".rowd/device.json");
    if !path.exists() {
        return Ok(DeviceStatus {
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
    device_status_from_config(home, &config)
}

fn device_status_from_config(home: &Path, config: &DeviceConfig) -> Result<DeviceStatus> {
    let fingerprint = pairing_fingerprint(config)?;
    let last_connection = device_runtime(home).last_connection;
    Ok(DeviceStatus {
        configured: true,
        paired: config.peer_device.is_some(),
        address: config.address.clone(),
        identity: config
            .peer_device
            .clone()
            .unwrap_or_else(|| "aguardando primeiro vínculo".into()),
        fingerprint,
        sync_paused: config.sync_paused,
        last_connection,
    })
}

/// Keep the server wildcard bind private; invitations must contain an address
/// reachable from the Android device on the local network.
fn published_address(address: &str) -> Result<String> {
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

fn pair(home: &Path, address: &str) -> Result<DeviceConfig> {
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
        cert: hex::encode(cert.cert.der()),
        key: hex::encode(cert.key_pair.serialize_der()),
        secret: random_id()?,
        peer_device: None,
        shares: vec![],
        share_requests: vec![],
        rejected_requests: vec![],
        sync_paused: false,
        pending_unlink: false,
        import_generation: None,
    };
    invitation(&cfg).validate()?;
    cfg.save(home)?;
    Ok(cfg)
}
fn migrate(home: &Path, folder: &Path, address: &str) -> Result<()> {
    let _lock = config_guard(home)?;
    ensure!(
        !home.join(".rowd/device.json").exists(),
        "device already configured"
    );
    let store = LocalStore::open(folder)?;
    let old = compat::v1::load_config(&store)?;
    let state = compat::v1::load_state(&store, &old)?;
    let mut cfg = DeviceConfig {
        version: CURRENT_DEVICE_VERSION,
        address: published_address(address)?,
        listen: "0.0.0.0:43821".into(),
        pair_id: old.pair_id,
        cert: old.cert,
        key: old.key,
        secret: old.secret,
        peer_device: state.peer_root.clone(),
        shares: vec![],
        share_requests: vec![],
        rejected_requests: vec![],
        sync_paused: false,
        pending_unlink: false,
        import_generation: None,
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
            binding_revision: 0,
            mode: SyncMode::Bidirectional,
            enabled: true,
            legacy_ignore: String::new(),
            request_id: None,
            remap_policy: None,
        },
    )?;
    atomic_json(&state_path(home, &cfg.shares[0]), &state)?;
    cfg.save(home)
}
fn update(home: &Path, action: impl FnOnce(&mut DeviceConfig) -> Result<()>) -> Result<()> {
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

fn accept_share_request(home: &Path, request_id: &str, folder: &Path) -> Result<()> {
    let _session = session_guard(home)?;
    update(home, |cfg| {
        let request = cfg
            .share_requests
            .iter()
            .find(|request| request.request_id == request_id)
            .cloned()
            .context("solicitação de Share não encontrada")?;
        cfg.put_share(
            home,
            ShareConfig {
                share_id: random_id()?,
                name: request.name.clone(),
                root: folder.to_path_buf(),
                binding_revision: 0,
                mode: request.mode,
                enabled: true,
                legacy_ignore: String::new(),
                request_id: Some(request.request_id.clone()),
                remap_policy: None,
            },
        )?;
        cfg.share_requests
            .retain(|pending| pending.request_id != request.request_id);
        Ok(())
    })
}

fn share_requests(home: &Path) -> Result<Vec<ShareRequest>> {
    Ok(DeviceConfig::load(home)?.share_requests)
}

fn reject_share_request(home: &Path, request_id: &str) -> Result<()> {
    let _session = session_guard(home)?;
    update(home, |cfg| {
        let index = cfg
            .share_requests
            .iter()
            .position(|request| request.request_id == request_id)
            .context("solicitação de Share não encontrada")?;
        cfg.rejected_requests.push(cfg.share_requests.remove(index));
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

fn reindex_share(home: &Path, id: &str) -> Result<()> {
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

fn remap_share(home: &Path, id: &str, policy: RemapPolicy) -> Result<()> {
    let _session = session_guard(home)?;
    let _config = config_guard(home)?;
    let mut config = DeviceConfig::load(home)?;
    let mut share = config
        .shares
        .iter()
        .find(|share| share.share_id == id)
        .context("unknown Share")?
        .clone();
    share.binding_revision = share
        .binding_revision
        .checked_add(1)
        .context("binding revision exhausted")?;
    share.remap_policy = Some(policy);
    config.put_share(home, share)?;
    backup_config(home, "remap")?;
    archive_derived_state(home, id, "remap")?;
    config.save(home)
}

fn validate_ignore(text: &str) -> Result<()> {
    rowd_core::ignore::Ignore::validate(text)
}

fn read_ignore(root: &Path) -> Result<String> {
    ensure!(
        root.is_dir(),
        "Share root unavailable while reading .rowdignore: {}",
        root.display()
    );
    ensure!(
        root.canonicalize()? == root,
        "Share root identity changed: {}",
        root.display()
    );
    let path = root.join(".rowdignore");
    match fs::symlink_metadata(&path) {
        Ok(meta) => ensure!(
            !meta.file_type().is_symlink(),
            "symlink .rowdignore is not an authoritative Share policy"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let policy = match fs::read_to_string(path) {
        Ok(policy) => policy,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    validate_ignore(&policy)?;
    Ok(policy)
}

fn set_ignore_text(home: &Path, id: &str, text: &str) -> Result<()> {
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

fn request_share_sync(home: &Path, id: &str) -> Result<()> {
    let cfg = DeviceConfig::load(home)?;
    let share = cfg
        .shares
        .iter()
        .find(|share| share.share_id == id)
        .context("unknown Share")?;
    ensure!(share.enabled, "Share is paused");
    atomic_json(&home.join(".rowd/next-share.json"), &id)
}

fn revoke_device_unlocked(home: &Path) -> Result<()> {
    let _lock = config_guard(home)?;
    let mut cfg = DeviceConfig::load(home)?;
    backup_config(home, "unlink")?;
    for share in &cfg.shares {
        archive_derived_state(home, &share.share_id, "unlink")?;
    }
    let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()])?;
    cfg.pair_id = random_id()?;
    cfg.cert = hex::encode(cert.cert.der());
    cfg.key = hex::encode(cert.key_pair.serialize_der());
    cfg.secret = random_id()?;
    cfg.peer_device = None;
    cfg.pending_unlink = false;
    cfg.share_requests.clear();
    cfg.rejected_requests.clear();
    cfg.save(home)
}

fn unlink_device(home: &Path) -> Result<()> {
    let _session = session_guard(home)?;
    update(home, |cfg| {
        if cfg.peer_device.is_some() {
            cfg.pending_unlink = true;
            Ok(())
        } else {
            anyhow::bail!("nenhum Android conectado ainda; use revogação imediata")
        }
    })
}

fn revoke_device(home: &Path) -> Result<()> {
    let _session = session_guard(home)?;
    revoke_device_unlocked(home)
}

fn reset_device_configuration(home: &Path) -> Result<()> {
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
        cert: hex::encode(cert.cert.der()),
        key: hex::encode(cert.key_pair.serialize_der()),
        secret: random_id()?,
        peer_device: None,
        shares: Vec::new(),
        share_requests: Vec::new(),
        rejected_requests: Vec::new(),
        sync_paused: false,
        pending_unlink: false,
        import_generation: None,
    }
    .save(home)
}

fn archive_all_application_data(home: &Path) -> Result<()> {
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

fn recovery(home: &Path) -> Result<Vec<RecoveryItem>> {
    let cfg = DeviceConfig::load(home)?;
    recovery_from_config(&cfg)
}

fn recovery_from_config(cfg: &DeviceConfig) -> Result<Vec<RecoveryItem>> {
    let mut items = Vec::new();
    for share in &cfg.shares {
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

fn resolve_recovery(
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

fn cleanup_recovery(home: &Path, share_id: &str, id: &str) -> Result<()> {
    let _session = session_guard(home)?;
    let share = share_by_id(home, share_id)?;
    LocalStore::open_recovery(&share.root)?.cleanup_recovery(id)
}

fn test_connection(home: &Path) -> Result<ConnectionTest> {
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

fn export_profile(home: &Path, output: &Path) -> Result<()> {
    let cfg = DeviceConfig::load(home)?;
    let profile = PublicProfile {
        version: 2,
        shares: cfg
            .shares
            .into_iter()
            .map(|share| {
                Ok(ProfileShare {
                    share_id: share.share_id,
                    name: share.name,
                    ignore: read_ignore(&share.root)?,
                    root: share.root,
                    mode: share.mode,
                    enabled: share.enabled,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        sync_paused: cfg.sync_paused,
    };
    write_private(output, &serde_json::to_vec_pretty(&profile)?)
}

fn import_profile(home: &Path, input: &Path) -> Result<()> {
    let _session = session_guard(home)?;
    let profile: PublicProfile = serde_json::from_reader(File::open(input)?)?;
    ensure!(
        profile.version == 1 || profile.version == 2,
        "unsupported profile version"
    );
    for share in &profile.shares {
        validate_ignore(&share.ignore)?;
        ensure!(read_ignore(&share.root)? == share.ignore,
            "profile ignore policy differs from the target .rowdignore; copy the policy to the Share root before importing");
    }
    update(home, |cfg| {
        let mut candidate = cfg.clone();
        candidate.shares.clear();
        for share in &profile.shares {
            let binding_revision = cfg
                .shares
                .iter()
                .find(|old| old.share_id == share.share_id)
                .map(|old| {
                    old.binding_revision
                        .checked_add(u64::from(profile.version == 1))
                        .context("binding revision exhausted")
                })
                .transpose()?
                .unwrap_or(0);
            candidate.put_share(
                home,
                ShareConfig {
                    share_id: share.share_id.clone(),
                    name: share.name.clone(),
                    root: share.root.clone(),
                    binding_revision,
                    mode: share.mode,
                    enabled: share.enabled,
                    legacy_ignore: String::new(),
                    request_id: None,
                    remap_policy: None,
                },
            )?;
        }
        candidate.sync_paused = profile.sync_paused;
        for old in &cfg.shares {
            let changed = candidate
                .shares
                .iter()
                .find(|share| share.share_id == old.share_id)
                .is_none_or(|imported| {
                    old.root != imported.root || old.binding_revision != imported.binding_revision
                });
            if changed {
                archive_derived_state(home, &old.share_id, "profile-import")?;
            }
        }
        *cfg = candidate;
        Ok(())
    })?;
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

fn encrypt_backup(value: &impl Serialize, passphrase: &str) -> Result<EncryptedBackup> {
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

fn export_backup(home: &Path, output: &Path, passphrase: &str) -> Result<()> {
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
        version: 2,
        ignore_policies: config
            .shares
            .iter()
            .map(|share| Ok((share.share_id.clone(), read_ignore(&share.root)?)))
            .collect::<Result<BTreeMap<_, _>>>()?,
        config,
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

fn import_backup(home: &Path, input: &Path, passphrase: &str) -> Result<()> {
    let _session = session_guard(home)?;
    ensure!(
        input.metadata()?.len() <= 256 * 1024 * 1024,
        "backup file is too large"
    );
    let encrypted: EncryptedBackup = serde_json::from_reader(File::open(input)?)?;
    let mut backup = decrypt_backup(encrypted, passphrase)?;
    ensure!(
        backup.version == 2,
        "unsupported full backup version; migrate with its original Rowd version"
    );
    ensure!(
        backup.config.version == CURRENT_DEVICE_VERSION,
        "backup configuration requires migration by its original Rowd version"
    );
    ensure!(
        backup
            .config
            .shares
            .iter()
            .all(|share| share.legacy_ignore.is_empty()),
        "backup contains unmigrated ignore rules"
    );
    invitation(&backup.config).validate()?;
    tls::server_config(&backup.config.cert, &backup.config.key)
        .context("backup contains an invalid TLS identity")?;
    let mut validated = backup.config.clone();
    validated.shares.clear();
    for share in backup.config.shares.clone() {
        let policy = backup
            .ignore_policies
            .get(&share.share_id)
            .context("backup is missing a Share ignore policy")?;
        validate_ignore(policy)?;
        ensure!(read_ignore(&share.root)? == *policy,
            "backup ignore policy differs from the target .rowdignore; restore that file before importing");
        validated.put_share(home, share)?;
    }
    ensure!(
        validated.shares.len() == backup.config.shares.len()
            && backup.ignore_policies.len() == backup.config.shares.len(),
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
        let path = safe_state_path(Path::new(""), &relative)?;
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
    let staging = tempfile::Builder::new()
        .prefix("shares-import-")
        .tempdir_in(home.join(".rowd"))?;
    let staged_state = staging.path().join("shares");
    fs::create_dir(&staged_state)?;
    for (path, bytes) in decoded_state {
        rowd_core::storage::atomic_write(&staged_state.join(path), &bytes)?;
    }
    #[cfg(unix)]
    File::open(&staged_state)?.sync_all()?;
    backup_config(home, "before-import")?;
    let generation = random_id()?;
    let archived_state = home
        .join(".rowd")
        .join(format!("shares-before-import-{generation}"));
    let had_state = state_root.exists();
    let marker = config::import_transaction_path(home);
    rowd_core::storage::atomic_json(
        &marker,
        &config::ImportTransaction {
            generation: generation.clone(),
            had_state,
        },
    )?;
    backup.config.import_generation = Some(generation);
    let commit = (|| -> Result<()> {
        if had_state {
            fs::rename(&state_root, &archived_state)?;
        }
        fs::rename(&staged_state, &state_root)?;
        #[cfg(unix)]
        File::open(home.join(".rowd"))?.sync_all()?;
        backup.config.save(home)
    })();
    if let Err(error) = commit {
        config::recover_import(home)?;
        return Err(error);
    }
    config::recover_import(home)?;
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
    last_round: Option<RoundMetrics>,
}

fn export_diagnostic(home: &Path, output: &Path) -> Result<()> {
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
        last_round: device_runtime(home).last_round,
    };
    write_private(output, &serde_json::to_vec_pretty(&report)?)
}
fn state_path(home: &Path, share: &ShareConfig) -> PathBuf {
    home.join(".rowd/shares")
        .join(&share.share_id)
        .join("base.json")
}
fn retire_journal(home: &Path, share: &ShareConfig) -> Result<()> {
    let path = home
        .join(".rowd/shares")
        .join(&share.share_id)
        .join("journal.json");
    if path.exists() {
        fs::rename(
            &path,
            path.with_extension(format!("retired-{}.json", random_id()?)),
        )?;
    }
    Ok(())
}
fn scan(home: &Path, full: bool) -> Result<()> {
    let _session = session_guard(home)?;
    let cfg = DeviceConfig::load(home)?;
    for share in &cfg.shares {
        if full {
            let _root_lock = LocalStore::open_existing(&share.root)?;
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
    retire_journal(home, share)?;
    let mut store = LocalStore::open_existing(&share.root)?;
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
    store.scan()?;
    Ok(())
}

fn invalidate_share_cache_serialized(
    home: &Path,
    share: &ShareConfig,
    full: bool,
    dirty: Option<&BTreeSet<String>>,
) -> Result<()> {
    let _session = session_guard(home)?;
    LocalStore::queue_cache_invalidation(&share.root, full, dirty)
}
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub share: ShareConfig,
    pub root_available: bool,
    pub conflicts: Vec<String>,
    pub last_sync: Option<u64>,
    pub error: Option<String>,
    pub last_error: Option<StoredError>,
}
fn status(home: &Path) -> Result<Vec<Status>> {
    let cfg = DeviceConfig::load(home)?;
    status_from_config(home, &cfg)
}

fn status_from_config(home: &Path, cfg: &DeviceConfig) -> Result<Vec<Status>> {
    cfg.shares
        .iter()
        .map(|share| {
            let state = State::load(&state_path(home, share), &cfg.pair_id, &share.share_id);
            let error = state.as_ref().err().map(ToString::to_string);
            let runtime = File::open(runtime_path(home, &share.share_id))
                .ok()
                .and_then(|file| serde_json::from_reader::<_, ShareRuntime>(file).ok());
            let last_error = runtime
                .as_ref()
                .and_then(|runtime| runtime.last_error.clone());
            Ok(Status {
                root_available: share.root.is_dir(),
                conflicts: state
                    .as_ref()
                    .map(|s| s.conflicts.iter().cloned().collect())
                    .unwrap_or_default(),
                last_sync: runtime
                    .and_then(|runtime| runtime.last_sync)
                    .or_else(|| state.as_ref().ok().and_then(|s| s.last_sync)),
                error,
                last_error,
                share: share.clone(),
            })
        })
        .collect()
}

fn session(
    home: &Path,
    socket: TcpStream,
    incremental_allowed: bool,
    wakes: mpsc::Receiver<String>,
    urgent: &Mutex<BTreeMap<String, bool>>,
    stop: &AtomicBool,
    once: bool,
    event: &impl Fn(String),
) -> Result<()> {
    let cfg = DeviceConfig::load(home)?;
    let mut io = tls::accept(socket, tls::server_config(&cfg.cert, &cfg.key)?)?;
    let device = protocol::server_auth(&mut io, &cfg.pair_id, &cfg.secret)?;
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
    }
    atomic_json(
        &device_runtime_path(home),
        &DeviceRuntime {
            last_connection: Some(now()),
            last_round: device_runtime(home).last_round,
        },
    )?;
    event("Android conectado".into());
    io.sock.set_read_timeout(Some(Duration::from_millis(50)))?;
    let mut audit_preempted = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut first = [0u8; 1];
        let buffered = match io.conn.reader().read(&mut first) {
            Ok(1) => true,
            Ok(0) => false,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(error) => return Err(error.into()),
            _ => unreachable!(),
        };
        let ready = if buffered {
            true
        } else {
            match io.sock.peek(&mut first) {
                Ok(0) => return Ok(()),
                Ok(_) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            }
        };
        if ready {
            io.sock.set_read_timeout(Some(Duration::from_secs(90)))?;
            let message = if buffered {
                protocol::receive_after_first(&mut io, first[0])?
            } else {
                protocol::receive(&mut io)?
            };
            ensure!(
                matches!(message, Message::StartRound),
                "expected round start"
            );
            let result = session_round(
                home,
                &mut io,
                &device,
                incremental_allowed,
                urgent,
                &mut audit_preempted,
                event,
            );
            if once {
                return result;
            }
            result?;
            let hint_wait = Instant::now();
            while urgent.lock().unwrap().values().any(|delivered| *delivered) {
                ensure!(
                    hint_wait.elapsed() < Duration::from_secs(5),
                    "watcher hint was not committed"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            io.sock.set_read_timeout(Some(Duration::from_millis(50)))?;
        }
        let mut pending = BTreeSet::new();
        while let Ok(id) = wakes.try_recv() {
            pending.insert(id);
        }
        for id in pending {
            protocol::send(
                &mut io,
                &Message::WakeShare {
                    share_id: id.clone(),
                },
            )?;
            event(format!(
                "wake_sent_at={} share={id}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis()
            ));
        }
    }
}

fn session_round(
    home: &Path,
    mut io: &mut (impl std::io::Read + std::io::Write),
    device: &str,
    incremental_allowed: bool,
    urgent: &Mutex<BTreeMap<String, bool>>,
    audit_preempted: &mut bool,
    event: &impl Fn(String),
) -> Result<()> {
    let started = Instant::now();
    let _session = session_guard(home)?;
    event(format!(
        "round_started_at={}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
    ));
    let mut cfg = DeviceConfig::load(home)?;
    let result = (|| -> Result<()> {
        let mut advertised = Vec::with_capacity(cfg.shares.len());
        for share in &cfg.shares {
            let (enabled, ignore_rules) = if share.enabled {
                match read_ignore(&share.root) {
                    Ok(rules) => (true, rules),
                    Err(error) => {
                        event(format!("{} indisponível: {error:#}", share.name));
                        (false, String::new())
                    }
                }
            } else {
                (false, String::new())
            };
            advertised.push(rowd_core::config::ShareDefinition {
                share_id: share.share_id.clone(),
                name: share.name.clone(),
                mode: share.mode,
                enabled,
                binding_revision: share.binding_revision,
                ignore_rules,
                request_id: share.request_id.clone(),
                remap_policy: share.remap_policy,
            });
        }
        protocol::send(&mut io, &Message::Shares { shares: advertised })?;
        let Message::Capabilities {
            device_id,
            share_requests,
            cancel_intents,
            available_shares,
            requested_share_ids,
            audit,
            unlink_requested,
            ..
        } = protocol::receive(&mut io)?
        else {
            anyhow::bail!("expected Android capabilities")
        };
        ensure!(device_id == device, "Android device/capabilities mismatch");
        if cfg.pending_unlink || unlink_requested {
            protocol::send(&mut io, &Message::DeviceUnlinked)?;
            ensure!(
                matches!(protocol::receive(&mut io)?, Message::UnlinkAck),
                "unlink acknowledgement missing"
            );
            revoke_device_unlocked(home)?;
            protocol::send(&mut io, &Message::UnlinkComplete)?;
            event("Android desvinculado; uma nova identidade de pareamento foi criada".into());
            return Ok(());
        }

        let available_shares: BTreeSet<_> = available_shares.into_iter().collect();
        for requested in &requested_share_ids {
            rowd_core::model::validate_hash(requested)?;
            ensure!(
                available_shares.contains(requested),
                "requested Share is unavailable"
            );
        }
        for id in &available_shares {
            rowd_core::model::validate_hash(id)?;
            ensure!(
                cfg.shares.iter().any(|share| &share.share_id == id),
                "Android advertised an unknown Share"
            );
        }

        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        let mut cancelled = Vec::new();
        let incoming_request_ids = share_requests
            .iter()
            .map(|request| request.request_id.clone())
            .collect::<BTreeSet<_>>();
        ensure!(
            incoming_request_ids.len() == share_requests.len(),
            "duplicate Share request ID"
        );
        let cancelled_ids = cancel_intents.iter().cloned().collect::<BTreeSet<_>>();
        ensure!(
            cancelled_ids.len() == cancel_intents.len(),
            "duplicate cancellation intent"
        );
        ensure!(
            cancelled_ids.is_disjoint(&incoming_request_ids),
            "request is both pending and cancelled"
        );
        update_runtime(home, |latest| {
            for id in cancel_intents {
                rowd_core::model::validate_hash(&id)?;
                latest
                    .share_requests
                    .retain(|request| request.request_id != id);
                latest
                    .rejected_requests
                    .retain(|request| request.request_id != id);
                cancelled.push(id);
            }
            for request in share_requests {
                rowd_core::model::validate_hash(&request.request_id)?;
                ensure!(
                    !request.name.trim().is_empty()
                        && request.name.len() <= 120
                        && !request.name.chars().any(char::is_control),
                    "invalid Share request name"
                );
                let id = request.request_id.clone();
                if let Some(stored) = latest
                    .share_requests
                    .iter()
                    .chain(&latest.rejected_requests)
                    .find(|stored| stored.request_id == id)
                {
                    ensure!(
                        stored.name == request.name && stored.mode == request.mode,
                        "Share request ID reused with different payload"
                    );
                }
                if let Some(share) = latest
                    .shares
                    .iter()
                    .find(|share| share.request_id.as_deref() == Some(request.request_id.as_str()))
                {
                    ensure!(
                        share.name == request.name && share.mode == request.mode,
                        "accepted Share request ID reused with different payload"
                    );
                    accepted.push(id);
                    continue;
                }
                if latest
                    .rejected_requests
                    .iter()
                    .any(|stored| stored.request_id == id)
                {
                    rejected.push(id);
                } else if !latest
                    .share_requests
                    .iter()
                    .any(|stored| stored.request_id == id)
                {
                    latest.share_requests.push(request);
                }
            }
            latest
                .rejected_requests
                .retain(|request| incoming_request_ids.contains(&request.request_id));
            ensure!(
                latest.share_requests.len() + latest.rejected_requests.len() <= 512,
                "too many Share requests"
            );
            Ok(())
        })?;
        cfg = DeviceConfig::load(home)?;
        protocol::send(
            &mut io,
            &Message::ShareRequestStatus {
                accepted,
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
                    && requested_share_ids.contains(&share.share_id)
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
        let mut round_metrics = RoundMetrics::default();
        let mut deferred = false;
        let mut deferred_shares = Vec::new();
        for (index, share) in ready.iter().enumerate() {
            if audit && !*audit_preempted {
                let mut pending = urgent.lock().unwrap();
                if !pending.is_empty() {
                    deferred_shares = pending.keys().cloned().collect();
                    for delivered in pending.values_mut() {
                        *delivered = true;
                    }
                    deferred = true;
                    *audit_preempted = true;
                    event(format!(
                        "Auditoria interrompida antes de {}; priorizando mudança do PC",
                        share.name
                    ));
                    break;
                }
            }
            let share_started_ms = started.elapsed().as_millis();
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
            let mut selected = false;
            let result = (|| -> Result<_> {
                retire_journal(home, share)?;
                let mut store = LocalStore::open_existing(&share.root)?;
                if incremental_allowed {
                    store.allow_incremental_scan();
                } else {
                    store.invalidate();
                }
                let path = state_path(home, share);
                let mut state = State::load(&path, &cfg.pair_id, &share.share_id)?;
                protocol::send(
                    &mut io,
                    &Message::SelectShare {
                        share_id: share.share_id.clone(),
                    },
                )?;
                selected = true;
                ensure!(
                    matches!(protocol::receive(&mut io)?, Message::Ready),
                    "Share not ready"
                );
                let mut last_progress = Instant::now() - Duration::from_secs(1);
                sync::coordinate_with_progress(
                    &mut io,
                    &mut store,
                    &mut state,
                    &path,
                    share.mode,
                    share.remap_policy,
                    |path, done, total| {
                        if path.is_empty() || last_progress.elapsed() >= Duration::from_millis(250)
                        {
                            event(format!("{}: {done}/{total} caminhos · {path}", share.name));
                            last_progress = Instant::now();
                        }
                    },
                )
            })()
            .with_context(|| format!("Share {} ({})", share.name, share.root.display()));
            let mut report = match result {
                Ok(report) => report,
                Err(error) => {
                    record_share_error(home, &share.share_id, operation, &error);
                    if selected {
                        return Err(error);
                    }
                    protocol::send(
                        &mut io,
                        &Message::ShareSkipped {
                            share_id: share.share_id.clone(),
                            reason: format!("{error:#}"),
                        },
                    )?;
                    continue;
                }
            };
            report.metrics.share_queue_wait_ms = share_started_ms;
            resolve_share_error(home, &share.share_id);
            round_metrics.shares_processed += 1;
            round_metrics.shares_changed +=
                usize::from(report.transferred > 0 || report.conflicts > 0);
            round_metrics.bytes_transferred += report.metrics.bytes_transferred;
            if round_metrics.ttfb_ms.is_none() {
                round_metrics.ttfb_ms =
                    report.metrics.first_byte_ms.map(|ms| share_started_ms + ms);
            }
            round_metrics
                .per_share
                .insert(share.share_id.clone(), report.metrics.clone());
            if share.remap_policy.is_some() {
                completed_remaps.push(share.share_id.clone());
            }
            event(format!(
                "{}: {} transferências, {} conflitos",
                share.name, report.transferred, report.conflicts
            ));
        }
        round_metrics.total_ms = started.elapsed().as_millis();
        let processed_any = round_metrics.shares_processed > 0;
        event(format!(
            "round_completed_at={} first_byte_ms={:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
            round_metrics.ttfb_ms
        ));
        atomic_json(
            &device_runtime_path(home),
            &DeviceRuntime {
                last_connection: Some(now()),
                last_round: Some(round_metrics),
            },
        )?;
        update_runtime(home, |latest| {
            for share in &mut latest.shares {
                if completed_remaps.contains(&share.share_id) {
                    share.remap_policy = None;
                }
            }
            Ok(())
        })?;
        if requested_share.is_some() && processed_any {
            let _ = fs::remove_file(home.join(".rowd/next-share.json"));
        }
        protocol::send(
            &mut io,
            &if deferred {
                Message::RoundDeferred {
                    shares: deferred_shares,
                }
            } else {
                Message::SessionDone
            },
        )?;
        if audit && !deferred {
            *audit_preempted = false;
        }
        event(
            if deferred {
                "Android: auditoria adiada para priorizar mudança do PC"
            } else {
                "Android: rodada concluída; aguardando mudanças"
            }
            .into(),
        );
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

fn serve(
    home: &Path,
    listen: Option<&str>,
    once: bool,
    stop: Arc<AtomicBool>,
    event: impl Fn(String) + Sync,
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
    let mut watcher_untrusted = watcher.is_none();
    let mut watched: BTreeMap<String, ShareConfig> = BTreeMap::new();
    let mut dirty: BTreeMap<String, (Instant, BTreeSet<String>)> = BTreeMap::new();
    let mut full = false;
    let mut last_fallback = Instant::now();
    let mut last_full = Instant::now();
    let mut last_config = Instant::now() - Duration::from_secs(2);
    let urgent = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| -> Result<()> {
        let mut connected = None;
        let mut connected_socket: Option<TcpStream> = None;
        let mut wake_tx: Option<mpsc::Sender<String>> = None;
        while !stop.load(Ordering::Relaxed) {
            if connected
                .as_ref()
                .is_some_and(std::thread::ScopedJoinHandle::is_finished)
            {
                if let Some(handle) = connected.take() {
                    if let Err(error) = handle.join().expect("session thread panicked") {
                        event(format!("Rodada interrompida: {error:#}"));
                    }
                }
                wake_tx = None;
                connected_socket = None;
            }
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
                                watcher_untrusted = true;
                                event(format!(
                                    "{}: watcher indisponível; usando scan periódico: {e}",
                                    share.name
                                ));
                            }
                        }
                        if let Err(e) = invalidate_share_cache_serialized(home, share, true, None) {
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
                        let structural = matches!(
                            kind,
                            notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                                | notify::EventKind::Remove(_)
                                | notify::EventKind::Create(notify::event::CreateKind::Folder)
                        );
                        if matches!(kind, notify::EventKind::Access(_)) {
                            continue;
                        }
                        for share in watched.values() {
                            let ignore = rowd_core::ignore::Ignore::parse(
                                &fs::read_to_string(share.root.join(".rowdignore"))
                                    .unwrap_or_default(),
                            );
                            for path in &paths {
                                if let Ok(relative) = path.strip_prefix(&share.root) {
                                    let relative = relative.to_string_lossy().to_string();
                                    if relative.is_empty()
                                        || relative == ".rowdignore"
                                        || !ignore.matches(&relative, path.is_dir())
                                    {
                                        if !dirty.contains_key(&share.share_id) {
                                            event(format!(
                                                "change_detected_at={} share={}",
                                                std::time::SystemTime::now()
                                                    .duration_since(std::time::UNIX_EPOCH)?
                                                    .as_millis(),
                                                share.share_id
                                            ));
                                        }
                                        let entry = dirty
                                            .entry(share.share_id.clone())
                                            .or_insert_with(|| (Instant::now(), BTreeSet::new()));
                                        entry.0 = Instant::now();
                                        if structural {
                                            entry.1.clear();
                                            entry.1.insert(String::new());
                                        } else if !entry.1.contains("") {
                                            entry.1.insert(relative);
                                        }
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
            if fallback && watcher_untrusted {
                full = true;
            }
            if last_full.elapsed() >= Duration::from_secs(15 * 60) {
                full = true;
            }
            for share in watched.values() {
                if full
                    || fallback
                    || dirty
                        .get(&share.share_id)
                        .is_some_and(|(t, _)| t.elapsed() >= Duration::from_millis(150))
                {
                    let changed = dirty.contains_key(&share.share_id);
                    let wake = full || changed;
                    if !share.root.is_dir() {
                        let (_, paths) = dirty
                            .entry(share.share_id.clone())
                            .or_insert_with(|| (Instant::now(), BTreeSet::new()));
                        paths.clear();
                        paths.insert(String::new());
                        continue;
                    }
                    if changed && connected.is_some() {
                        urgent.lock().unwrap().insert(share.share_id.clone(), false);
                    }
                    invalidate_share_cache_serialized(
                        home,
                        share,
                        full,
                        dirty.get(&share.share_id).map(|(_, paths)| paths),
                    )
                    .with_context(|| format!("watcher hint for {}", share.name))?;
                    dirty.remove(&share.share_id);
                    let delivered = urgent
                        .lock()
                        .unwrap()
                        .remove(&share.share_id)
                        .unwrap_or(false);
                    if wake && !delivered {
                        if let Some(tx) = &wake_tx {
                            let _ = tx.send(share.share_id.clone());
                            event(format!("{}: wake enviado", share.name));
                        }
                    }
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
                    if let Some(old) = connected_socket.take() {
                        let _ = old.shutdown(std::net::Shutdown::Both);
                    }
                    if let Some(handle) = connected.take() {
                        if let Err(error) = handle.join().expect("session thread panicked") {
                            event(format!("Sessão anterior encerrada: {error:#}"));
                        }
                    }
                    for (id, (_, paths)) in std::mem::take(&mut dirty) {
                        if let Some(share) = watched.get(&id) {
                            if !share.root.is_dir() {
                                dirty.insert(id, (Instant::now(), BTreeSet::from([String::new()])));
                                continue;
                            }
                            invalidate_share_cache_serialized(home, share, false, Some(&paths))
                                .with_context(|| format!("watcher hint for {}", share.name))?;
                        }
                    }
                    let (tx, wakes) = mpsc::channel();
                    wake_tx = Some(tx);
                    connected_socket = Some(socket.try_clone()?);
                    let urgent = &urgent;
                    let stop = &stop;
                    let event = &event;
                    let handle = scope.spawn(move || {
                        session(
                            home,
                            socket,
                            !watcher_untrusted,
                            wakes,
                            &urgent,
                            stop,
                            once,
                            event,
                        )
                    });
                    if once {
                        return handle.join().expect("session thread panicked");
                    }
                    connected = Some(handle);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => return Err(e.into()),
            }
        }
        drop(wake_tx);
        if let Some(handle) = connected {
            handle.join().expect("session thread panicked")?;
        }
        Ok(())
    })
}

fn qr(cfg: &DeviceConfig) -> Result<String> {
    let text = pairing_payload(cfg)?;
    let code = qrcode::QrCode::new(text.as_bytes())?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build())
}

fn export_qr(home: &Path, cfg: &DeviceConfig) -> Result<PathBuf> {
    let code = qrcode::QrCode::new(pairing_payload(cfg)?.as_bytes())?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(768, 768)
        .build();
    let path = home.join(".rowd/pairing.svg");
    rowd_core::storage::atomic_write(&path, svg.as_bytes())?;
    Ok(path)
}

fn pairing_payload(cfg: &DeviceConfig) -> Result<String> {
    invitation(cfg).encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_and_pairing_are_derived() {
        let directory = tempfile::tempdir().unwrap();
        let empty = App::new(directory.path());
        let snapshot = empty.snapshot().unwrap();
        assert!(!snapshot.device.configured);
        assert!(snapshot.shares.is_empty());
        let (_directory, app, _share) = configured_app();
        let snapshot = app.snapshot().unwrap();
        assert!(snapshot.device.configured);
        let pairing = app.pairing_info().unwrap();
        assert_eq!(pairing.device.fingerprint, snapshot.device.fingerprint);
        assert!(!pairing.qr.is_empty());
        assert!(pairing.qr_image.exists());
    }

    #[test]
    fn old_ui_fields_are_accepted_without_touching_local_preferences() {
        let (directory, app, _share) = configured_app();
        let ui_path = app.home().join(".rowd/ui.json");
        fs::write(&ui_path, br#"{"local":true}"#).unwrap();
        let profile_path = directory.path().join("profile.json");
        app.export_profile(&profile_path).unwrap();
        let mut profile: serde_json::Value =
            serde_json::from_slice(&fs::read(&profile_path).unwrap()).unwrap();
        assert!(profile.get("ui").is_none());
        profile["ui"] = serde_json::json!({"legacy": true});
        fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
        app.import_profile(&profile_path).unwrap();
        assert_eq!(fs::read(&ui_path).unwrap(), br#"{"local":true}"#);

        let backup_path = directory.path().join("backup.json");
        app.export_backup(&backup_path, "passphrase").unwrap();
        let encrypted: EncryptedBackup =
            serde_json::from_slice(&fs::read(&backup_path).unwrap()).unwrap();
        let backup = decrypt_backup(encrypted, "passphrase").unwrap();
        let mut legacy = serde_json::to_value(&backup).unwrap();
        assert!(legacy.get("ui").is_none());
        legacy["ui"] = serde_json::json!({"legacy": true});
        let encrypted = encrypt_backup(&legacy, "passphrase").unwrap();
        fs::write(&backup_path, serde_json::to_vec(&encrypted).unwrap()).unwrap();
        app.import_backup(&backup_path, "passphrase").unwrap();
        assert_eq!(fs::read(&ui_path).unwrap(), br#"{"local":true}"#);
    }

    #[test]
    fn reset_keeps_global_lock_inode_stable() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        fs::create_dir(home.join(".rowd")).unwrap();
        let held = config_guard(home).unwrap();
        archive_all_application_data(home).unwrap();
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(home.join(".rowd-locks/config.lock"))
            .unwrap();
        assert!(contender.try_lock_exclusive().is_err());
        drop(held);
    }

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
        assert_eq!(bytes[0], 2);
        assert_eq!(
            u32::from_be_bytes(bytes[1..5].try_into().unwrap()),
            INVITATION_VERSION
        );
        assert!(payload.len() < serde_json::to_string(&invitation(&config)).unwrap().len());
        for credential in [&config.pair_id, &config.secret] {
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
            .add_share("Docs".into(), share.clone(), SyncMode::Bidirectional)
            .unwrap();
        fs::write(share.join(".rowdignore"), "*.tmp\n").unwrap();
        app.set_sync_paused(true).unwrap();
        let backup = directory.path().join("rowd-backup.json");
        app.export_backup(&backup, "correct horse").unwrap();

        app.set_sync_paused(false).unwrap();
        app.set_share_enabled(&share_id, false).unwrap();
        assert!(app.import_backup(&backup, "wrong passphrase").is_err());
        assert!(!DeviceConfig::load(app.home()).unwrap().sync_paused);

        fs::write(share.join(".rowdignore"), "changed/\n").unwrap();
        assert!(app
            .import_backup(&backup, "correct horse")
            .unwrap_err()
            .to_string()
            .contains("ignore policy differs"));
        fs::write(share.join(".rowdignore"), "*.tmp\n").unwrap();

        app.import_backup(&backup, "correct horse").unwrap();
        let restored = DeviceConfig::load(app.home()).unwrap();
        assert!(restored.sync_paused);
        assert!(restored.shares[0].enabled);
    }

    #[test]
    fn failed_backup_materialization_preserves_active_state() {
        let (directory, app, share) = configured_app();
        let id = app
            .add_share("Docs".into(), share, SyncMode::Bidirectional)
            .unwrap();
        let state_dir = app.home().join(".rowd/shares").join(&id);
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("base.json"), b"active base").unwrap();
        let config = DeviceConfig::load(app.home()).unwrap();
        let backup = FullBackup {
            version: 2,
            config: config.clone(),
            ignore_policies: BTreeMap::from([(id.clone(), String::new())]),
            state: BTreeMap::from([
                ("collision".into(), hex::encode(b"file")),
                ("collision/child".into(), hex::encode(b"child")),
            ]),
        };
        let encrypted = encrypt_backup(&backup, "correct horse").unwrap();
        let input = directory.path().join("bad-backup.json");
        fs::write(&input, serde_json::to_vec(&encrypted).unwrap()).unwrap();
        assert!(app.import_backup(&input, "correct horse").is_err());
        assert_eq!(
            fs::read(state_dir.join("base.json")).unwrap(),
            b"active base"
        );
        assert_eq!(
            DeviceConfig::load(app.home()).unwrap().shares[0].share_id,
            id
        );
    }

    #[test]
    fn interrupted_backup_import_recovers_the_matching_generation() {
        for boundary in [
            "before_rename",
            "after_archive",
            "after_stage",
            "after_config",
        ] {
            let (_directory, app, share) = configured_app();
            let id = app
                .add_share("Docs".into(), share, SyncMode::Bidirectional)
                .unwrap();
            let home = app.home();
            let state = home.join(".rowd/shares");
            fs::create_dir_all(state.join(&id)).unwrap();
            fs::write(state.join(&id).join("base.json"), b"old").unwrap();
            let generation = "a".repeat(64);
            let archive = home
                .join(".rowd")
                .join(format!("shares-before-import-{generation}"));
            atomic_json(
                &config::import_transaction_path(home),
                &config::ImportTransaction {
                    generation: generation.clone(),
                    had_state: true,
                },
            )
            .unwrap();
            if boundary != "before_rename" {
                fs::rename(&state, &archive).unwrap();
            }
            if boundary == "after_stage" || boundary == "after_config" {
                fs::create_dir_all(state.join(&id)).unwrap();
                fs::write(state.join(&id).join("base.json"), b"new").unwrap();
            }
            if boundary == "after_config" {
                let mut config: DeviceConfig =
                    serde_json::from_reader(File::open(home.join(".rowd/device.json")).unwrap())
                        .unwrap();
                config.import_generation = Some(generation);
                config.save(home).unwrap();
            }
            DeviceConfig::load(home).unwrap();
            assert!(!config::import_transaction_path(home).exists());
            assert_eq!(
                fs::read(state.join(&id).join("base.json")).unwrap(),
                if boundary == "after_config" {
                    b"new"
                } else {
                    b"old"
                }
            );
        }
    }

    #[test]
    fn profile_import_invalidates_state_when_share_root_changes() {
        let (directory, app, old_root) = configured_app();
        let id = app
            .add_share("Docs".into(), old_root, SyncMode::Bidirectional)
            .unwrap();
        let derived = app.home().join(".rowd/shares").join(&id);
        fs::create_dir_all(&derived).unwrap();
        fs::write(derived.join("base.json"), b"old base").unwrap();
        let profile = directory.path().join("profile.json");
        app.export_profile(&profile).unwrap();
        let new_root = directory.path().join("new-root");
        fs::create_dir(&new_root).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&profile).unwrap()).unwrap();
        value["shares"][0]["root"] = serde_json::json!(new_root);
        fs::write(&profile, serde_json::to_vec(&value).unwrap()).unwrap();
        app.import_profile(&profile).unwrap();
        assert!(!derived.join("base.json").exists());
        assert!(fs::read_dir(&derived).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("base.json.profile-import-")));
    }

    #[test]
    fn remap_invalidates_binding_without_a_fake_android_path() {
        let (_directory, app, share) = configured_app();
        let id = app
            .add_share("Docs".into(), share, SyncMode::Bidirectional)
            .unwrap();
        let before = DeviceConfig::load(app.home()).unwrap();
        assert_eq!(before.shares[0].binding_revision, 0);
        app.remap_share(&id, RemapPolicy::Compare).unwrap();
        let after = DeviceConfig::load(app.home()).unwrap();
        assert_eq!(after.shares[0].binding_revision, 1);
        assert_eq!(after.shares[0].remap_policy, Some(RemapPolicy::Compare));
        assert_eq!(after.shares[0].root, before.shares[0].root);
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
            });
            Ok(())
        })
        .unwrap();
        app.reject_share_request(&request_id).unwrap();
        assert!(app.share_requests().unwrap().is_empty());
        assert_eq!(
            DeviceConfig::load(app.home()).unwrap().rejected_requests[0].request_id,
            request_id
        );
    }
}
