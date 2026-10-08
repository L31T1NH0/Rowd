mod digest;
mod idle;
mod transport;
use anyhow::{ensure, Context, Result};
use jni::{
    objects::{JObject, JString, JValue},
    sys::{jboolean, jint, jlong, jstring},
    JNIEnv,
};
use rowd_core::{
    model::{Entry, Invitation, Manifest},
    storage::{Store, VerifiedStaged},
};
use rowd_core::{
    trace::{self, Component as TraceComponent, Level as TraceLevel, TraceError},
    trace_event,
};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use std::time::Instant;
use tempfile::{NamedTempFile, TempPath};
#[cfg(test)]
use transport::WakeReadiness;
use transport::{FailureKind, LocalFilesystemError, PollWake};

static CONNECTION_TRACE_CONTEXT: OnceLock<Mutex<trace::TraceContext>> = OnceLock::new();
static CONNECTION_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();
fn connection_context() -> trace::TraceContext {
    let id = CONNECTION_ID
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone();
    let mut ctx = trace::current_context();
    ctx.ids.extend(
        CONNECTION_TRACE_CONTEXT
            .get_or_init(|| Mutex::new(trace::TraceContext::default()))
            .lock()
            .unwrap()
            .ids
            .clone(),
    );
    if let Some(id) = id {
        ctx.with("connection_id", id)
    } else {
        ctx
    }
}
static IDLE_WAKE: OnceLock<idle::IdleWake> = OnceLock::new();
fn idle_wake() -> &'static idle::IdleWake {
    IDLE_WAKE.get_or_init(|| idle::IdleWake::new().expect("idle socketpair"))
}
fn io_cancelled() -> std::io::Result<()> {
    if CANCELLED.load(Ordering::Relaxed) {
        return Err(std::io::Error::other(transport::CancelledError));
    }
    Ok(())
}
#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_signalIdle(
    _env: JNIEnv,
    _class: JObject,
    reason: jint,
) {
    if let Err(error) = idle_wake().signal(if reason == 2 {
        idle::NETWORK
    } else {
        idle::LOCAL
    }) {
        trace_event!(
            TraceLevel::Error,
            TraceComponent::Connection,
            "IDLE_SIGNAL_FAILED",
            serde_json::json!({"error":error.to_string()})
        );
    }
}
static POLL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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
        let result = match rowd_core::io_retry::poll("pair_offer_recv_from", || {
            socket.recv_from(&mut buf)
        }) {
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

fn clear_connection(connection: &mut Option<(String, rowd_core::tls::ClientStream)>, reason: &str) {
    if connection.is_none() {
        return;
    }
    let _context = connection_context().enter();
    trace_event!(
        TraceLevel::Debug,
        TraceComponent::Connection,
        "CONNECTION_CLEARED",
        serde_json::json!({"reason":reason,"connection_present":connection.is_some(),"network_generation":NETWORK_GENERATION.load(Ordering::SeqCst),"connection_generation":CONNECTION_GENERATION.load(Ordering::SeqCst)})
    );
    *CONNECTION_ID
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = None;
    *CONNECTION_TRACE_CONTEXT
        .get_or_init(|| Mutex::new(Default::default()))
        .lock()
        .unwrap() = Default::default();
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
    let _context = connection_context().enter();
    let previous = NETWORK_GENERATION.fetch_add(1, Ordering::SeqCst);
    let _ = idle_wake().signal(idle::NETWORK);
    trace_event!(
        TraceLevel::Info,
        TraceComponent::Network,
        "NETWORK_GENERATION_CHANGED",
        serde_json::json!({"previous_generation":previous,"network_generation":previous+1,"operation":"native_network_changed","connection_invalidated":ACTIVE_SOCKET.get().is_some_and(|s|s.lock().unwrap().is_some())})
    );
    if let Some(socket) = ACTIVE_SOCKET.get() {
        if let Ok(mut socket) = socket.lock() {
            if let Some(socket) = socket.take() {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
        }
    }
    // If sync owns CONNECTION, socket shutdown or its generation check clears it.
    if let Ok(mut connection) = connection().try_lock() {
        clear_connection(&mut connection, "network_generation_changed");
    }
}

fn base_tokens() -> &'static Mutex<std::collections::HashMap<String, String>> {
    BASE_TOKENS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn check_cancelled() -> Result<()> {
    if CANCELLED.load(Ordering::Relaxed) {
        return Err(transport::CancelledError.into());
    }
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
    scan_errors: Vec<String>,
    completed_shares: std::collections::BTreeSet<String>,
}
impl AndroidStore<'_, '_, '_> {
    fn poll_stream(&mut self, io: &mut (impl Read + Write), operation: &str) -> Result<String> {
        let socket = self
            .scan_socket
            .as_ref()
            .context("scan socket missing")?
            .try_clone()?;
        let share = self.share_id.clone().context("Share not selected")?;
        let mut alive = Instant::now();
        let result = (|| loop {
            check_cancelled()?;
            let status = self.call(operation, &[])?;
            if !status.is_empty() {
                return Ok(status);
            }
            if alive.elapsed() >= Duration::from_secs(5) {
                rowd_core::protocol::send_for(io, &share, rowd_core::protocol::Message::ScanAlive)?;
                alive = Instant::now();
            }
            socket.set_read_timeout(Some(Duration::from_millis(50)))?;
            let mut first = [0];
            match rowd_core::io_retry::poll_with_control("stream_control", io_cancelled, || {
                io.read(&mut first)
            }) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "peer disconnected during scan stream",
                    )
                    .into())
                }
                Ok(_) => {
                    socket.set_read_timeout(Some(Duration::from_secs(90)))?;
                    let message =
                        rowd_core::protocol::receive_for_after_first(io, first[0], &share)?;
                    if let rowd_core::protocol::Message::AuditPreempt { shares } = message {
                        for id in shares {
                            rowd_core::model::validate_hash(&id)?;
                        }
                        self.call("deferScanJson", &[])?;
                        return Err(rowd_core::sync::ScanDeferred.into());
                    }
                    anyhow::bail!("unexpected scan stream control");
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        })();
        let _ = socket.set_read_timeout(Some(Duration::from_secs(90)));
        if result.is_err() {
            let _ = self.call("deferScanJson", &[]);
            let _ = self.call("finishScanJson", &[]);
            let _ = self.call("discardScanJson", &[]);
        }
        transport::deferred_scan_result(result, &mut self.scan_errors)
    }
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
    fn focused_scan_result(
        &mut self,
        result: Option<serde_json::Value>,
    ) -> Result<Option<Manifest>> {
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
        let started = Instant::now();
        trace_event!(
            TraceLevel::Trace,
            TraceComponent::SAF,
            "SAF_CALL_START",
            serde_json::json!({"operation":name,"argument_count":arguments.len()})
        );
        let result = self.env.with_local_frame(16, |env| -> Result<String> {
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
                    .call_method(&exception, "toString", "()Ljava/lang/String;", &[])?
                    .l()?;
                let text: String = env.get_string(&JString::from(message))?.into();
                if text.contains("AUDIT_DEFERRED") {
                    return Err(rowd_core::sync::ScanDeferred.into());
                }
                if trace::enabled() {
                    let described = env
                        .call_static_method(
                            "app/rowd/PerformanceTrace",
                            "describeError",
                            "(Ljava/lang/Throwable;)Ljava/lang/String;",
                            &[JValue::Object(exception.as_ref())],
                        )
                        .and_then(|v| v.l());
                    if let Ok(object) = described {
                        if let Ok(description) = env.get_string(&JString::from(object)) {
                            let description: String = description.into();
                            if let Ok(error) =
                                serde_json::from_str::<serde_json::Value>(&description)
                            {
                                trace_event!(
                                    TraceLevel::Error,
                                    TraceComponent::SAF,
                                    "SAF_CALL_FAILED",
                                    serde_json::json!({"operation":name,"error":error})
                                );
                            }
                        }
                    } else {
                        let _ = env.exception_clear();
                    }
                }
                return Err(LocalFilesystemError(format!("SAF {name}: {text}")).into());
            }
            let object = result?.l()?;
            Ok(env.get_string(&JString::from(object))?.into())
        });
        match &result {
            Ok(_) => trace_event!(
                TraceLevel::Trace,
                TraceComponent::SAF,
                "SAF_CALL_END",
                serde_json::json!({"operation":name,"duration_us":started.elapsed().as_micros()})
            ),
            Err(error) if error.is::<rowd_core::sync::ScanDeferred>() => trace_event!(
                TraceLevel::Debug,
                TraceComponent::Scanner,
                "STREAM_PREEMPT_APPLIED",
                serde_json::json!({"operation":name,"frame_boundary":true})
            ),
            Err(error) => trace_event!(
                TraceLevel::Error,
                TraceComponent::SAF,
                "SAF_CALL_FAILED",
                serde_json::json!({"operation":name,"error":TraceError::new("filesystem",name,error)})
            ),
        }
        result
    }
}
impl Store for AndroidStore<'_, '_, '_> {
    fn scan_stream_metrics(&mut self) -> Result<rowd_core::storage::ScanStreamMetrics> {
        Ok(serde_json::from_str(
            &self.call("scanStreamMetricsJson", &[])?,
        )?)
    }
    fn scan_binding(&mut self) -> Result<String> {
        self.call("bindingIdentity", &[])
    }
    fn stream_namespace(
        &mut self,
        io: &mut (impl Read + Write),
    ) -> Result<Option<rowd_core::model::Namespace>> {
        self.call("startNamespaceJson", &[])?;
        let status = self.poll_stream(io, "pollScanJson")?;
        let result: serde_json::Value = serde_json::from_str(&status)?;
        if result["deferred"] == true {
            return Err(rowd_core::sync::ScanDeferred.into());
        }
        let namespace: rowd_core::model::Namespace = serde_json::from_value(result)?;
        self.metrics.files_enumerated += namespace.values().filter(|e| !e.directory).count() as u64;
        self.metrics.full_scans += 1;
        Ok(Some(namespace))
    }
    fn start_hash_stream(
        &mut self,
        stage_paths: &std::collections::BTreeSet<String>,
    ) -> Result<()> {
        self.call(
            "startHashStreamJson",
            &[&serde_json::to_string(stage_paths)?],
        )?;
        Ok(())
    }
    fn next_hash_chunk(&mut self, io: &mut (impl Read + Write)) -> Result<Option<Manifest>> {
        let result = self.poll_stream(io, "pollHashStreamJson")?;
        if result == "end" {
            let metrics: serde_json::Value =
                serde_json::from_str(&self.call("finishHashStreamJson", &[])?)?;
            self.metrics.files_hashed += metrics["hashed"]
                .as_u64()
                .context("stream hashes missing")?;
            self.metrics.bytes_hashed += metrics["bytes_hashed"]
                .as_u64()
                .context("stream hash bytes missing")?;
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&result)?))
    }
    fn release_hash_staging(&mut self, paths: &std::collections::BTreeSet<String>) -> Result<()> {
        self.call("releaseHashStagingJson", &[&serde_json::to_string(paths)?])?;
        Ok(())
    }
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
        self.focused_scan_result(serde_json::from_str(&response)?)
    }
    fn delta_scan_with_control(
        &mut self,
        io: &mut (impl Read + Write),
        paths: &std::collections::BTreeSet<String>,
    ) -> Result<Option<(std::collections::BTreeSet<String>, Manifest)>> {
        check_cancelled()?;
        self.call("startDeltaScanJson", &[&serde_json::to_string(paths)?])?;
        let response = self.poll_stream(io, "pollScanJson")?;
        let result: Option<serde_json::Value> = serde_json::from_str(&response)?;
        let Some(result) = result else {
            return Ok(None);
        };
        let paths = serde_json::from_value(result["paths"].clone())?;
        Ok(self
            .focused_scan_result(Some(result))?
            .map(|files| (paths, files)))
    }
    fn base_token(&self) -> Option<String> {
        self.token_key
            .as_ref()
            .and_then(|key| base_tokens().lock().unwrap().get(key).cloned())
    }
    fn set_base_token(&mut self, token: Option<String>) {
        if token.is_some() {
            if let Some(id) = &self.share_id {
                self.completed_shares.insert(id.clone());
            }
        }
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
        let mut preempted = false;
        let mut last_alive = std::time::Instant::now();
        let result = (|| -> Result<Manifest> {
            self.call("startScanJson", &[])?;
            loop {
                check_cancelled()?;
                let status = self.call("pollScanJson", &[])?;
                if !status.is_empty() {
                    if preempted {
                        return Err(rowd_core::sync::ScanDeferred.into());
                    }
                    return self.scan_result(&status);
                }
                if last_alive.elapsed() >= Duration::from_secs(5) && !preempted {
                    rowd_core::protocol::send_for(
                        io,
                        &share_id,
                        rowd_core::protocol::Message::ScanAlive,
                    )?;
                    last_alive = std::time::Instant::now();
                }
                socket.set_read_timeout(Some(Duration::from_millis(50)))?;
                let mut byte = [0];
                match rowd_core::io_retry::poll_with_control("scan_peek", io_cancelled, || {
                    socket.peek(&mut byte)
                }) {
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "peer disconnected during SAF scan",
                        )
                        .into())
                    }
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
            let _ = self.call("finishScanJson", &[]);
            let _ = self.call("discardScanJson", &[]);
        }
        transport::deferred_scan_result(result, &mut self.scan_errors)
    }
    fn commit_scan(&mut self) -> Result<()> {
        self.call("commitScanJson", &[])?;
        Ok(())
    }
    fn discard_scan(&mut self) -> Result<()> {
        self.call("discardScanJson", &[])?;
        Ok(())
    }
    fn legacy_hash_with_control(
        &mut self,
        io: &mut (impl Read + Write),
        path: &str,
        entry: &Entry,
    ) -> Result<String> {
        self.call(
            "startLegacyHashJson",
            &[path, &entry.hash, &entry.size.to_string()],
        )?;
        self.poll_stream(io, "pollScanJson")
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
        rowd_core::trace_legacy_event!(
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
        rowd_core::trace_legacy_event!(
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
    session_id: JString,
) -> jboolean {
    let id: String = match env.get_string(&session_id) {
        Ok(s) => s.into(),
        Err(_) => return false.into(),
    };
    if let Ok(path) = env.get_string(&path) {
        let path: String = path.into();
        let result = if path.is_empty() {
            rowd_core::trace::disable()
        } else {
            rowd_core::trace::start(std::path::Path::new(&path), "android", Some(&id))
        };
        return result.is_ok().into();
    }
    false.into()
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_traceEvent(
    mut env: JNIEnv,
    _class: JObject,
    event: JString,
) -> jboolean {
    let result = (|| -> Result<()> {
        let text: String = env.get_string(&event).map_err(|error| {
            let error = anyhow::Error::new(error);
            eprintln!("INGEST_ANDROID_FAILED: {error:#}");
            rowd_core::trace_event!(trace::Level::Warn, trace::Component::Trace, "INGEST_ANDROID_FAILED",
                serde_json::json!({"error":trace::TraceError::new("trace", "read_kotlin_event", &error)}));
            error
        })?.into();
        rowd_core::trace::ingest_android_json(&text)?;
        Ok(())
    })();
    (result.is_ok() && rowd_core::trace::enabled()).into()
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_stopTrace(
    mut env: JNIEnv,
    _class: JObject,
    termination: JString,
) -> jboolean {
    let result = (|| -> Result<()> {
        let termination: String = env.get_string(&termination)?.into();
        trace::stop(&termination)
    })();
    result.is_ok().into()
}
#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_traceRuntimeState(
    env: JNIEnv,
    _class: JObject,
) -> jstring {
    let present = ACTIVE_SOCKET
        .get()
        .and_then(|s| s.try_lock().ok())
        .map(|s| s.is_some());
    let state=serde_json::json!({"connection_present":present,"network_generation":NETWORK_GENERATION.load(Ordering::SeqCst),"connection_generation":CONNECTION_GENERATION.load(Ordering::SeqCst),"poll_count":POLL_COUNT.load(Ordering::Relaxed),"idle":idle_wake().metrics(),"cancellation_state":CANCELLED.load(Ordering::Relaxed),"trace_active":trace::enabled(),"trace_error":trace::status()["error"]}).to_string();
    env.new_string(state)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
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
    fn finish_round(&mut self, report: &rowd_core::sync::Report) -> Result<()> {
        let result: Result<()> = (|| {
            check_cancelled()?;
            if !self.scan_errors.is_empty() {
                return Err(LocalFilesystemError(self.scan_errors.join("; ")).into());
            }
            Ok(())
        })();
        result.context(transport::PendingWakes(report.pending_wakes.clone()))
    }
    fn finish_unlink(&mut self) -> Result<()> {
        Ok(())
    }
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_cancel(_env: JNIEnv, _class: JObject) {
    CANCELLED.store(true, Ordering::Relaxed);
    let _ = idle_wake().signal(idle::CANCEL);
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_resetCancellation(_env: JNIEnv, _class: JObject) {
    CANCELLED.store(false, Ordering::Relaxed);
    idle_wake().reset_cancel();
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
            scan_errors: Vec::new(),
            completed_shares: Default::default(),
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
                clear_connection(&mut connection, "identity_or_network_generation_changed");
            }
            let mut skipped = std::collections::BTreeSet::new();
            let mut errors = Vec::new();
            loop {
                let reused = connection.is_some();
                if connection.is_none() {
                    let generation = NETWORK_GENERATION.load(Ordering::SeqCst);
                    let mut resolver = RESOLVER
                        .get_or_init(|| Mutex::new(Default::default()))
                        .lock()
                        .unwrap();
                    let (io, endpoint) = resolver
                        .connect(&invite, &device, generation, |fingerprint| {
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
                        })
                        .context(transport::ConnectionAttemptFailure)?;
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
                    let resolved_context = resolver.trace_context();
                    *CONNECTION_TRACE_CONTEXT
                        .get_or_init(|| Mutex::new(Default::default()))
                        .lock()
                        .unwrap() = resolved_context.clone();
                    let id = resolved_context
                        .ids
                        .get("connection_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| trace::new_id("connection"));
                    *CONNECTION_ID
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .unwrap() = Some(id.clone());
                    let _context = trace::current_context().with("connection_id", id).enter();
                    trace_event!(
                        TraceLevel::Info,
                        TraceComponent::Connection,
                        "PERSISTENT_CONNECTION_INSTALLED",
                        serde_json::json!({"endpoint":endpoint,"network_generation":generation,"connection_generation":generation})
                    );
                    *connection = Some((key.clone(), io));
                    if generation != NETWORK_GENERATION.load(Ordering::SeqCst) {
                        clear_connection(&mut connection, "network_generation_changed");
                        anyhow::bail!("Rede alterada durante a conexão");
                    }
                }
                let _connection_context = connection_context().enter();
                if reused {
                    trace_event!(
                        TraceLevel::Debug,
                        TraceComponent::Connection,
                        "CONNECTION_REUSED",
                        serde_json::json!({"reason":"persistent_stream_present","network_generation":NETWORK_GENERATION.load(Ordering::SeqCst),"connection_generation":CONNECTION_GENERATION.load(Ordering::SeqCst)})
                    );
                }
                connection
                    .as_ref()
                    .unwrap()
                    .1
                    .sock
                    .set_read_timeout(Some(Duration::from_secs(90)))?;
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
                        let mut result = serde_json::to_value(&report)?;
                        result["completed_shares"] = serde_json::to_value(&store.completed_shares)?;
                        break Ok(result.to_string());
                    }
                    Err(error) => {
                        store.scan_socket = None;
                        let kind =
                            transport::classify(&error, false, CANCELLED.load(Ordering::Relaxed));
                        if transport::requires_reconnect(&error, kind) {
                            clear_connection(&mut connection, kind.label());
                        }
                        if matches!(
                            kind,
                            FailureKind::TransportInvalid | FailureKind::NetworkGenerationChanged
                        ) {
                            break Err(error); // Retry the same dirty Shares after reconnect; never skip them for TCP loss.
                        }
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
            store.scan_socket = None;
            let kind = transport::classify(
                result.as_ref().unwrap_err(),
                CONNECTION_GENERATION.load(Ordering::SeqCst)
                    != NETWORK_GENERATION.load(Ordering::SeqCst),
                CANCELLED.load(Ordering::Relaxed),
            );
            if transport::requires_reconnect(result.as_ref().unwrap_err(), kind) {
                clear_connection(&mut connection, kind.label());
            }
        }
        result
    }));
    let output = match result {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => {
            let kind = transport::classify(
                &e,
                CONNECTION_GENERATION.load(Ordering::SeqCst)
                    != NETWORK_GENERATION.load(Ordering::SeqCst),
                CANCELLED.load(Ordering::Relaxed),
            );
            trace_event!(
                TraceLevel::Warn,
                TraceComponent::Round,
                "SYNC_OPERATION_FAILED",
                serde_json::json!({"error_kind":kind.label(),"error":TraceError::new("scheduler","native_sync",&e)})
            );
            serde_json::json!({"error":format!("{e:#}"),"error_kind":kind.label(),"pending_wakes":e.downcast_ref::<transport::PendingWakes>().map(|pending| &pending.0)}).to_string()
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

struct IdleFrameSocket<'a> {
    socket: &'a mut TcpStream,
    pending: u8,
    deadline: Instant,
}
impl Read for IdleFrameSocket<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match idle_wake().wait(self.socket, self.deadline, io_cancelled)? {
                idle::Ready::Socket => {
                    return rowd_core::io_retry::interrupted_with_control(
                        "idle_frame_read",
                        io_cancelled,
                        || self.socket.read(buf),
                    )
                }
                idle::Ready::Deadline => return Err(std::io::ErrorKind::TimedOut.into()),
                idle::Ready::Control(reason) => {
                    if reason & idle::CANCEL != 0 {
                        return Err(std::io::Error::other(transport::CancelledError));
                    }
                    if reason & idle::NETWORK != 0 {
                        return Err(std::io::ErrorKind::NotConnected.into());
                    }
                    self.pending |= reason;
                }
            }
        }
    }
}
impl Write for IdleFrameSocket<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.socket.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}
impl Drop for IdleFrameSocket<'_> {
    fn drop(&mut self) {
        if self.pending != 0 {
            let _ = idle_wake().signal(self.pending);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_app_rowd_NativeBridge_pollWake<'local>(
    env: JNIEnv<'local>,
    _class: JObject<'local>,
    audit_in_ms: jlong,
) -> jstring {
    let mut connection = connection().lock().unwrap();
    let result = poll_connection(
        &mut connection,
        Duration::from_millis(audit_in_ms.max(0) as u64),
    );
    env.new_string(result.json())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

enum IdleInput {
    First(u8),
    Event(PollWake),
    Eof,
}
fn wait_application_byte(
    io: &mut rowd_core::tls::ClientStream,
    audit_in: Duration,
) -> Result<IdleInput> {
    let deadline = Instant::now() + audit_in;
    loop {
        let mut byte = [0];
        match rowd_core::io_retry::poll_with_control("idle_tls_reader", io_cancelled, || {
            io.conn.reader().read(&mut byte)
        }) {
            Ok(0) => return Ok(IdleInput::Eof),
            Ok(_) => return Ok(IdleInput::First(byte[0])),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        match idle_wake().wait(&io.sock, deadline, io_cancelled)? {
            idle::Ready::Deadline => return Ok(IdleInput::Event(PollWake::AuditDue)),
            idle::Ready::Control(reason) => {
                return Ok(IdleInput::Event(if reason & idle::CANCEL != 0 {
                    PollWake::Cancelled
                } else if reason & idle::NETWORK != 0 {
                    PollWake::Network
                } else {
                    PollWake::Local
                }))
            }
            idle::Ready::Socket => {
                // rustls read_tls performs one read and retains partial TLS records. No
                // application frame has started yet, so local control may safely return.
                if rowd_core::io_retry::interrupted_with_control(
                    "idle_read_tls",
                    io_cancelled,
                    || io.conn.read_tls(&mut io.sock),
                )? == 0
                {
                    return Ok(IdleInput::Eof);
                }
                io.conn.process_new_packets()?;
                while io.conn.wants_write() {
                    io_cancelled()?;
                    // rustls keeps the unwritten suffix. Never replay a write on error.
                    ensure!(
                        io.conn.write_tls(&mut io.sock)? > 0,
                        "TLS write made no progress"
                    );
                }
            }
        }
    }
}

fn poll_connection(
    connection: &mut Option<(String, rowd_core::tls::ClientStream)>,
    audit_in: Duration,
) -> PollWake {
    if CANCELLED.load(Ordering::Relaxed) {
        return PollWake::Cancelled;
    }
    let _context = connection_context().enter();
    POLL_COUNT.fetch_add(1, Ordering::Relaxed);
    let poll_started = Instant::now();
    let mut frame_started = false;
    let result = if connection.is_some()
        && CONNECTION_GENERATION.load(Ordering::SeqCst) != NETWORK_GENERATION.load(Ordering::SeqCst)
    {
        PollWake::TransportInvalid
    } else if let Some((_, io)) = connection.as_mut() {
        match wait_application_byte(io, audit_in) {
            Ok(IdleInput::Event(event)) => event,
            Ok(IdleInput::Eof) => {
                trace_event!(
                    TraceLevel::Warn,
                    TraceComponent::Connection,
                    "POLL_WAKE_EOF",
                    serde_json::json!({"reason":"socket_eof"})
                );
                PollWake::TransportInvalid
            }
            Ok(IdleInput::First(first)) => {
                frame_started = true;
                let _ = io.sock.set_read_timeout(Some(Duration::from_secs(90)));
                // A started frame keeps its 90s liveness deadline. Control can cancel its
                // individual socket reads; local hints are retained until the frame finishes.
                let mut socket = IdleFrameSocket {
                    socket: &mut io.sock,
                    pending: 0,
                    deadline: Instant::now() + Duration::from_secs(90),
                };
                let mut stream = rustls::Stream::new(&mut io.conn, &mut socket);
                let incoming = rowd_core::protocol::receive_after_first(&mut stream, first);
                match incoming {
                    Ok(rowd_core::protocol::Message::WakeShare { share_id }) => {
                        idle_wake().remote.fetch_add(1, Ordering::Relaxed);
                        PollWake::Share(share_id)
                    }
                    Err(_) if CANCELLED.load(Ordering::Relaxed) => {
                        trace_event!(
                            TraceLevel::Debug,
                            TraceComponent::Connection,
                            "IDLE_FRAME_CANCELLED",
                            serde_json::json!({"stream_reusable":false})
                        );
                        PollWake::Cancelled
                    }
                    Err(error) => {
                        trace_event!(
                            TraceLevel::Error,
                            TraceComponent::Connection,
                            "POLL_WAKE_ERROR",
                            serde_json::json!({"error":TraceError::new("protocol","poll_wake_receive",&error)})
                        );
                        PollWake::TransportInvalid
                    }
                    Ok(message) => {
                        trace_event!(
                            TraceLevel::Warn,
                            TraceComponent::Connection,
                            "POLL_WAKE_RESULT",
                            serde_json::json!({"reason":"unexpected_message","message_type":message.trace_type()})
                        );
                        PollWake::TransportInvalid
                    }
                }
            }
            Err(_) if CANCELLED.load(Ordering::Relaxed) => PollWake::Cancelled,
            Err(error) => {
                trace_event!(
                    TraceLevel::Error,
                    TraceComponent::Connection,
                    "POLL_WAKE_ERROR",
                    serde_json::json!({"error":TraceError::new("connection","idle_wait_application_byte",&error)})
                );
                PollWake::TransportInvalid
            }
        }
    } else {
        PollWake::TransportInvalid
    };
    if result == PollWake::Cancelled && frame_started {
        clear_connection(connection, "cancelled_partial_frame");
    }
    let result = if result == PollWake::TransportInvalid
        || (connection.is_some()
            && CONNECTION_GENERATION.load(Ordering::SeqCst)
                != NETWORK_GENERATION.load(Ordering::SeqCst))
    {
        clear_connection(connection, "transport_invalid");
        PollWake::TransportInvalid
    } else {
        result
    };
    trace_event!(
        TraceLevel::Debug,
        TraceComponent::Connection,
        "IDLE_WAIT_END",
        serde_json::json!({"duration_ms":poll_started.elapsed().as_millis(),"result":serde_json::from_str::<serde_json::Value>(&result.json()).unwrap(),"counters":idle_wake().metrics()})
    );
    if result != PollWake::None {
        trace_event!(
            TraceLevel::Info,
            TraceComponent::Connection,
            "POLL_WAKE_RESULT",
            serde_json::from_str::<serde_json::Value>(&result.json()).unwrap()
        );
    }
    result
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

#[cfg(test)]
mod regression_tests {
    use super::*;
    #[test]
    fn interrupted_scan_and_poll_keep_stream_but_real_eof_clears_it() {
        let directory = tempfile::tempdir().unwrap();
        trace::start(directory.path(), "android", None).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cert: String = include_bytes!("../tests/fixtures/public-test-cert.der")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        // TLS remains lazy here: test the underlying wait/EOF without any wire writes.
        let stream = rowd_core::tls::connect_pinned(
            &cert,
            &listener.local_addr().unwrap().to_string(),
            Duration::from_secs(1),
        )
        .unwrap();
        let (peer, _) = listener.accept().unwrap();
        let mut connection = Some(("regression".into(), stream));
        let idle_started = Instant::now();
        assert_eq!(
            poll_connection(&mut connection, Duration::from_millis(100)),
            PollWake::AuditDue
        );
        assert!(idle_started.elapsed() >= Duration::from_millis(80));
        let mut attempts = 0;
        assert_eq!(
            rowd_core::io_retry::poll("scan_peek", || {
                attempts += 1;
                if attempts == 1 {
                    Err(std::io::ErrorKind::Interrupted.into())
                } else {
                    Ok(1)
                }
            })
            .unwrap(),
            1
        );
        let mut attempts = 0;
        assert_eq!(
            transport::wait_for_wake(
                |_| Err(std::io::ErrorKind::WouldBlock.into()),
                |_| {
                    attempts += 1;
                    Err(if attempts == 1 {
                        std::io::ErrorKind::Interrupted
                    } else {
                        std::io::ErrorKind::TimedOut
                    }
                    .into())
                }
            )
            .unwrap(),
            WakeReadiness::Idle
        );
        assert!(connection.is_some());
        trace::flush().unwrap();
        let path = directory.path().join("Latest-trace/trace-0001.jsonl");
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(!before.contains("CONNECTION_CLEARED"));
        assert!(!before.contains("ROUND_FAILED"));
        assert!(!before.contains("transport_invalid"));
        peer.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(
            poll_connection(&mut connection, Duration::from_millis(100)),
            PollWake::TransportInvalid
        );
        assert!(connection.is_none());
        assert_eq!(
            poll_connection(&mut connection, Duration::from_millis(100)),
            PollWake::TransportInvalid
        );
        trace::stop("regression_test").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let records: Vec<serde_json::Value> = after
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records
                .iter()
                .filter(|v| v["event"] == "CONNECTION_CLEARED")
                .count(),
            1
        );
        assert!(!records.iter().any(|v| v["event"] == "POLL_WAKE_START"
            || (v["event"] == "POLL_WAKE_RESULT" && v["fields"]["kind"] == "none")));
        assert!(records.iter().any(
            |v| v["event"] == "POLL_WAKE_RESULT" && v["fields"]["kind"] == "transport_invalid"
        ));
        tls_wakes_without_polling();
    }
    // Run inside the regression test so native globals/trace state have one owner.
    fn tls_wakes_without_polling() {
        use rowd_core::protocol::{self, Message};
        use std::sync::mpsc;
        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }
        let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()]).unwrap();
        let cert_der = hex(cert.cert.der());
        let server =
            rowd_core::tls::server_config(&cert_der, &hex(&cert.key_pair.serialize_der())).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let (commands, receive_commands) = mpsc::channel();
        let (sent, receive_sent) = mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut io = rowd_core::tls::accept(socket, server).unwrap();
            assert!(matches!(
                protocol::receive(&mut io).unwrap(),
                Message::SessionDone
            ));
            protocol::send(&mut io, &Message::SessionDone).unwrap();
            while let Ok(command) = receive_commands.recv() {
                match command {
                    "remote" => protocol::send(
                        &mut io,
                        &Message::WakeShare {
                            share_id: "share-X".into(),
                        },
                    )
                    .unwrap(),
                    "tls_control" => {
                        io.conn.refresh_traffic_keys().unwrap();
                        io.flush().unwrap();
                    }
                    "partial" => {
                        io.write_all(&[0, 0]).unwrap();
                        io.flush().unwrap();
                    }
                    _ => break,
                }
                sent.send(()).unwrap();
            }
        });
        let mut stream =
            rowd_core::tls::connect_pinned(&cert_der, &endpoint, Duration::from_secs(1)).unwrap();
        protocol::send(&mut stream, &Message::SessionDone).unwrap();
        assert!(matches!(
            protocol::receive(&mut stream).unwrap(),
            Message::SessionDone
        ));
        let mut connection = Some(("tls-wake-test".into(), stream));
        let baseline_timeouts = idle_wake().timeouts.load(Ordering::Relaxed);
        let baseline_remote = idle_wake().remote.load(Ordering::Relaxed);
        for (command, reason, expected) in [
            ("tls_control", idle::LOCAL, PollWake::Local),
            ("remote", 0, PollWake::Share("share-X".into())),
            ("", idle::NETWORK, PollWake::Network),
            ("", idle::CANCEL, PollWake::Cancelled),
        ] {
            let waiter = std::thread::spawn(move || {
                let result = poll_connection(&mut connection, Duration::from_secs(30));
                (result, connection)
            });
            std::thread::sleep(Duration::from_millis(80));
            if !command.is_empty() {
                commands.send(command).unwrap();
                receive_sent.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            if command == "tls_control" {
                std::thread::sleep(Duration::from_millis(80));
            }
            let start = Instant::now();
            if reason != 0 {
                idle_wake().signal(reason).unwrap();
            }
            let (result, remaining) = waiter.join().unwrap();
            connection = remaining;
            assert_eq!(result, expected);
            assert!(connection.is_some());
            assert!(start.elapsed() < Duration::from_millis(500));
        }
        assert_eq!(
            idle_wake().remote.load(Ordering::Relaxed) - baseline_remote,
            1
        );
        assert_eq!(
            idle_wake().timeouts.load(Ordering::Relaxed),
            baseline_timeouts
        );
        // A partial protocol frame cannot be replayed after cancellation.
        commands.send("partial").unwrap();
        receive_sent.recv_timeout(Duration::from_secs(2)).unwrap();
        let waiter = std::thread::spawn(move || {
            let result = poll_connection(&mut connection, Duration::from_secs(30));
            (result, connection)
        });
        std::thread::sleep(Duration::from_millis(80));
        CANCELLED.store(true, Ordering::Relaxed);
        idle_wake().signal(idle::CANCEL).unwrap();
        let (result, connection) = waiter.join().unwrap();
        assert_eq!(result, PollWake::Cancelled);
        assert!(connection.is_none());
        CANCELLED.store(false, Ordering::Relaxed);
        idle_wake().reset_cancel();
        commands.send("stop").unwrap();
        peer.join().unwrap();
    }
}
