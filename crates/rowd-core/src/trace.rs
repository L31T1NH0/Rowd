//! Opt-in diagnostic trace. JSONL is authoritative; no payloads or credentials.
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub const CHUNK_BYTES: u64 = 64 * 1024 * 1024;
static ENABLED: AtomicBool = AtomicBool::new(false);
static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();
static LAST_STATUS: OnceLock<Mutex<Value>> = OnceLock::new();
static FAILURE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SECRETS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
pub fn register_secret(secret: &str) {
    if !secret.is_empty() {
        SECRETS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap()
            .insert(secret.into());
    }
}
static FLUSHER: OnceLock<Result<(), String>> = OnceLock::new();
static IDS: AtomicU64 = AtomicU64::new(1);
static PROCESS_STARTED: OnceLock<u64> = OnceLock::new();
pub fn process_start() {
    PROCESS_STARTED.get_or_init(wall_ms);
    let _ = process_id();
}
static PROCESS: OnceLock<String> = OnceLock::new();
fn session() -> &'static Mutex<Option<Session>> {
    SESSION.get_or_init(|| Mutex::new(None))
}
pub fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        wall_ms(),
        IDS.fetch_add(1, Ordering::Relaxed)
    )
}
pub fn process_id() -> &'static str {
    PROCESS.get_or_init(|| new_id("process"))
}
// UTC ISO 8601 without a platform/timezone dependency.
pub fn wall_time(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        ms / 3_600_000 % 24,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Component {
    CLI,
    #[serde(rename = "Terminal-output")]
    TerminalOutput,
    Daemon,
    #[serde(rename = "Daemon-IPC")]
    DaemonIpc,
    #[serde(rename = "Android-Service")]
    AndroidService,
    Watcher,
    Scanner,
    Scheduler,
    Round,
    Discovery,
    Network,
    Connection,
    Protocol,
    Heartbeat,
    Transfer,
    Filesystem,
    SAF,
    StateStore,
    Pairing,
    Recovery,
    Trace,
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct TraceContext {
    #[serde(flatten)]
    pub ids: serde_json::Map<String, Value>,
}
impl TraceContext {
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.ids.insert(key.into(), value.into());
        self
    }
    pub fn file(self, share: &str, path: &str) -> Self {
        self.with("share_id", share)
            .with("file_id", file_id(share, path))
    }
    pub fn for_path(self, path: &str) -> Self {
        if !enabled() {
            return self;
        }
        let share = self
            .ids
            .get("share_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(share) = share {
            self.file(&share, path)
        } else {
            self
        }
    }
    pub fn enter(self) -> ContextGuard {
        let old = CURRENT.with(|c| c.replace(self));
        ContextGuard(old)
    }
}
thread_local! { static CURRENT: RefCell<TraceContext> = RefCell::new(TraceContext::default()); }
pub struct ContextGuard(TraceContext);
impl Drop for ContextGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| c.replace(self.0.clone()));
    }
}
pub fn current_context() -> TraceContext {
    CURRENT.with(|c| c.borrow().clone())
}
pub fn file_id(share: &str, path: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(share);
    hash.update([0]);
    hash.update(path);
    hex::encode(&hash.finalize()[..8])
}
fn error_code(kind: std::io::ErrorKind) -> &'static str {
    use std::io::ErrorKind::*;
    match kind {
        NotFound => "socket_not_found",
        ConnectionRefused => "connection_refused",
        BrokenPipe => "broken_pipe",
        PermissionDenied => "permission_denied",
        TimedOut | WouldBlock => "timeout",
        InvalidData => "invalid_response",
        UnexpectedEof => "unexpected_eof",
        _ => "io_error",
    }
}
#[derive(Serialize)]
pub struct TraceError {
    pub kind: String,
    pub code: String,
    pub operation: String,
    pub message: String,
    pub chain: Vec<String>,
    pub os_kind: Option<String>,
    pub os_code: Option<i32>,
    pub os_message: Option<String>,
}
impl TraceError {
    pub fn new(kind: &str, operation: &str, error: &anyhow::Error) -> Self {
        let io = error
            .chain()
            .find_map(|e| e.downcast_ref::<std::io::Error>());
        Self {
            kind: kind.into(),
            code: io
                .map(|e| error_code(e.kind()).into())
                .unwrap_or_else(|| "unknown".into()),
            operation: operation.into(),
            message: error.to_string(),
            chain: error.chain().map(ToString::to_string).collect(),
            os_kind: io.map(|e| format!("{:?}", e.kind())),
            os_code: io.and_then(std::io::Error::raw_os_error),
            os_message: io.map(ToString::to_string),
        }
    }
}
#[derive(Serialize)]
pub struct Source {
    pub file: String,
    pub line: u32,
    pub function: String,
    pub thread: String,
    pub pid: u32,
}
#[derive(Serialize)]
pub struct TraceEvent {
    pub schema_version: u32,
    pub seq: u64,
    pub wall_time: String,
    pub wall_ms: u64,
    pub elapsed_us: u128,
    pub level: Level,
    pub side: String,
    pub component: Component,
    pub event: String,
    pub source: Source,
    pub context: TraceContext,
    pub fields: Value,
}
struct Session {
    root: PathBuf,
    _lock: File,
    latest: PathBuf,
    metadata: Value,
    side: String,
    started: Instant,
    writer: File,
    chunk: u32,
    bytes: u64,
    seq: u64,
    limit: u64,
    seen: HashSet<String>,
    transfers: HashMap<String, String>,
}
fn save_metadata(path: &Path, metadata: &Value) -> Result<()> {
    let tmp = path.join("metadata.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(metadata)?)?;
    file.sync_all()?;
    fs::rename(tmp, path.join("metadata.json"))?;
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    Ok(())
}
fn archive(root: &Path, latest: &Path, recovered: bool) -> Result<()> {
    let mut metadata: Value = serde_json::from_slice(&fs::read(latest.join("metadata.json"))?)?;
    if recovered && metadata["complete"] != true {
        metadata["recovered"] = json!(true);
        metadata["termination"] = json!("recovered_after_unclean_exit");
        save_metadata(latest, &metadata)?;
    }
    let id = metadata["trace_session_id"]
        .as_str()
        .context("missing session ID")?;
    anyhow::ensure!(
        !id.contains(['/', '\\']) && id != "." && id != "..",
        "invalid session ID"
    );
    let target = root.join("traces").join(format!("trace-{id}"));
    if target.exists() {
        return Ok(());
    }
    fs::create_dir_all(root.join("traces"))?;
    private(&root.join("traces"))?;
    let staging = root
        .join("traces")
        .join(format!(".archive-{id}-{}", new_id("copy")));
    fs::create_dir_all(&staging)?;
    private(&staging)?;
    for entry in fs::read_dir(latest)? {
        let entry = entry?;
        anyhow::ensure!(entry.file_type()?.is_file(), "unexpected trace entry");
        let copied = staging.join(entry.file_name());
        fs::copy(entry.path(), &copied)?;
        File::open(&copied)?.sync_all()?;
        let mut permissions = fs::metadata(&copied)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&copied, permissions)?;
    }
    fs::rename(staging, target)?;
    #[cfg(unix)]
    File::open(root.join("traces"))?.sync_all()?;
    Ok(())
}
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
/// Legacy path API: PC sessions now live beside the requested JSONL.
pub fn enable(path: &Path, side: &'static str) -> Result<()> {
    start(path.parent().context("trace parent")?, side, None)
}
pub fn start(root: &Path, side: &str, shared_id: Option<&str>) -> Result<()> {
    start_with_limit(root, side, shared_id, CHUNK_BYTES)
}
fn start_with_limit(root: &Path, side: &str, shared_id: Option<&str>, limit: u64) -> Result<()> {
    process_start();
    let mut guard = session().lock().unwrap();
    anyhow::ensure!(guard.is_none(), "trace already active");
    fs::create_dir_all(root)?;
    let mut lock_options = OpenOptions::new();
    lock_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options.mode(0o600);
    }
    let trace_lock = lock_options.open(root.join(".trace.lock"))?;
    trace_lock
        .try_lock_exclusive()
        .context("another process is writing this trace directory")?;
    let latest = root.join("Latest-trace");
    if latest.exists() {
        anyhow::ensure!(
            fs::symlink_metadata(&latest)?.file_type().is_dir(),
            "Latest-trace must be a real directory"
        );
    }
    if latest.exists() {
        archive(root, &latest, true)?;
        fs::remove_dir_all(&latest)?;
    }
    fs::create_dir(&latest)?;
    private(&latest)?;
    let id = shared_id
        .map(str::to_owned)
        .unwrap_or_else(|| new_id("session"));
    let metadata = json!({"trace_schema":2,"trace_session_id":id,"rowd_version":env!("CARGO_PKG_VERSION"),"platform":std::env::consts::OS,"side":side,"process_instance_id":process_id(),"daemon_instance_id":if side=="daemon" {Some(process_id())} else {None},"started_at":wall_time(wall_ms()),"finished_at":null,"complete":false,"recovered":false,"termination":"unknown","os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"pid":std::process::id(),"launch_mode":std::env::var("ROWD_LAUNCH_MODE").unwrap_or_else(|_|"foreground".into())});
    save_metadata(&latest, &metadata)?;
    let writer = open_chunk(&latest.join("trace-0001.jsonl"))?;
    *guard = Some(Session {
        root: root.into(),
        _lock: trace_lock,
        latest,
        metadata,
        side: side.into(),
        started: Instant::now(),
        writer,
        chunk: 1,
        bytes: 0,
        seq: 0,
        limit,
        seen: HashSet::new(),
        transfers: HashMap::new(),
    });
    *FAILURE.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
    ENABLED.store(true, Ordering::Relaxed);
    let flusher = FLUSHER.get_or_init(|| {
        std::thread::Builder::new()
            .name("rowd-trace-flush".into())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if enabled() {
                    let _ = flush();
                }
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    });
    if let Err(error) = flusher {
        let error = anyhow::anyhow!("flush thread: {error}");
        writer_failed(&mut guard, &error);
        return Err(error);
    }
    drop(guard);
    crate::trace_event!(
        Level::Info,
        Component::Trace,
        "TRACE_START",
        json!({"process_started_wall_ms":PROCESS_STARTED.get(),"activation":"opt_in"})
    );
    Ok(())
}
fn session_status(s: &Session) -> Value {
    json!({"metadata":s.metadata,"events":s.seq,"latest_trace":s.latest})
}
pub fn status() -> Value {
    let guard = session().lock().unwrap();
    let mut status = guard.as_ref().map(session_status).unwrap_or_else(|| {
        LAST_STATUS
            .get_or_init(|| Mutex::new(json!({})))
            .lock()
            .unwrap()
            .clone()
    });
    status["active"] = json!(enabled());
    status["error"] = json!(failure());
    status
}
pub fn flush() -> Result<()> {
    let mut guard = session().lock().unwrap();
    let result = if let Some(s) = guard.as_mut() {
        s.writer
            .flush()
            .and_then(|_| s.writer.sync_data())
            .map_err(anyhow::Error::from)
    } else {
        Ok(())
    };
    if let Err(error) = &result {
        writer_failed(&mut guard, error);
    }
    result
}
fn writer_failed(guard: &mut Option<Session>, error: &anyhow::Error) {
    let message = format!("TRACE_WRITER_FAILED: {error:#}");
    eprintln!("{message}");
    if let Some(s) = guard.as_mut() {
        s.metadata["writer_error"] = json!(TraceError::new("trace", "write", error));
        let _ = save_metadata(&s.latest, &s.metadata);
        *LAST_STATUS
            .get_or_init(|| Mutex::new(json!({})))
            .lock()
            .unwrap() = session_status(s);
    }
    *FAILURE.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(message);
    ENABLED.store(false, Ordering::Relaxed);
    *guard = None;
}
fn private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn open_chunk(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
pub fn disable() -> Result<()> {
    stop("trace_stop")
}
pub fn stop(termination: &str) -> Result<()> {
    crate::trace_event!(
        Level::Info,
        Component::Trace,
        "TRACE_STOP",
        json!({"termination":termination})
    );
    ENABLED.store(false, Ordering::Relaxed);
    let mut guard = session().lock().unwrap();
    let result = (|| -> Result<()> {
        if let Some(s) = guard.as_mut() {
            s.writer.flush()?;
            s.writer.sync_data()?;
            s.metadata["finished_at"] = json!(wall_time(wall_ms()));
            s.metadata["complete"] = json!(true);
            s.metadata["termination"] = json!(termination);
            s.metadata["event_count"] = json!(s.seq);
            s.metadata["chunk_count"] = json!(s.chunk);
            save_metadata(&s.latest, &s.metadata)?;
            archive(&s.root, &s.latest, false)?;
            *LAST_STATUS
                .get_or_init(|| Mutex::new(json!({})))
                .lock()
                .unwrap() = session_status(s);
        }
        Ok(())
    })();
    if let Err(error) = &result {
        writer_failed(&mut guard, error);
    } else {
        *guard = None;
    }
    result
}
#[macro_export]
macro_rules! trace_event {
    ($level:expr,$component:expr,$event:expr,$fields:expr) => {
        if $crate::trace::enabled() {
            $crate::trace::emit(
                $level,
                $component,
                $event,
                $fields,
                file!(),
                line!(),
                module_path!(),
            )
        }
    };
}
#[macro_export]
macro_rules! trace_legacy_event {
    ($component:expr,$event:expr,$share:expr,$path:expr,$bytes:expr,$duration:expr,$detail:expr $(,)?) => {
        if $crate::trace::enabled() {
            $crate::trace::event_at(
                $component,
                $event,
                $share,
                $path,
                $bytes,
                $duration,
                $detail,
                file!(),
                line!(),
                module_path!(),
            )
        }
    };
}
// Redact known secret keys recursively at the writer boundary.
fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                let k = key.to_ascii_lowercase();
                if [
                    "secret",
                    "private_key",
                    "credential",
                    "password",
                    "invitation",
                    "qr",
                    "payload",
                    "content",
                ]
                .iter()
                .any(|s| k.contains(s))
                    && k != "payload_size"
                {
                    *v = json!("[REDACTED]");
                } else {
                    redact(v);
                }
            }
        }
        Value::Array(values) => {
            for v in values {
                redact(v)
            }
        }
        Value::String(text) => {
            while let Some(start) = text.find("rowd1:") {
                let end = text[start..]
                    .find(|c: char| {
                        !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ':')
                    })
                    .map(|n| start + n)
                    .unwrap_or(text.len());
                text.replace_range(start..end, "[REDACTED invitation]");
            }
            for secret in SECRETS
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap()
                .iter()
            {
                *text = text.replace(secret, "[REDACTED]");
            }
        }
        _ => {}
    }
}
pub fn emit(
    level: Level,
    component: Component,
    name: &str,
    mut fields: Value,
    file: &str,
    line: u32,
    function: &str,
) {
    if !enabled() {
        return;
    }
    let mut guard = session().lock().unwrap();
    if !enabled() {
        return;
    }
    let Some(s) = guard.as_mut() else { return };
    let mut context = current_context();
    context.ids.insert(
        "trace_session_id".into(),
        s.metadata["trace_session_id"].clone(),
    );
    context
        .ids
        .insert("process_instance_id".into(), json!(process_id()));
    if s.side == "daemon" {
        context
            .ids
            .insert("daemon_instance_id".into(), json!(process_id()));
    }
    let kotlin_thread = context
        .ids
        .remove("_source_thread")
        .and_then(|v| v.as_str().map(str::to_owned));
    redact(&mut fields);
    let mut context_value = json!(&context);
    redact(&mut context_value);
    if let Ok(redacted) = serde_json::from_value(context_value) {
        context = redacted;
    }
    let ms = wall_ms();
    s.seq += 1;
    let event = TraceEvent {
        schema_version: 2,
        seq: s.seq,
        wall_time: wall_time(ms),
        wall_ms: ms,
        elapsed_us: s.started.elapsed().as_micros(),
        level,
        side: s.side.clone(),
        component,
        event: name.to_ascii_uppercase(),
        source: Source {
            file: file.into(),
            line,
            function: function.into(),
            thread: kotlin_thread.unwrap_or_else(|| {
                format!(
                    "{}:{:?}",
                    std::thread::current().name().unwrap_or("unnamed"),
                    std::thread::current().id()
                )
            }),
            pid: std::process::id(),
        },
        context,
        fields,
    };
    let result = (|| -> Result<()> {
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        if s.bytes > 0 && s.bytes + bytes.len() as u64 > s.limit {
            s.writer.flush()?;
            s.chunk += 1;
            s.writer = open_chunk(&s.latest.join(format!("trace-{:04}.jsonl", s.chunk)))?;
            s.bytes = 0;
        }
        s.writer.write_all(&bytes)?;
        s.bytes += bytes.len() as u64;
        if matches!(level, Level::Error)
            || name.eq_ignore_ascii_case("round_end")
            || name == "INVARIANT_VIOLATION"
        {
            s.writer.flush()?;
            s.writer.sync_data()?;
        }
        if name.eq_ignore_ascii_case("round_end") {
            s.transfers.clear();
        }
        Ok(())
    })();
    if let Err(error) = result {
        writer_failed(&mut guard, &error);
    }
}
#[track_caller]
pub fn event(
    component: &str,
    name: &str,
    share: Option<&str>,
    path: Option<&str>,
    bytes: Option<u64>,
    duration: Option<Instant>,
    detail: Option<&str>,
) {
    let loc = std::panic::Location::caller();
    event_at(
        component,
        name,
        share,
        path,
        bytes,
        duration,
        detail,
        loc.file(),
        loc.line(),
        "legacy_event",
    );
}
#[allow(clippy::too_many_arguments)]
pub fn event_at(
    component: &str,
    name: &str,
    share: Option<&str>,
    path: Option<&str>,
    bytes: Option<u64>,
    duration: Option<Instant>,
    detail: Option<&str>,
    source_file: &str,
    source_line: u32,
    source_module: &str,
) {
    if !enabled() {
        return;
    }
    let mut ctx = current_context();
    if let Some(share) = share {
        ctx = ctx.with("share_id", share);
        if let Some(path) = path {
            ctx = ctx.file(share, path);
        }
    }
    let transfer_event = matches!(
        name,
        "get_sent"
            | "get_received"
            | "put_sent"
            | "snapshot_start"
            | "snapshot_end"
            | "blob_send_start"
            | "blob_send_end"
            | "blob_receive_start"
            | "blob_receive_end"
            | "install_start"
            | "install_end"
            | "accept_received"
            | "accept_sent"
            | "ack_received"
            | "ack_sent"
    );
    let mut queued = false;
    if transfer_event {
        if let Some(id) = ctx.ids.get("file_id").and_then(Value::as_str) {
            let key = format!(
                "{}:{id}",
                ctx.ids
                    .get("round_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unscoped")
            );
            if let Some(session) = session().lock().unwrap().as_mut() {
                let transfer = session
                    .transfers
                    .entry(key)
                    .or_insert_with(|| {
                        queued = true;
                        new_id("transfer")
                    })
                    .clone();
                ctx = ctx.with("transfer_id", transfer);
            }
        }
    }
    let _scope = ctx.enter();
    let component = match component {
        "saf" => Component::SAF,
        "scanner" => Component::Scanner,
        "network" => Component::Network,
        "connection" => Component::Connection,
        _ => {
            if name.starts_with("state_") {
                Component::StateStore
            } else if name.contains("scan") || name.contains("hash") {
                Component::Scanner
            } else if name.starts_with("round") {
                Component::Round
            } else {
                Component::Transfer
            }
        }
    };
    if queued {
        emit(
            Level::Trace,
            Component::Transfer,
            "TRANSFER_QUEUED",
            json!({"relative_path":path,"bytes":bytes}),
            source_file,
            source_line,
            source_module,
        );
        emit(
            Level::Trace,
            Component::Transfer,
            "TRANSFER_START",
            json!({"relative_path":path,"bytes":bytes}),
            source_file,
            source_line,
            source_module,
        );
    }
    let canonical = match name {
        "delta_fallback" => "FULL_SCAN_FALLBACK",
        "accept_received" | "ack_received" => "REMOTE_ACK",
        _ => name,
    };
    let mut fields = json!({"bytes":bytes,"duration_us":duration.map(|s|s.elapsed().as_micros()),"detail":detail,"relative_path":path});
    if matches!(
        name,
        "delta_fallback" | "delta_unavailable" | "manifest_source"
    ) {
        fields["reason"] = json!(detail.unwrap_or("legacy_callsite_no_reason"));
    }
    emit(
        Level::Trace,
        component,
        canonical,
        fields,
        source_file,
        source_line,
        source_module,
    );
    if transfer_event
        && matches!(
            name,
            "accept_received" | "ack_received" | "accept_sent" | "ack_sent"
        )
        && path.is_some()
    {
        emit(
            Level::Info,
            Component::Transfer,
            "TRANSFER_COMPLETE",
            json!({"confirmation":name}),
            source_file,
            source_line,
            source_module,
        );
    }
}

