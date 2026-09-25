use anyhow::{Context, Result};
use jni::{
    objects::{JObject, JString, JValue},
    sys::jstring,
    JNIEnv,
};
use rowd_core::{
    model::{Entry, Invitation, Manifest},
    storage::{Store, VerifiedStaged},
};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tempfile::{NamedTempFile, TempPath};

static CANCELLED: AtomicBool = AtomicBool::new(false);
static BASE_TOKENS: OnceLock<Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();
static CONNECTION: OnceLock<Mutex<Option<(String, rowd_core::tls::ClientStream)>>> =
    OnceLock::new();

fn connection() -> &'static Mutex<Option<(String, rowd_core::tls::ClientStream)>> {
    CONNECTION.get_or_init(|| Mutex::new(None))
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
}
impl AndroidStore<'_, '_, '_> {
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
        let result: serde_json::Value = serde_json::from_str(&self.call("scanJson", &[])?)?;
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
    fn snapshot(&mut self, path: &str, entry: &Entry) -> Result<VerifiedStaged> {
        check_cancelled()?;
        let staged: serde_json::Value = serde_json::from_str(&self.call("snapshot", &[path])?)?;
        let owned =
            TempPath::try_from_path(staged["path"].as_str().context("snapshot path missing")?)?;
        let file = std::fs::File::open(&owned)?;
        let hash = staged["hash"].as_str().context("snapshot hash missing")?;
        let size = staged["size"].as_u64().context("snapshot size missing")?;
        let verified =
            VerifiedStaged::from_digest(NamedTempFile::from_parts(file, owned), entry, hash, size)
                .with_context(|| format!("STALE_SOURCE: {path}"))?;
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

impl rowd_core::managed::ManagedClient for AndroidStore<'_, '_, '_> {
    fn configure(&mut self, shares: &[rowd_core::config::ShareDefinition]) -> Result<()> {
        self.call("configureShares", &[&serde_json::to_string(shares)?])?;
        Ok(())
    }
    fn select(&mut self, id: &str) -> Result<()> {
        check_cancelled()?;
        self.call("selectShare", &[id])?;
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
    fn prepare_unlink(&mut self) -> Result<()> {
        self.call("prepareUnlink", &[])?;
        Ok(())
    }
    fn finish_unlink(&mut self) -> Result<()> {
        self.call("confirmUnlinked", &[])?;
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
        };
        // One sync worker per process; private app cache is writable on Android.
        std::env::set_var("TMPDIR", store.call("tempDirectory", &[])?);
        let mut connection = connection().lock().unwrap();
        let key = format!("{text}|{device}");
        if connection
            .as_ref()
            .is_some_and(|(current, _)| current != &key)
        {
            *connection = None;
        }
        let mut skipped = std::collections::BTreeSet::new();
        let mut errors = Vec::new();
        loop {
            if connection.is_none() {
                let mut io = rowd_core::tls::connect(&invite)?;
                rowd_core::protocol::client_auth(
                    &mut io,
                    &invite.pair_id,
                    &invite.secret,
                    &device,
                )?;
                *connection = Some((key.clone(), io));
            }
            let mut failed = None;
            let result = rowd_core::managed::client_round_on_excluding(
                &mut connection.as_mut().unwrap().1,
                &device,
                &mut store,
                &skipped,
                &mut failed,
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
                    *connection = None;
                    match failed {
                        Some(id) if skipped.insert(id.clone()) => {
                            errors.push(format!("{id}: {error:#}"))
                        }
                        _ => break Err(error),
                    }
                }
            }
        }
    }));
    let output = match result {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => serde_json::json!({"error":format!("{e:#}")}).to_string(),
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
    let result = if let Some((_, io)) = connection.as_mut() {
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
    if result == "!" {
        *connection = None;
    }
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
