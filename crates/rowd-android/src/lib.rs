use anyhow::{ensure, Context, Result};
use jni::{
    objects::{JObject, JString, JValue},
    sys::{jboolean, jstring},
    JNIEnv,
};
use rowd_core::{
    model::{Entry, Invitation, Manifest},
    storage::{Store, VerifiedStaged},
};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use std::time::Instant;
use tempfile::{NamedTempFile, TempPath};

static CANCELLED: AtomicBool = AtomicBool::new(false);
static BASE_TOKENS: OnceLock<Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();
static CONNECTION: OnceLock<Mutex<Option<(String, rowd_core::tls::ClientStream)>>> =
    OnceLock::new();
static RESOLVER: OnceLock<Mutex<rowd_core::discovery::EndpointResolver>> = OnceLock::new();
static NETWORK_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CONNECTION_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ACTIVE_SOCKET: OnceLock<Mutex<Option<TcpStream>>> = OnceLock::new();
static PAIRING_CONNECTION: OnceLock<
    Mutex<
        Option<(
            rowd_core::pairing::Peer,
            String,
            rowd_core::tls::ClientStream,
        )>,
    >,
> = OnceLock::new();

fn pairing_connection() -> &'static Mutex<
    Option<(
        rowd_core::pairing::Peer,
        String,
        rowd_core::tls::ClientStream,
    )>,
> {
    PAIRING_CONNECTION.get_or_init(|| Mutex::new(None))
}

fn pairing_result(env: JNIEnv, result: Result<serde_json::Value>) -> jstring {
    let output = match result {
        Ok(value) => value,
        Err(error) => serde_json::json!({"error":format!("{error:#}")}),
    };
    env.new_string(output.to_string())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_discoverPairing(
    env: JNIEnv,
    _class: JObject,
) -> jstring {
    let result = (|| -> Result<serde_json::Value> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let peers = rowd_core::pairing::discover(
            &socket,
            rowd_core::pairing::DeviceType::Pc,
            (rowd_core::pairing::GROUP, rowd_core::pairing::PORT).into(),
        )?;
        Ok(serde_json::to_value(peers)?)
    })();
    pairing_result(env, result)
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_pollPairOffer(
    mut env: JNIEnv,
    _class: JObject,
    name: JString,
    session: JString,
) -> jstring {
    let result = (|| -> Result<serde_json::Value> {
        let name: String = env.get_string(&name)?.into();
        let session_id: String = env.get_string(&session)?.into();
        let socket =
            std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, rowd_core::pairing::PORT))?;
        socket.set_read_timeout(Some(Duration::from_millis(900)))?;
        let _ =
            socket.join_multicast_v4(&rowd_core::pairing::GROUP, &std::net::Ipv4Addr::UNSPECIFIED);
        let mut buf = [0u8; 4097];
        let result = match socket.recv_from(&mut buf) {
            Ok((len, source)) => match rowd_core::pairing::decode(&buf[..len])? {
                rowd_core::pairing::Packet::Discover {
                    nonce,
                    device_type: rowd_core::pairing::DeviceType::Android,
                } => {
                    let reply = rowd_core::pairing::Packet::Announce {
                        nonce,
                        session_id,
                        device_name: name,
                        device_type: rowd_core::pairing::DeviceType::Android,
                        pair_id: None,
                        fingerprint: None,
                        cert_der: None,
                        tcp_port: None,
                    };
                    socket.send_to(&rowd_core::pairing::encode(&reply)?, source)?;
                    serde_json::Value::Null
                }
                packet @ rowd_core::pairing::Packet::Offer { .. } => {
                    serde_json::to_value(rowd_core::pairing::offered_peer(packet, source)?)?
                }
                _ => serde_json::Value::Null,
            },
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                serde_json::Value::Null
            }
            Err(error) => return Err(error.into()),
        };
        Ok(result)
    })();
    pairing_result(env, result)
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_beginPairing(
    mut env: JNIEnv,
    _class: JObject,
    peer: JString,
    device_id: JString,
    name: JString,
) -> jstring {
    let result = (|| -> Result<serde_json::Value> {
        let peer: rowd_core::pairing::Peer =
            serde_json::from_str(&String::from(env.get_string(&peer)?))?;
        rowd_core::pairing::validate_pc_peer(&peer)?;
        let device_id: String = env.get_string(&device_id)?.into();
        let name: String = env.get_string(&name)?.into();
        let mut io = rowd_core::tls::connect_pinned(
            peer.cert_der
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("missing certificate"))?,
            &peer.endpoint.to_string(),
            Duration::from_secs(5),
        )?;
        io.sock.set_read_timeout(Some(Duration::from_secs(65)))?;
        let (_, code) = rowd_core::pairing::request(&mut io, &peer, &device_id, &name)?;
        *pairing_connection().lock().unwrap() = Some((peer, device_id, io));
        Ok(serde_json::json!({"verification_code":code}))
    })();
    pairing_result(env, result)
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_finishPairing(
    env: JNIEnv,
    _class: JObject,
) -> jstring {
    let result = (|| -> Result<serde_json::Value> {
        let (peer, device_id, mut io) = pairing_connection()
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow::anyhow!("no pairing in progress"))?;
        let invitation = rowd_core::pairing::finish(&mut io, &peer, &device_id)?;
        Ok(serde_json::json!({"invitation":invitation.encode()?}))
    })();
    pairing_result(env, result)
}