#[track_caller]
pub fn transfer_context(share: &str, path: &str) -> TraceContext {
    let ctx = current_context();
    if !enabled() {
        return ctx;
    }
    let mut ctx = ctx.file(share, path);
    let id = ctx.ids["file_id"].as_str().unwrap();
    let key = format!(
        "{}:{id}",
        ctx.ids
            .get("round_id")
            .and_then(Value::as_str)
            .unwrap_or("unscoped")
    );
    let mut fresh = false;
    if let Some(session) = session().lock().unwrap().as_mut() {
        let id = session
            .transfers
            .entry(key)
            .or_insert_with(|| {
                fresh = true;
                new_id("transfer")
            })
            .clone();
        ctx = ctx.with("transfer_id", id);
    }
    if fresh {
        let _scope = ctx.clone().enter();
        let loc = std::panic::Location::caller();
        for name in ["TRANSFER_QUEUED", "TRANSFER_START"] {
            emit(
                Level::Trace,
                Component::Transfer,
                name,
                json!({"relative_path":path}),
                loc.file(),
                loc.line(),
                "transfer_context",
            );
        }
    }
    ctx
}

#[track_caller]
pub fn record_error(
    component: Component,
    event: &str,
    kind: &str,
    operation: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    if enabled() {
        let loc = std::panic::Location::caller();
        emit(
            Level::Error,
            component,
            event,
            json!({"error":TraceError::new(kind,operation,&error)}),
            loc.file(),
            loc.line(),
            operation,
        );
    }
    error
}

