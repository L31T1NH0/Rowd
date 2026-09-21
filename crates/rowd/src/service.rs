use anyhow::{ensure, Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use rowd_core::{
    config::{DeviceConfig, ShareConfig, ShareRequest, SyncMode},
    journal::{ShareState, TrackedStore},
    model::{Invitation, VERSION},
    protocol::{self, Message},
    random_id,
    storage::{atomic_json, LocalStore, Store},
    sync::{self, State},
    tls,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::{IpAddr, TcpListener, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

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
    let _lock = LocalStore::open(home)?;
    if home.join(".rowd/device.json").exists() {
        let mut cfg = DeviceConfig::load(home)?;
        cfg.address = published_address(address)?;
        cfg.save(home)?;
        return Ok(cfg);
    }
    let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()])?;
    let cfg = DeviceConfig {
        version: 2,
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
    };
    invitation(&cfg).validate()?;
    cfg.save(home)?;
    Ok(cfg)
}
pub fn migrate(home: &Path, folder: &Path, address: &str) -> Result<()> {
    let _lock = LocalStore::open(home)?;
    ensure!(
        !home.join(".rowd/device.json").exists(),
        "device already configured"
    );
    let store = LocalStore::open(folder)?;
    let old = crate::config(&store)?;
    let state = State::load(
        &store.private().join("sync-state.json"),
        &old.pair_id,
        &old.folder_id,
    )?;
    let mut cfg = DeviceConfig {
        version: 2,
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
            ignore: String::new(),
            request_id: None,
        },
    )?;
    atomic_json(&state_path(home, &cfg.shares[0]), &state)?;
    cfg.save(home)
}
pub fn update(home: &Path, action: impl FnOnce(&mut DeviceConfig) -> Result<()>) -> Result<()> {
    let _lock = LocalStore::open(home)?;
    let mut cfg = DeviceConfig::load(home)?;
    action(&mut cfg)?;
    cfg.save(home)
}

pub fn accept_share_request(home: &Path, request_id: &str, folder: &Path) -> Result<()> {
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
                android_path: request.name.clone(),
                mode: request.mode,
                ignore: String::new(),
                request_id: Some(request.request_id.clone()),
            },
        )?;
        cfg.share_requests
            .retain(|pending| pending.request_id != request.request_id);
        Ok(())
    })
}

pub fn pending_share_requests(home: &Path) -> Result<Vec<ShareRequest>> {
    Ok(DeviceConfig::load(home)?.share_requests)
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
#[derive(Serialize)]
pub struct Status {
    pub share: ShareConfig,
    pub pending: usize,
    pub pending_paths: Vec<String>,
    pub conflicts: Vec<String>,
    pub last_sync: Option<u64>,
    pub error: Option<String>,
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
            Ok(Status {
                pending: pending_paths.len(),
                pending_paths,
                conflicts: state
                    .as_ref()
                    .map(|s| s.conflicts.iter().cloned().collect())
                    .unwrap_or_default(),
                last_sync: state.as_ref().ok().and_then(|s| s.last_sync),
                error,
                share,
            })
        })
        .collect()
}