fn clear_connection(connection: &mut Option<(String, rowd_core::tls::ClientStream)>) {
    *connection = None;
    if let Some(socket) = ACTIVE_SOCKET.get() {
        *socket.lock().unwrap() = None;
    }
}

fn connection() -> &'static Mutex<Option<(String, rowd_core::tls::ClientStream)>> {
    CONNECTION.get_or_init(|| Mutex::new(None))
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_clearPersistentConnection(
    env: JNIEnv,
    class: JObject,
) {
    Java_app_rowd_NativeBridge_networkChanged(env, class);
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_revokeRemotePairing(
    mut env: JNIEnv,
    _class: JObject,
    invitation: JString,
    device_id: JString,
) -> jstring {
    let result = (|| -> Result<serde_json::Value> {
        let invite = Invitation::decode(&String::from(env.get_string(&invitation)?))?;
        let device: String = env.get_string(&device_id)?.into();
        let mut resolver = rowd_core::discovery::EndpointResolver::default();
        let (mut io, _) = resolver.connect(&invite, &device, 0, |fingerprint| {
            let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
            rowd_core::discovery::collect(
                &socket,
                (rowd_core::discovery::GROUP, rowd_core::discovery::PORT).into(),
                fingerprint,
            )
        })?;
        io.sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        match rowd_core::protocol::receive(&mut io)? {
            rowd_core::protocol::Message::Shares { .. } => {}
            _ => anyhow::bail!("expected PC Shares"),
        }
        rowd_core::protocol::send(
            &mut io,
            &rowd_core::protocol::Message::Capabilities {
                device_id: device,
                share_requests: Vec::new(),
                cancel_intents: Vec::new(),
                available_shares: Vec::new(),
                requested_share_ids: Vec::new(),
                audit: false,
                unlink_requested: true,
            },
        )?;
        Ok(serde_json::Value::Null)
    })();
    pairing_result(env, result)
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_networkChanged(_env: JNIEnv, _class: JObject) {
    NETWORK_GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Some(socket) = ACTIVE_SOCKET.get() {
        if let Ok(mut socket) = socket.lock() {
            if let Some(socket) = socket.take() {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
        }
    }
    // If sync owns CONNECTION, socket shutdown or its generation check clears it.
    if let Ok(mut connection) = connection().try_lock() {
        clear_connection(&mut connection);
    }
}

fn base_tokens() -> &'static Mutex<std::collections::HashMap<String, String>> {
    BASE_TOKENS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn check_cancelled() -> Result<()> {
    anyhow::ensure!(
        !CANCELLED.load(Ordering::Relaxed),
        "Sincronização cancelada"
    );
    Ok(())
}

// Android owns SAF streams; Rust owns networking, hashing and the sync protocol.
struct AndroidStore<'a, 'b, 'c> {
    env: &'a mut JNIEnv<'b>,
    access: &'c JObject<'b>,
    ignore: rowd_core::ignore::Ignore,
    focus: Option<Vec<String>>,
    metrics: rowd_core::storage::StoreMetrics,
    token_key: Option<String>,
    share_id: Option<String>,
    scan_socket: Option<TcpStream>,
}
impl AndroidStore<'_, '_, '_> {
    fn scan_result(&mut self, result: &str) -> Result<Manifest> {
        let result: serde_json::Value = serde_json::from_str(result)?;
        if result["deferred"] == true {
            return Err(rowd_core::sync::ScanDeferred.into());
        }
        self.metrics.files_enumerated += result["enumerated"]
            .as_u64()
            .context("scan count missing")?;
        self.metrics.files_hashed += result["hashed"].as_u64().context("hash count missing")?;
        self.metrics.bytes_hashed += result["bytes_hashed"]
            .as_u64()
            .context("hash bytes missing")?;
        self.metrics.full_scans +=
            u64::from(result["full"].as_bool().context("scan mode missing")?);
        Ok(serde_json::from_value(result["files"].clone())?)
    }
    fn request_records(&mut self) -> Result<Vec<serde_json::Value>> {
        let requests: Vec<serde_json::Value> =
            serde_json::from_str(&self.call("pendingShareRequests", &[])?)?;
        for request in &requests {
            anyhow::ensure!(
                matches!(
                    request
                        .get("state")
                        .and_then(|state| state.as_str())
                        .unwrap_or("pending"),
                    "pending" | "cancelled"
                ),
                "unsupported local Share request state"
            );
        }
        Ok(requests)
    }
    fn call(&mut self, name: &str, arguments: &[&str]) -> Result<String> {
        let access = self.access;
        self.env.with_local_frame(16, |env| -> Result<String> {
            let strings: Vec<_> = arguments
                .iter()
                .map(|s| env.new_string(s))
                .collect::<std::result::Result<_, _>>()?;
            let objects: Vec<JObject> = strings.into_iter().map(Into::into).collect();
            let values: Vec<_> = objects.iter().map(JValue::Object).collect();
            let sig = format!(
                "({})Ljava/lang/String;",
                "Ljava/lang/String;".repeat(values.len())
            );
            let result = env.call_method(access, name, sig, &values);
            if env.exception_check()? {
                let exception = env.exception_occurred()?;
                env.exception_clear()?;
                let message = env
                    .call_method(exception, "toString", "()Ljava/lang/String;", &[])?
                    .l()?;
                let text: String = env.get_string(&JString::from(message))?.into();
                anyhow::bail!("{text}");
            }
            let object = result?.l()?;
            Ok(env.get_string(&JString::from(object))?.into())
        })
    }
}
impl Store for AndroidStore<'_, '_, '_> {
    fn scan_is_staged(&self) -> bool {
        true
    }
    fn delta_paths(&mut self) -> Result<Option<std::collections::BTreeSet<String>>> {
        check_cancelled()?;
        Ok(serde_json::from_str(&self.call("deltaPathsJson", &[])?)?)
    }
    fn scan_paths(
        &mut self,
        paths: &std::collections::BTreeSet<String>,
    ) -> Result<Option<Manifest>> {
        check_cancelled()?;
        let response = self.call("scanPathsJson", &[&serde_json::to_string(paths)?])?;
        let result: Option<serde_json::Value> = serde_json::from_str(&response)?;
        let Some(result) = result else {
            return Ok(None);
        };
        self.metrics.files_enumerated += result["enumerated"]
            .as_u64()
            .context("delta scan count missing")?;
        self.metrics.files_hashed += result["hashed"]
            .as_u64()
            .context("delta hash count missing")?;
        self.metrics.bytes_hashed += result["bytes_hashed"]
            .as_u64()
            .context("delta hash bytes missing")?;
        Ok(Some(serde_json::from_value(result["files"].clone())?))
    }
    fn base_token(&self) -> Option<String> {
        self.token_key
            .as_ref()
            .and_then(|key| base_tokens().lock().unwrap().get(key).cloned())
    }
    fn set_base_token(&mut self, token: Option<String>) {
        if let Some(key) = &self.token_key {
            let mut tokens = base_tokens().lock().unwrap();
            if let Some(token) = token {
                tokens.insert(key.clone(), token);
            } else {
                tokens.remove(key);
            }
        }
    }
    fn require_full_scan(&mut self) -> Result<()> {
        self.call("forceFullScan", &[])?;
        Ok(())
    }
    fn metrics(&self) -> rowd_core::storage::StoreMetrics {
        self.metrics
    }
    fn excluded(&self, path: &str) -> bool {
        self.ignore.matches(path, false)
    }
    fn scan(&mut self) -> Result<Manifest> {
        check_cancelled()?;
        self.call("discardScanJson", &[])?;
        let result = self.call("scanJson", &[])?;
        let files = self.scan_result(&result)?;
        self.call("commitScanJson", &[])?;
        Ok(files)
    }
    fn scan_with_control(&mut self, io: &mut (impl Read + Write)) -> Result<Manifest> {
        let socket = self
            .scan_socket
            .as_ref()
            .context("scan socket missing")?
            .try_clone()?;
        let share_id = self.share_id.clone().context("Share not selected")?;
        self.call("startScanJson", &[])?;
        let mut preempted = false;
        let result = (|| -> Result<Manifest> {
            loop {
                check_cancelled()?;
                let status = self.call("pollScanJson", &[])?;
                if !status.is_empty() {
                    if preempted {
                        self.call("discardScanJson", &[])?;
                        return Err(rowd_core::sync::ScanDeferred.into());
                    }
                    return self.scan_result(&status);
                }
                socket.set_read_timeout(Some(Duration::from_millis(50)))?;
                let mut byte = [0];
                match socket.peek(&mut byte) {
                    Ok(0) => anyhow::bail!("peer disconnected during SAF scan"),
                    Ok(_) => {
                        socket.set_read_timeout(Some(Duration::from_secs(90)))?;
                        match rowd_core::protocol::receive_for(io, &share_id)? {
                            rowd_core::protocol::Message::AuditPreempt { shares } => {
                                for id in shares {
                                    rowd_core::model::validate_hash(&id)?;
                                }
                                preempted = true;
                                self.call("deferScanJson", &[])?;
                            }
                            _ => anyhow::bail!("unexpected message during SAF scan"),
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        })();
        let _ = socket.set_read_timeout(Some(Duration::from_secs(90)));
        if result.is_err() {
            let _ = self.call("deferScanJson", &[]);
            // The SAF worker must finish before another Share can be selected.
            while self
                .call("pollScanJson", &[])
                .is_ok_and(|status| status.is_empty())
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.call("discardScanJson", &[]);
        }
        result
    }
    fn commit_scan(&mut self) -> Result<()> {
        self.call("commitScanJson", &[])?;
        Ok(())
    }
    fn discard_scan(&mut self) -> Result<()> {
        self.call("discardScanJson", &[])?;
        Ok(())
    }
    fn snapshot(&mut self, path: &str, entry: &Entry) -> Result<VerifiedStaged> {
        check_cancelled()?;
        let staged: serde_json::Value = serde_json::from_str(&self.call("snapshot", &[path])?)?;
        let owned =
            TempPath::try_from_path(staged["path"].as_str().context("snapshot path missing")?)?;
        let file = std::fs::File::open(&owned)?;
        let hash = staged["hash"].as_str().context("snapshot hash missing")?;
        let size = staged["size"].as_u64().context("snapshot size missing")?;
        ensure!(file.metadata()?.len() == size, "STALE_SOURCE: {path}");
        rowd_core::trace::event(
            "storage",
            "snapshot_verify_start",
            self.share_id.as_deref(),
            Some(path),
            Some(size),
            None,
            None,
        );
        let verifying = Instant::now();
        // Transfer ownership of the private Kotlin temp; no second Rust staging copy.
        let verified =
            VerifiedStaged::from_digest(NamedTempFile::from_parts(file, owned), entry, hash, size)
                .with_context(|| format!("STALE_SOURCE: {path}"))?;
        rowd_core::trace::event(
            "storage",
            "snapshot_verify_end",
            self.share_id.as_deref(),
            Some(path),
            Some(size),
            Some(verifying),
            None,
        );
        check_cancelled()?;
        Ok(verified)
    }
    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: &VerifiedStaged,
    ) -> Result<()> {
        check_cancelled()?;
        anyhow::ensure!(staged.entry() == entry, "staged entry mismatch");
        self.call(
            "install",
            &[
                path,
                expected.unwrap_or(""),
                &entry.hash,
                staged.path().to_str().context("invalid staging path")?,
            ],
        )?;
        Ok(())
    }
    fn acknowledge(&mut self, _path: &str, _entry: &Entry) -> Result<()> {
        check_cancelled()
    }
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_setTrace(
    mut env: JNIEnv,
    _class: JObject,
    path: JString,
) -> jboolean {
    if let Ok(path) = env.get_string(&path) {
        let path: String = path.into();
        let result = if path.is_empty() {
            rowd_core::trace::disable()
        } else {
            rowd_core::trace::enable(std::path::Path::new(&path), "android")
        };
        return result.is_ok().into();
    }
    false.into()
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_flushTrace(_env: JNIEnv, _class: JObject) {
    let _ = rowd_core::trace::flush();
}

impl rowd_core::managed::ManagedClient for AndroidStore<'_, '_, '_> {
    fn configure(&mut self, shares: &[rowd_core::config::ShareDefinition]) -> Result<()> {
        self.call("configureShares", &[&serde_json::to_string(shares)?])?;
        Ok(())
    }
    fn select(&mut self, id: &str) -> Result<()> {
        check_cancelled()?;
        self.call("selectShare", &[id])?;
        self.share_id = Some(id.to_owned());
        self.ignore = rowd_core::ignore::Ignore::parse(&self.call("ignoreText", &[])?);
        self.token_key = Some(format!("{id}|{}", self.call("bindingIdentity", &[])?));
        Ok(())
    }
    fn session_state(&mut self) -> Result<rowd_core::managed::ClientState> {
        let records = self.request_records()?;
        let share_requests = records
            .iter()
            .filter(|request| {
                request
                    .get("state")
                    .and_then(|state| state.as_str())
                    .unwrap_or("pending")
                    == "pending"
            })
            .cloned()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let cancel_intents = records
            .iter()
            .filter(|request| {
                request.get("state").and_then(|state| state.as_str()) == Some("cancelled")
            })
            .map(|request| {
                Ok(request
                    .get("request_id")
                    .and_then(|id| id.as_str())
                    .ok_or_else(|| anyhow::anyhow!("cancel intent has no ID"))?
                    .to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        let json = match &self.focus {
            Some(ids) => self.call("availableSharesForSync", &[&serde_json::to_string(ids)?])?,
            None => self.call("availableShares", &[])?,
        };
        Ok(rowd_core::managed::ClientState {
            focus_shares: self.focus.clone(),
            audit: self.call("auditRound", &[])? == "true",
            available_shares: serde_json::from_str(&json)?,
            share_requests,
            cancel_intents,
            unlink_requested: self.call("unlinkRequested", &[])? == "true",
        })
    }
    fn acknowledge_share_requests(
        &mut self,
        accepted: &[String],
        rejected: &[String],
        cancelled: &[String],
    ) -> Result<()> {
        self.call(
            "acknowledgeShareRequests",
            &[
                &serde_json::to_string(accepted)?,
                &serde_json::to_string(rejected)?,
                &serde_json::to_string(cancelled)?,
            ],
        )?;
        Ok(())
    }
    fn finish_unlink(&mut self) -> Result<()> {
        Ok(())
    }
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_cancel(_env: JNIEnv, _class: JObject) {
    CANCELLED.store(true, Ordering::Relaxed);
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_resetCancellation(_env: JNIEnv, _class: JObject) {
    CANCELLED.store(false, Ordering::Relaxed);
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_sync<'local>(
    mut env: JNIEnv<'local>,
    _class: JObject<'local>,
    invitation: JString<'local>,
    device_id: JString<'local>,
    focus_json: JString<'local>,
    access: JObject<'local>,
) -> jstring {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<String> {
        let text: String = env.get_string(&invitation)?.into();
        let device: String = env.get_string(&device_id)?.into();
        let focus_json: String = env.get_string(&focus_json)?.into();
        let invite = Invitation::decode(&text)?;
        let mut store = AndroidStore {
            env: &mut env,
            access: &access,
            ignore: rowd_core::ignore::Ignore::default(),
            focus: if focus_json.is_empty() {
                None
            } else {
                Some(serde_json::from_str(&focus_json)?)
            },
            metrics: Default::default(),
            token_key: None,
            share_id: None,
            scan_socket: None,
        };
        // One sync worker per process; private app cache is writable on Android.
        std::env::set_var("TMPDIR", store.call("tempDirectory", &[])?);
        let mut connection = connection().lock().unwrap();
        let result = (|| -> Result<String> {
            let fingerprint = invite.fingerprint()?;
            let key = format!(
                "{}|{}|{}|{device}",
                invite.pair_id, fingerprint, invite.secret
            );
            if connection.as_ref().is_some_and(|(current, _)| {
                current != &key
                    || CONNECTION_GENERATION.load(Ordering::SeqCst)
                        != NETWORK_GENERATION.load(Ordering::SeqCst)
            }) {
                clear_connection(&mut connection);
            }
            let mut skipped = std::collections::BTreeSet::new();
            let mut errors = Vec::new();
            loop {
                if connection.is_none() {
                    let generation = NETWORK_GENERATION.load(Ordering::SeqCst);
                    let mut resolver = RESOLVER
                        .get_or_init(|| Mutex::new(Default::default()))
                        .lock()
                        .unwrap();
                    let (io, endpoint) =
                        resolver.connect(&invite, &device, generation, |fingerprint| {
                            let acquired = store.call("acquireMulticast", &[]);
                            let result = match acquired {
                                Ok(_) => (|| {
                                    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
                                    rowd_core::discovery::collect(
                                        &socket,
                                        (rowd_core::discovery::GROUP, rowd_core::discovery::PORT)
                                            .into(),
                                        fingerprint,
                                    )
                                })(),
                                Err(error) => Err(error),
                            };
                            let _ = store.call("releaseMulticast", &[]);
                            result
                        })?;
                    anyhow::ensure!(
                        generation == NETWORK_GENERATION.load(Ordering::SeqCst),
                        "Rede alterada durante a conexão"
                    );
                    store.call("authenticatedAddress", &[&endpoint])?;
                    CONNECTION_GENERATION.store(generation, Ordering::SeqCst);
                    *ACTIVE_SOCKET
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .unwrap() = Some(io.sock.try_clone()?);
                    *connection = Some((key.clone(), io));
                    if generation != NETWORK_GENERATION.load(Ordering::SeqCst) {
                        clear_connection(&mut connection);
                        anyhow::bail!("Rede alterada durante a conexão");
                    }
                }
                store.scan_socket = Some(connection.as_ref().unwrap().1.sock.try_clone()?);
                let mut failed = None;
                let result = rowd_core::managed::client_round_on_excluding(
                    &mut connection.as_mut().unwrap().1,
                    &device,
                    &mut store,
                    &skipped,
                    &mut failed,
                );
                anyhow::ensure!(
                    CONNECTION_GENERATION.load(Ordering::SeqCst)
                        == NETWORK_GENERATION.load(Ordering::SeqCst),
                    "Rede alterada durante a rodada"
                );
                match result {
                    Ok(report) => {
                        anyhow::ensure!(
                            errors.is_empty(),
                            "{} Share(s) failed: {}",
                            errors.len(),
                            errors.join("; ")
                        );
                        break Ok(serde_json::to_string(&report)?);
                    }
                    Err(error) => {
                        store.scan_socket = None;
                        clear_connection(&mut connection);
                        match failed {
                            Some(id) if skipped.insert(id.clone()) => {
                                errors.push(format!("{id}: {error:#}"))
                            }
                            _ => break Err(error),
                        }
                    }
                }
            }
        })();
        if result.is_err() {
            // Every error after acquiring the mutex must discard the persistent stream.
            store.scan_socket = None;
            clear_connection(&mut connection);
        }
        result
    }));
    let output = match result {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => {
            rowd_core::trace::event(
                "sync",
                "error",
                None,
                None,
                None,
                None,
                Some("round_failed"),
            );
            serde_json::json!({"error":format!("{e:#}")}).to_string()
        }
        Err(_) => {
            serde_json::json!({"error":"Falha interna do Rowd; os backups foram preservados."})
                .to_string()
        }
    };
    env.new_string(output)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_pollWake<'local>(
    env: JNIEnv<'local>,
    _class: JObject<'local>,
) -> jstring {
    let mut connection = connection().lock().unwrap();
    let result = if connection.is_some()
        && CONNECTION_GENERATION.load(Ordering::SeqCst) != NETWORK_GENERATION.load(Ordering::SeqCst)
    {
        "!".to_string()
    } else if let Some((_, io)) = connection.as_mut() {
        let _ = io.sock.set_read_timeout(Some(Duration::from_millis(100)));
        let mut first = [0u8; 1];
        let buffered = match io.conn.reader().read(&mut first) {
            Ok(1) => Some(true),
            Ok(0) => Some(false),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Some(false),
            Err(_) => None,
            _ => unreachable!(),
        };
        let ready = match buffered {
            Some(true) => Ok(1),
            Some(false) => io.sock.peek(&mut first),
            None => Err(std::io::Error::other("TLS receive failed")),
        };
        match ready {
            Ok(0) => "!".to_string(),
            Ok(_) => {
                let _ = io.sock.set_read_timeout(Some(Duration::from_secs(90)));
                let incoming = if buffered == Some(true) {
                    rowd_core::protocol::receive_after_first(io, first[0])
                } else {
                    rowd_core::protocol::receive(io)
                };
                match incoming {
                    Ok(rowd_core::protocol::Message::WakeShare { share_id }) => share_id,
                    _ => "!".to_string(),
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                String::new()
            }
            Err(_) => "!".to_string(),
        }
    } else {
        "!".to_string()
    };
    let result = if result == "!"
        || (connection.is_some()
            && CONNECTION_GENERATION.load(Ordering::SeqCst)
                != NETWORK_GENERATION.load(Ordering::SeqCst))
    {
        clear_connection(&mut connection);
        "!".to_string()
    } else {
        result
    };
    env.new_string(result)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_previewInvitation<'local>(
    mut env: JNIEnv<'local>,
    _class: JObject<'local>,
    invitation: JString<'local>,
    address: JString<'local>,
) -> jstring {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<String> {
        let text: String = env.get_string(&invitation)?.into();
        let override_address: String = env.get_string(&address)?.into();
        let mut invite = Invitation::decode(&text)?;
        if !override_address.is_empty() {
            invite.address = override_address;
        }
        invite.validate()?;
        let fingerprint = invite
            .fingerprint()?
            .as_bytes()
            .chunks(2)
            .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(":");
        Ok(serde_json::json!({
            "address": invite.address,
            "fingerprint": fingerprint,
            "invitation": invite.encode()?,
        })
        .to_string())
    }));
    let output = match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => serde_json::json!({"error":format!("{error:#}")}).to_string(),
        Err(_) => serde_json::json!({"error":"Convite inválido."}).to_string(),
    };
    env.new_string(output)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