#[track_caller]
pub fn acknowledge_file(share: &str, path: &str, confirmation: &str) {
    if !enabled() {
        return;
    }
    let mut ctx = current_context().file(share, path);
    ctx.ids.remove("transfer_id");
    let key = format!(
        "{}:{}",
        ctx.ids
            .get("round_id")
            .and_then(Value::as_str)
            .unwrap_or("unscoped"),
        ctx.ids["file_id"].as_str().unwrap()
    );
    let transfer = session()
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|s| s.transfers.get(&key).cloned());
    if let Some(id) = &transfer {
        ctx = ctx.with("transfer_id", id.clone());
    }
    let _scope = ctx.enter();
    let loc = std::panic::Location::caller();
    emit(
        Level::Trace,
        Component::Protocol,
        "REMOTE_ACK",
        json!({"confirmation":confirmation,"relative_path":path}),
        loc.file(),
        loc.line(),
        "acknowledge_file",
    );
    if transfer.is_some() {
        emit(
            Level::Info,
            Component::Transfer,
            "TRANSFER_COMPLETE",
            json!({"confirmation":confirmation}),
            loc.file(),
            loc.line(),
            "acknowledge_file",
        );
    }
}

pub fn failure() -> Option<String> {
    FAILURE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone()
}
/// Evidence only: this helper never mutates runtime state.
#[track_caller]
pub fn invariant(valid: bool, name: &str, state: Value) {
    if enabled() && !valid {
        let loc = std::panic::Location::caller();
        emit(
            Level::Error,
            Component::Trace,
            "INVARIANT_VIOLATION",
            json!({"invariant":name,"state":state}),
            loc.file(),
            loc.line(),
            "invariant",
        );
    }
}
pub struct Lifecycle {
    component: Component,
    event: &'static str,
    started: Instant,
    context: TraceContext,
}
impl Lifecycle {
    pub fn end_event(component: Component, event: &'static str) -> Self {
        Self {
            component,
            event,
            started: Instant::now(),
            context: current_context(),
        }
    }
}
impl Drop for Lifecycle {
    fn drop(&mut self) {
        let _scope = self.context.clone().enter();
        emit(
            Level::Info,
            self.component,
            self.event,
            json!({"reason":"scope_exit","duration_us":self.started.elapsed().as_micros()}),
            file!(),
            line!(),
            module_path!(),
        );
    }
}