fn session(home: &Path, socket: TcpStream, event: &impl Fn(String)) -> Result<()> {
    let _lock = LocalStore::open(home)?;
    let mut cfg = DeviceConfig::load(home)?;
    let mut io = tls::accept(socket, tls::server_config(&cfg.cert, &cfg.key)?)?;
    let result = (|| -> Result<()> {
        let device = protocol::server_auth(&mut io, &cfg.pair_id, &cfg.folder_id, &cfg.secret)?;
        if let Some(peer) = &cfg.peer_device {
            ensure!(peer == &device, "another Android device is already paired");
        } else {
            cfg.peer_device = Some(device.clone());
            cfg.save(home)?;
        }
        event("Android conectado".into());
        let mut advertised = cfg.shares.clone();
        for share in &mut advertised {
            share.ignore.push_str(&format!(
                "\n{}",
                fs::read_to_string(share.root.join(".rowdignore")).unwrap_or_default()
            ));
        }
        protocol::send(
            &mut io,
            &Message::Shares {
                shares: advertised,
                removed: cfg.removed.clone(),
            },
        )?;
        let Message::Capabilities {
            device_id,
            managed_shares,
            share_requests,
            available_shares,
            ..
        } = protocol::receive(&mut io)?
        else {
            anyhow::bail!("expected Android capabilities")
        };
        ensure!(
            device_id == device && managed_shares,
            "Android device/capabilities mismatch"
        );
        let available_shares: BTreeSet<_> = available_shares.into_iter().collect();
        for id in &available_shares {
            rowd_core::model::validate_hash(id)?;
            ensure!(
                cfg.shares.iter().any(|share| &share.share_id == id),
                "Android advertised an unknown Share"
            );
        }
        let mut accepted_requests = Vec::new();
        let mut pending_requests = Vec::new();
        let mut requests_changed = false;
        for request in share_requests {
            rowd_core::model::validate_hash(&request.request_id)?;
            ensure!(
                !request.name.trim().is_empty()
                    && request.name.len() <= 120
                    && !request.name.chars().any(char::is_control),
                "invalid Share request name"
            );
            if cfg
                .shares
                .iter()
                .any(|share| share.request_id.as_deref() == Some(request.request_id.as_str()))
            {
                accepted_requests.push(request.request_id);
            } else if cfg
                .share_requests
                .iter()
                .any(|pending| pending.request_id == request.request_id)
            {
                pending_requests.push(request.request_id);
            } else {
                pending_requests.push(request.request_id.clone());
                cfg.share_requests.push(request);
                requests_changed = true;
            }
        }
        if requests_changed {
            cfg.save(home)?;
        }
        protocol::send(
            &mut io,
            &Message::ShareRequestStatus {
                accepted: accepted_requests,
                pending: pending_requests,
            },
        )?;
        let ready: Vec<_> = cfg
            .shares
            .iter()
            .filter(|share| available_shares.contains(&share.share_id))
            .collect();
        let waiting: Vec<_> = cfg
            .shares
            .iter()
            .filter(|share| !available_shares.contains(&share.share_id))
            .map(|share| share.name.as_str())
            .collect();
        if !waiting.is_empty() {
            event(format!(
                "Aguardando pasta Android para: {}",
                waiting.join(", ")
            ));
        }
        for (index, share) in ready.iter().enumerate() {
            event(format!(
                "Sincronizando {} ({}/{})",
                share.name,
                index + 1,
                ready.len()
            ));
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
                sync::coordinate_with_progress(
                    &mut io,
                    &mut store,
                    &mut state,
                    &path,
                    share.mode,
                    |path, done, total| {
                        event(format!("{}: {done}/{total} caminhos · {path}", share.name))
                    },
                )
            })()
            .with_context(|| format!("Share {} ({})", share.name, share.root.display()));
            let report = result?;
            event(format!(
                "{}: {} transferências, {} conflitos",
                share.name, report.transferred, report.conflicts
            ));
        }
        protocol::send(&mut io, &Message::SessionDone)?;
        cfg.removed.clear();
        cfg.save(home)?;
        event("Android: última rodada concluída; aguardando conexão".into());
        Ok(())
    })();
    if let Err(ref e) = result {
        let _ = protocol::send(
            &mut io,
            &Message::Error {
                message: format!("{e:#}"),
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
            for share in &cfg.shares {
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
                    if let Err(e) =
                        scan_share(home, share, !journal_path(home, share).exists(), None)
                    {
                        event(format!("{}: {e:#}", share.name));
                    }
                }
            }
            watched = cfg
                .shares
                .into_iter()
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
                if let Err(e) = scan_share(
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
    let text = serde_json::to_string(&invitation(cfg))?;
    let code = qrcode::QrCode::new(text.as_bytes())?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build())
}

pub fn export_qr(home: &Path, cfg: &DeviceConfig) -> Result<PathBuf> {
    let code = qrcode::QrCode::new(serde_json::to_vec(&invitation(cfg))?)?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(768, 768)
        .build();
    let path = home.join(".rowd/pairing.svg");
    rowd_core::storage::atomic_write(&path, svg.as_bytes())?;
    Ok(path)
}