/// Register only actual observations, never infer discovery from transfer events.
#[track_caller]
pub fn first_seen(
    share: &str,
    path: &str,
    source: &str,
    modified: Option<u64>,
    created: Option<u64>,
) {
    if !enabled() {
        return;
    }
    let id = file_id(share, path);
    let first = session()
        .lock()
        .unwrap()
        .as_mut()
        .is_some_and(|s| s.seen.insert(id));
    if !first {
        return;
    }
    let _scope = current_context().file(share, path).enter();
    let ms = wall_ms();
    let loc = std::panic::Location::caller();
    emit(
        Level::Trace,
        Component::Filesystem,
        "FILE_FIRST_SEEN",
        json!({"relative_path":path,"source":source,"first_observed_wall_ms":ms,"metadata_created_ms":created,"metadata_modified_ms":modified,"provider_last_modified_ms":null,"file_age_at_first_observation_ms":modified.map(|m|ms as i128-m as i128)}),
        loc.file(),
        loc.line(),
        "first_seen",
    );
}
#[track_caller]
pub fn observed_file(path: &str, source: &str, metadata: &fs::Metadata) {
    if !enabled() {
        return;
    }
    let ctx = current_context();
    let Some(share) = ctx.ids.get("share_id").and_then(Value::as_str) else {
        return;
    };
    first_seen(
        share,
        path,
        ctx.ids
            .get("discovery_source")
            .and_then(Value::as_str)
            .unwrap_or(source),
        metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64),
        metadata
            .created()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64),
    );
}

/// Kotlin routes structured events through the same writer and sequence as JNI Rust.
pub fn ingest_android(value: Value) -> Result<()> {
    let component = match value["component"].as_str().unwrap_or("SAF") {
        "Watcher" => Component::Watcher,
        "Scanner" => Component::Scanner,
        "Scheduler" => Component::Scheduler,
        "Round" => Component::Round,
        "Network" => Component::Network,
        "Filesystem" => Component::Filesystem,
        "Android-Service" => Component::AndroidService,
        "Terminal-output" => Component::TerminalOutput,
        _ => Component::SAF,
    };
    let level = match value["level"].as_str().unwrap_or("trace") {
        "error" => Level::Error,
        "warn" => Level::Warn,
        "info" => Level::Info,
        "debug" => Level::Debug,
        _ => Level::Trace,
    };
    let mut ctx = current_context();
    if let Some(map) = value["context"].as_object() {
        ctx.ids.extend(map.clone());
    }
    ctx = ctx.with("_source_thread", value["source"]["thread"].clone());
    if value["event"] == "PROCESS_START" {
        if let Some(s) = session().lock().unwrap().as_mut() {
            for key in ["android_version", "device_model", "app_version"] {
                s.metadata[key] = value["fields"][key].clone();
            }
            save_metadata(&s.latest, &s.metadata)?;
        }
    }
    if value["event"] == "FILE_FIRST_SEEN" {
        if let Some(id) = ctx.ids.get("file_id").and_then(Value::as_str) {
            if !session()
                .lock()
                .unwrap()
                .as_mut()
                .is_some_and(|s| s.seen.insert(id.into()))
            {
                return Ok(());
            }
        }
    }
    let _scope = ctx.enter();
    emit(
        level,
        component,
        value["event"].as_str().context("missing event")?,
        value["fields"].clone(),
        value["source"]["file"].as_str().unwrap_or("Kotlin"),
        0,
        value["source"]["function"].as_str().unwrap_or("unknown"),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistence_schema_chunking_recovery_and_redaction() {
        let dir = tempfile::tempdir().unwrap();
        register_secret("PRIVATE_TEST_KEY");
        start_with_limit(dir.path(), "pc", None, 32 * 1024).unwrap();
        let _ctx = TraceContext::default()
            .with("round_id", "round-test")
            .enter();
        std::thread::scope(|scope| {
            for producer in 0..4 {
                let context = current_context();
                scope.spawn(move || {
                    let _context = context.enter();
                    for index in 0..1000 {
                        crate::trace_event!(Level::Trace, Component::Scanner, "FILE_ENUMERATED", json!({"producer":producer,"index":index,"secret":"DO_NOT_WRITE","private_key":"PRIVATE_TEST_KEY","message":"error PRIVATE_TEST_KEY rowd1:privateQR","payload_size":1}));
                    }
                });
            }
        });
        let latest = dir.path().join("Latest-trace");
        assert!(fs::metadata(latest.join("trace-0001.jsonl")).unwrap().len() > 0);
        stop("trace_stop").unwrap();
        assert!(latest.exists());
        assert_eq!(fs::read_dir(dir.path().join("traces")).unwrap().count(), 1);
        let mut chunks: Vec<_> = fs::read_dir(&latest)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        chunks.sort();
        assert!(chunks.len() > 1);
        let mut seq = 0;
        let mut elapsed = 0;
        let mut own = 0;
        let mut identities = HashSet::new();
        for p in chunks {
            let text = fs::read_to_string(p).unwrap();
            assert!(!text.contains("DO_NOT_WRITE"));
            assert!(!text.contains("PRIVATE_TEST_KEY"));
            assert!(!text.contains("rowd1:"));
            for line in text.lines() {
                let v: Value = serde_json::from_str(line).unwrap();
                seq += 1;
                assert_eq!(v["seq"], seq);
                assert_eq!(v["schema_version"], 2);
                for field in [
                    "seq",
                    "wall_time",
                    "wall_ms",
                    "elapsed_us",
                    "level",
                    "side",
                    "component",
                    "event",
                    "source",
                    "context",
                    "fields",
                ] {
                    assert!(v.get(field).is_some(), "missing {field}");
                }
                assert!(v["context"]["trace_session_id"].is_string());
                assert!(v["context"]["process_instance_id"].is_string());
                if v["context"]["round_id"] == "round-test" && v["event"] == "FILE_ENUMERATED" {
                    own += 1;
                    assert!(identities.insert((
                        v["fields"]["producer"].as_u64().unwrap(),
                        v["fields"]["index"].as_u64().unwrap()
                    )));
                }
                assert!(v["source"]["line"].as_u64().unwrap() > 0);
                let now = v["elapsed_us"].as_u64().unwrap();
                assert!(now >= elapsed);
                elapsed = now;
            }
        }
        assert_eq!(own, 4000);
        start(dir.path(), "pc", None).unwrap();
        let old = status()["metadata"]["trace_session_id"].clone();
        // Simulate abrupt termination without calling stop.
        ENABLED.store(false, Ordering::Relaxed);
        *session().lock().unwrap() = None;
        start(dir.path(), "pc", None).unwrap();
        let recovered: Value = serde_json::from_slice(
            &fs::read(
                dir.path()
                    .join("traces")
                    .join(format!("trace-{}", old.as_str().unwrap()))
                    .join("metadata.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(recovered["complete"], false);
        assert_eq!(recovered["recovered"], true);
        disable().unwrap();
        let io = anyhow::Error::new(std::io::Error::from_raw_os_error(32)).context("send response");
        let error = TraceError::new("daemon_ipc", "send_status_response", &io);
        assert_eq!(error.os_code, Some(32));
        assert_eq!(error.chain.len(), 2);
        assert_eq!(error.code, "broken_pipe");
        start(dir.path(), "pc", Some("shared-android-session")).unwrap();
        ingest_android(json!({"component":"Watcher","event":"OBSERVER_CALLBACK","source":{"file":"SyncService.kt","function":"onChange","thread":"main"},"context":{"share_id":"camera"},"fields":{"self_change":false}})).unwrap();
        let line: Value = serde_json::from_str(
            fs::read_to_string(latest.join("trace-0001.jsonl"))
                .unwrap()
                .lines()
                .find(|line| line.contains("OBSERVER_CALLBACK"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            line["context"]["trace_session_id"],
            "shared-android-session"
        );
        assert_eq!(line["source"]["thread"], "main");
        let before = status()["events"].as_u64().unwrap();
        acknowledge_file("share", "unchanged", "ack_batch_sent");
        assert_eq!(status()["events"], before + 1); // No fabricated queued/start/complete events.
        let transfer = transfer_context("share", "changed");
        let transfer_id = transfer.ids["transfer_id"].clone();
        let _transfer = transfer.enter();
        acknowledge_file("share", "unchanged", "ack_batch_sent");
        acknowledge_file("share", "changed", "ack_batch_sent");
        let records: Vec<Value> = fs::read_to_string(latest.join("trace-0001.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.last().unwrap()["event"], "TRANSFER_COMPLETE");
        assert_eq!(
            records.last().unwrap()["context"]["transfer_id"],
            transfer_id
        );
        let unchanged = records
            .iter()
            .rev()
            .find(|v| v["fields"]["relative_path"] == "unchanged")
            .unwrap();
        assert!(unchanged["context"].get("transfer_id").is_none());
        // Read-only descriptor makes writes fail without depending on disk capacity.
        session().lock().unwrap().as_mut().unwrap().writer =
            File::open(latest.join("trace-0001.jsonl")).unwrap();
        crate::trace_event!(
            Level::Error,
            Component::Trace,
            "TEST_WRITE_FAILURE",
            json!({})
        );
        assert!(!enabled());
        assert!(status()["error"]
            .as_str()
            .unwrap()
            .contains("TRACE_WRITER_FAILED"));
        start(dir.path(), "pc", None).unwrap();
        disable().unwrap();
    }
}
