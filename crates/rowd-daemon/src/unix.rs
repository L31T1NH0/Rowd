//! Unix socket transport, resident lifecycle, and systemd user integration.
use crate::{Reply, Request, DAEMON_IPC_VERSION};

use anyhow::{anyhow, bail, ensure, Context, Result};
use fs2::FileExt;
use rowd_app::{App, AppSignal};
use rowd_core::{
    trace::{self, Component, Level, TraceError},
    trace_event,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LOG_LIMIT: u64 = 1_048_576;
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn home_path(home: &Path) -> Result<PathBuf> {
    fs::create_dir_all(home)?;
    Ok(home.canonicalize()?)
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_dir(),
        "runtime directory is not a real directory"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn socket_path(home: &Path) -> Result<PathBuf> {
    let home = home_path(home)?;
    let fallback = home.join(".rowd/run/rowd");
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .map(|p| p.join("rowd"))
        .filter(|p| private_dir(p).is_ok())
        .unwrap_or(fallback);
    private_dir(&dir)?;
    let id = hex::encode(Sha256::digest(home.as_os_str().as_encoded_bytes()));
    Ok(dir.join(format!("{}.sock", &id[..20])))
}
fn stream_path(home: &Path, stream: &str) -> Result<PathBuf> {
    ensure!(stream == "logs" || stream == "events", "invalid stream");
    Ok(home.join(".rowd/logs").join(if stream == "logs" {
        "daemon.log"
    } else {
        "events.jsonl"
    }))
}
fn send_line<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let result = (|| -> Result<()> {
        // serde_json's error source skips the wrapped io::Error. Recover it so
        // ErrorKind and raw errno survive the serialization boundary.
        serde_json::to_writer(&mut *stream, value).map_err(|error| {
            if error.is_io() {
                anyhow::Error::new(std::io::Error::from(error)).context("write IPC frame")
            } else {
                anyhow::Error::new(error)
            }
        })?;
        stream.write_all(b"\n")?;
        Ok(())
    })();
    if let Err(error) = &result {
        let context = trace::current_context();
        let operation = context
            .ids
            .get("ipc_write_operation")
            .and_then(Value::as_str)
            .unwrap_or("write_ipc_frame");
        trace_event!(
            Level::Error,
            Component::DaemonIpc,
            "IPC_WRITE_FAILED",
            json!({"error":TraceError::new("daemon_ipc", operation, error)})
        );
    }
    result
}
// Callers identify semantic responses and subscription items; send_line is only I/O.
fn send_response(stream: &mut UnixStream, reply: &Reply, started: Instant) -> Result<()> {
    let context = trace::current_context();
    let command = context
        .ids
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let _scope = context
        .clone()
        .with("ipc_write_operation", format!("send_{command}_response"))
        .enter();
    send_line(stream, reply)?;
    trace_event!(
        Level::Trace,
        Component::DaemonIpc,
        "IPC_RESPONSE_SENT",
        json!({"duration_us":started.elapsed().as_micros()})
    );
    Ok(())
}
fn send_stream_item(stream: &mut UnixStream, line: &str) -> Result<()> {
    let _scope = trace::current_context()
        .with("ipc_write_operation", "send_subscription_item")
        .enter();
    send_line(stream, &line)?;
    trace_event!(
        Level::Trace,
        Component::DaemonIpc,
        "IPC_STREAM_ITEM_SENT",
        json!({})
    );
    Ok(())
}
fn receive_line<T: for<'a> Deserialize<'a>>(stream: &mut UnixStream) -> Result<T> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        ensure!(line.len() < 65_536, "oversized IPC message");
        line.push(byte[0]);
    }
    ensure!(!line.is_empty(), "empty IPC message");
    Ok(serde_json::from_slice(&line)?)
}
pub fn request(home: &Path, command: &str) -> Result<Reply> {
    request_args(home, command, Value::Null)
}
pub fn request_args(home: &Path, command: &str, args: Value) -> Result<Reply> {
    let mut stream = UnixStream::connect(socket_path(home)?)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    send_line(
        &mut stream,
        &Request {
            version: DAEMON_IPC_VERSION,
            command: command.into(),
            stream: None,
            args,
        },
    )?;
    let reply: Reply = receive_line(&mut stream)?;
    ensure!(
        reply.version == DAEMON_IPC_VERSION,
        "incompatible daemon IPC version"
    );
    ensure!(
        reply.ok,
        "{}",
        reply.error.as_deref().unwrap_or("daemon error")
    );
    Ok(reply)
}
pub fn active(home: &Path) -> bool {
    request(home, "ping").is_ok()
}
pub fn ipc_present(home: &Path) -> bool {
    socket_path(home).is_ok_and(|path| UnixStream::connect(path).is_ok())
}

struct Bus {
    path: PathBuf,
    subscribers: Mutex<Vec<mpsc::Sender<String>>>,
}
impl Bus {
    fn new(path: PathBuf) -> Result<Self> {
        private_dir(path.parent().context("log directory")?)?;
        Ok(Self {
            path,
            subscribers: Mutex::new(Vec::new()),
        })
    }
    fn emit(&self, line: String) {
        let mut subscribers = self.subscribers.lock().unwrap();
        let _ = (|| -> Result<()> {
            if self.path.metadata().is_ok_and(|m| m.len() >= LOG_LIMIT) {
                let rotated = self.path.with_extension("1");
                let _ = fs::remove_file(&rotated);
                fs::rename(&self.path, rotated)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&self.path)?;
            writeln!(file, "{line}")?;
            Ok(())
        })();
        subscribers.retain(|subscriber| subscriber.send(line.clone()).is_ok());
    }
    fn subscribe(&self) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }
}
fn ok(data: Option<Value>) -> Reply {
    Reply {
        version: DAEMON_IPC_VERSION,
        ok: true,
        data,
        error: None,
    }
}
fn err(message: &str) -> Reply {
    Reply {
        version: DAEMON_IPC_VERSION,
        ok: false,
        data: None,
        error: Some(message.into()),
    }
}
struct Runtime {
    home: PathBuf,
    started: u64,
    mode: String,
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    logs: Arc<Bus>,
    events: Arc<Bus>,
}
impl Runtime {
    fn status(&self) -> Result<Value> {
        Ok(
            json!({ "pid": std::process::id(), "started_at": self.started,
            "uptime_seconds": now().saturating_sub(self.started), "launch_mode": self.mode,
            "ipc_version": DAEMON_IPC_VERSION, "rowd_version": env!("CARGO_PKG_VERSION"),
            "connected": self.connected.load(Ordering::Relaxed), "app": App::new(&self.home).snapshot()?, }),
        )
    }
    fn handle(&self, stream: UnixStream) -> Result<()> {
        let request_id = trace::new_id("request");
        let _scope = trace::current_context()
            .with("request_id", request_id)
            .with("socket", socket_path(&self.home)?.display().to_string())
            .enter();
        let result = self.handle_request(stream);
        if let Err(error) = &result {
            trace_event!(
                Level::Error,
                Component::DaemonIpc,
                "IPC_HANDLER_FAILED",
                json!({"error":TraceError::new("daemon_ipc","handle_request",error)})
            );
        }
        result
    }
    fn handle_request(&self, mut stream: UnixStream) -> Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let started = Instant::now();
        trace_event!(
            Level::Trace,
            Component::DaemonIpc,
            "IPC_ACCEPT",
            json!({"socket":socket_path(&self.home)?.display().to_string()})
        );
        trace_event!(
            Level::Trace,
            Component::DaemonIpc,
            "IPC_REQUEST_RECEIVED",
            json!({})
        );
        let req: Request = receive_line(&mut stream).map_err(|error| {
            trace_event!(
                Level::Error,
                Component::DaemonIpc,
                if error
                    .chain()
                    .any(
                        |e| e.downcast_ref::<std::io::Error>().is_some_and(|e| matches!(
                            e.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ))
                    )
                {
                    "IPC_TIMEOUT"
                } else {
                    "IPC_READ_FAILED"
                },
                json!({"error":TraceError::new("daemon_ipc","read_request",&error)})
            );
            error
        })?;
        let _command_scope = trace::current_context()
            .with("command", req.command.clone())
            .enter();
        trace_event!(
            Level::Trace,
            Component::DaemonIpc,
            "IPC_REQUEST_PARSED",
            json!({"command":req.command})
        );
        trace_event!(
            Level::Trace,
            Component::DaemonIpc,
            "IPC_RESPONSE_START",
            json!({"command":req.command})
        );
        if req.version != DAEMON_IPC_VERSION {
            send_response(
                &mut stream,
                &err("incompatible daemon IPC version"),
                started,
            )?;
            return Ok(());
        }
        match req.command.as_str() {
            "trace_start" => {
                let root = match req.args.get("output_dir") {
                    Some(Value::String(path)) if Path::new(path).is_absolute() => {
                        PathBuf::from(path)
                    }
                    Some(_) => {
                        send_response(&mut stream, &err("output_dir must be absolute"), started)?;
                        return Ok(());
                    }
                    None => self.home.join(".rowd"),
                };
                match trace::start(&root, "daemon", None) {
                    Ok(()) => {
                        trace_event!(
                            Level::Trace,
                            Component::DaemonIpc,
                            "IPC_ACCEPT",
                            json!({"socket":socket_path(&self.home)?.display().to_string(),"trace_activation_request":true})
                        );
                        trace_event!(
                            Level::Trace,
                            Component::DaemonIpc,
                            "IPC_REQUEST_RECEIVED",
                            json!({"command":req.command})
                        );
                        trace_event!(
                            Level::Trace,
                            Component::DaemonIpc,
                            "IPC_REQUEST_PARSED",
                            json!({"command":req.command})
                        );
                        trace_event!(
                            Level::Trace,
                            Component::DaemonIpc,
                            "IPC_RESPONSE_START",
                            json!({"command":req.command})
                        );
                        trace_event!(
                            Level::Info,
                            Component::Daemon,
                            "DAEMON_START",
                            json!({"started_at":self.started,"trace_activation_snapshot":true})
                        );
                        trace_event!(
                            Level::Info,
                            Component::Daemon,
                            "DAEMON_READY",
                            json!({"ready":self.ready.load(Ordering::Relaxed),"launch_mode":self.mode})
                        );
                        send_response(&mut stream, &ok(Some(trace::status())), started)?;
                    }
                    Err(error) => send_response(&mut stream, &err(&format!("{error:#}")), started)?,
                }
            }
            "trace_status" => send_response(&mut stream, &ok(Some(trace::status())), started)?,
            "trace_flush" => {
                trace::flush()?;
                send_response(&mut stream, &ok(Some(trace::status())), started)?;
            }
            "trace_stop" => {
                trace::stop("trace_stop")?;
                send_response(&mut stream, &ok(Some(trace::status())), started)?;
            }
            "ping" if self.ready.load(Ordering::Relaxed) => {
                send_response(&mut stream, &ok(None), started)?
            }
            "status" if self.ready.load(Ordering::Relaxed) => {
                send_response(&mut stream, &ok(Some(self.status()?)), started)?
            }
            "stop" => {
                send_response(&mut stream, &ok(None), started)?;
                self.stop.store(true, Ordering::Relaxed);
            }
            "subscribe" => {
                let bus = match req.stream.as_deref() {
                    Some("logs") => &self.logs,
                    Some("events") => &self.events,
                    _ => {
                        send_response(&mut stream, &err("invalid stream"), started)?;
                        return Ok(());
                    }
                };
                let rx = bus.subscribe();
                send_response(&mut stream, &ok(None), started)?;
                for line in recent_lines(&bus.path, 100)? {
                    send_stream_item(&mut stream, &line)?;
                }
                while !self.stop.load(Ordering::Relaxed) {
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(line) => {
                            if send_stream_item(&mut stream, &line).is_err() {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }
            _ => send_response(&mut stream, &err("unknown command"), started)?,
        }
        trace_event!(
            Level::Trace,
            Component::DaemonIpc,
            "IPC_CLIENT_CLOSED",
            json!({"reason":"handler_finished"})
        );
        Ok(())
    }
}
fn recent_lines(path: &Path, count: usize) -> Result<Vec<String>> {
    let mut value = String::new();
    match File::open(path) {
        Ok(mut file) => {
            file.read_to_string(&mut value)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    Ok(value
        .lines()
        .rev()
        .take(count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(str::to_owned)
        .collect())
}
pub fn history(home: &Path, stream: &str) -> Result<Vec<String>> {
    recent_lines(&stream_path(home, stream)?, 100)
}
pub fn follow(home: &Path, stream_name: &str) -> Result<()> {
    ensure!(
        stream_name == "logs" || stream_name == "events",
        "invalid stream"
    );
    let mut stream = UnixStream::connect(socket_path(home)?)?;
    send_line(
        &mut stream,
        &Request {
            version: DAEMON_IPC_VERSION,
            command: "subscribe".into(),
            stream: Some(stream_name.into()),
            args: Value::Null,
        },
    )?;
    let reply: Reply = receive_line(&mut stream)?;
    ensure!(
        reply.version == DAEMON_IPC_VERSION && reply.ok,
        "{}",
        reply.error.as_deref().unwrap_or("incompatible daemon IPC")
    );
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        let decoded: String = serde_json::from_str(line.trim_end())?;
        println!("{decoded}");
        line.clear();
    }
    Ok(())
}

pub fn run(home: &Path) -> Result<()> {
    let home = home_path(home)?;
    let runtime_dir = home.join(".rowd");
    private_dir(&runtime_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(runtime_dir.join("daemon.lock"))?;
    lock.try_lock_exclusive()
        .context("daemon already running for this ROWD_HOME")?;
    let path = socket_path(&home)?;
    if let Ok(metadata) = path.symlink_metadata() {
        ensure!(!active(&home), "daemon already running for this ROWD_HOME");
        ensure!(metadata.file_type().is_socket(), "IPC path is not a socket");
        fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    struct SocketCleanup(PathBuf);
    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _cleanup = SocketCleanup(path);
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let connected = Arc::new(AtomicBool::new(false));
    let logs = Arc::new(Bus::new(stream_path(&home, "logs")?)?);
    let events = Arc::new(Bus::new(stream_path(&home, "events")?)?);
    let mode = std::env::var("ROWD_LAUNCH_MODE").unwrap_or_else(|_| "foreground".into());
    ensure!(
        ["foreground", "manual", "systemd"].contains(&mode.as_str()),
        "invalid launch mode"
    );
    let state = Arc::new(Runtime {
        home: home.clone(),
        started: now(),
        mode,
        stop: stop.clone(),
        ready: ready.clone(),
        connected: connected.clone(),
        logs: logs.clone(),
        events: events.clone(),
    });
    logs.emit(format!("{} daemon started mode={}", now(), state.mode));
    let app_home = home.clone();
    let worker = std::thread::spawn(move || {
        let result = App::new(&app_home).serve_observed(stop.clone(),
            |message| logs.emit(format!("{} {}", now(), message.replace('\n', " "))),
            |signal: AppSignal| {
                if signal.kind == "ready" { ready.store(true, Ordering::Relaxed); }
                if signal.kind == "connected" { connected.store(true, Ordering::Relaxed); }
                if signal.kind == "disconnected" && !connected.swap(false, Ordering::Relaxed) { return; }
                events.emit(json!({"version": DAEMON_IPC_VERSION, "timestamp": now(), "kind": signal.kind,
                    "share_id": signal.share_id, "duration_ms": signal.duration_ms, "transferred": signal.transferred}).to_string());
            });
        result
    });
    let mut snapshot_at = Instant::now();
    let mut trace_failure_logged = None;
    let mut ipc_error = None;
    let mut handlers = Vec::new();
    while !state.stop.load(Ordering::Relaxed) && !worker.is_finished() {
        let failure = trace::failure();
        if failure.is_some() && failure != trace_failure_logged {
            state
                .logs
                .emit(format!("{} {}", now(), failure.as_deref().unwrap()));
            trace_failure_logged = failure;
        }
        if trace::enabled() && snapshot_at.elapsed() >= Duration::from_secs(30) {
            trace_event!(
                Level::Debug,
                Component::Daemon,
                "RUNTIME_STATE_SNAPSHOT",
                json!({"ready":state.ready.load(Ordering::Relaxed),"connected":state.connected.load(Ordering::Relaxed),"uptime":now().saturating_sub(state.started),"active_ipc_handlers":handlers.iter().filter(|h: &&std::thread::JoinHandle<()>|!h.is_finished()).count(),"trace_active":true,"app_state":if state.ready.load(Ordering::Relaxed){"serving"}else{"starting"}})
            );
            snapshot_at = Instant::now();
        }
        match rowd_core::io_retry::poll("accept", || listener.accept()) {
            Ok((stream, _)) => {
                let state = state.clone();
                handlers.retain(|handle: &std::thread::JoinHandle<()>| !handle.is_finished());
                handlers.push(std::thread::spawn(move || {
                    if let Err(error) = state.handle(stream) {
                        trace_event!(
                            Level::Error,
                            Component::DaemonIpc,
                            "IPC_HANDLER_FAILED",
                            json!({"error":TraceError::new("daemon_ipc","handle_request",&error)})
                        );
                        state
                            .logs
                            .emit(format!("{} IPC handler failed: {error:#}", now()));
                    }
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(250))
            }
            Err(error) => {
                state.logs.emit(format!("{} IPC error: {error}", now()));
                ipc_error = Some(error);
                state.stop.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    state.stop.store(true, Ordering::Relaxed);
    for handler in handlers {
        let _ = handler.join();
    }
    let result = worker.join().map_err(|_| anyhow!("app thread panicked"))?;
    if let Err(error) = &result {
        state.logs.emit(format!("{} app error: {error:#}", now()));
    }
    state.logs.emit(format!("{} daemon stopped", now()));
    trace_event!(Level::Info, Component::Daemon, "DAEMON_STOP", json!({}));
    if let Err(error) = trace::stop("daemon_stop") {
        state
            .logs
            .emit(format!("{} TRACE_WRITER_FAILED: {error:#}", now()));
    }
    result?;
    if let Some(error) = ipc_error {
        return Err(error.into());
    }
    Ok(())
}

pub fn start(home: &Path) -> Result<()> {
    if ipc_present(home) {
        bail!("daemon already running");
    }
    let home = home_path(home)?;
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("--home")
        .arg(&home)
        .arg("daemon")
        .arg("run")
        .env("ROWD_LAUNCH_MODE", "manual")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    wait_ready(&home, &mut child)
}
fn wait_ready(home: &Path, child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(reply) = request(home, "status") {
            if reply.data.as_ref().and_then(|v| v["pid"].as_u64()) == Some(child.id() as u64) {
                return Ok(());
            }
            bail!("another daemon is running");
        }
        if let Some(status) = child.try_wait()? {
            bail!("daemon exited during startup: {status}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("daemon did not become ready")
}
pub fn stop(home: &Path) -> Result<()> {
    request(home, "stop")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if !active(home) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("daemon did not stop")
}
pub fn restart(home: &Path) -> Result<()> {
    let reply = request(home, "status")?;
    let mode = reply
        .data
        .as_ref()
        .and_then(|v| v["launch_mode"].as_str())
        .context("missing launch mode")?;
    if mode == "systemd" {
        systemctl(&["restart", "rowd.service"])?;
        Ok(())
    } else if mode == "foreground" {
        stop(home)?;
        run(home)
    } else {
        stop(home)?;
        start(home)
    }
}
fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "systemctl --user {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}
fn unit_arg(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .context("non-UTF8 path is unsupported by systemd unit")?;
    ensure!(
        !text.chars().any(char::is_control),
        "control character in systemd path"
    );
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}
pub fn unit_text(exe: &Path, home: &Path) -> Result<String> {
    ensure!(
        exe.is_absolute() && home.is_absolute(),
        "absolute paths required"
    );
    Ok(format!("[Unit]\nDescription=Rowd file synchronization daemon\nAfter=network-online.target\n\n[Service]\nType=simple\nExecStart={} --home {} daemon run\nRestart=on-failure\nRestartSec=2\nEnvironment=ROWD_LAUNCH_MODE=systemd\n\n[Install]\nWantedBy=default.target\n", unit_arg(exe)?, unit_arg(home)?))
}
fn unit_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?)
            .join(".config/systemd/user/rowd.service"),
    )
}
pub fn autostart_enable(home: &Path) -> Result<()> {
    let path = unit_path()?;
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(
        &path,
        unit_text(&std::env::current_exe()?, &home_path(home)?)?,
    )?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "rowd.service"])?;
    Ok(())
}
pub fn autostart_disable() -> Result<()> {
    systemctl(&["disable", "rowd.service"])?;
    Ok(())
}
pub fn autostart_status() -> Result<String> {
    let enabled = Command::new("systemctl")
        .args(["--user", "is-enabled", "rowd.service"])
        .output()?;
    let state = String::from_utf8_lossy(&enabled.stdout).trim().to_owned();
    let user = std::env::var("USER").unwrap_or_default();
    let linger = if user.is_empty() {
        "unknown".into()
    } else {
        Command::new("loginctl")
            .args(["show-user", &user, "-p", "Linger", "--value"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_else(|| "unknown".into())
    };
    Ok(format!(
        "Autostart: {}\nUnit: {}\nLinger: {}",
        if state.is_empty() { "disabled" } else { &state },
        unit_path()?.display(),
        linger
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowd_app::DeviceConfig;
    #[test]
    fn protocol_roundtrip_and_version() {
        let request = Request {
            version: DAEMON_IPC_VERSION,
            command: "status".into(),
            stream: None,
            args: Value::Null,
        };
        let decoded: Request =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.command, "status");
    }
    #[test]
    fn unit_runs_foreground_and_never_now() {
        let text = unit_text(Path::new("/usr/bin/rowd"), Path::new("/home/me/rowd")).unwrap();
        assert!(text.contains("daemon run"));
        assert!(!text.contains("daemon start"));
        assert!(!text.contains("--now"));
    }

    #[test]
    fn runtime_ipc_stop_single_instance_and_stale_socket() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().to_path_buf();
        App::new(&home).pair("127.0.0.1:0").unwrap();
        let mut config = DeviceConfig::load(&home).unwrap();
        config.listen = "127.0.0.1:0".into();
        let secret = config.secret.clone();
        config.save(&home).unwrap();

        let socket = socket_path(&home).unwrap();
        drop(UnixListener::bind(&socket).unwrap()); // stale socket must be recovered
        let worker_home = home.clone();
        let worker = std::thread::spawn(move || run(&worker_home));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !active(&home) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(active(&home));
        let status = request(&home, "status").unwrap();
        assert_eq!(status.data.as_ref().unwrap()["ipc_version"], 1);
        assert!(!serde_json::to_string(&status).unwrap().contains(&secret));
        assert!(run(&home).is_err());
        assert!(request_args(&home, "trace_start", json!({"output_dir":"relative"})).is_err());
        let output = home.join("diagnostics");
        request_args(&home, "trace_start", json!({"output_dir":output})).unwrap();
        assert_eq!(
            request(&home, "trace_status").unwrap().data.unwrap()["active"],
            true
        );
        request(&home, "status").unwrap();
        let mut subscription = UnixStream::connect(&socket).unwrap();
        subscription
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        send_line(
            &mut subscription,
            &Request {
                version: 1,
                command: "subscribe".into(),
                stream: Some("logs".into()),
                args: Value::Null,
            },
        )
        .unwrap();
        assert!(receive_line::<Reply>(&mut subscription).unwrap().ok);
        assert!(!receive_line::<String>(&mut subscription)
            .unwrap()
            .is_empty());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !fs::read_to_string(output.join("Latest-trace/trace-0001.jsonl"))
            .unwrap()
            .contains("IPC_STREAM_ITEM_SENT")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let (mut broken_server, broken_client) = UnixStream::pair().unwrap();
        drop(broken_client);
        {
            let _scope = trace::current_context()
                .with("request_id", "broken-write-test")
                .with("command", "status")
                .enter();
            assert!(send_response(&mut broken_server, &ok(None), Instant::now()).is_err());
        }
        request(&home, "trace_flush").unwrap();
        let latest = output.join("Latest-trace");
        assert!(fs::metadata(latest.join("trace-0001.jsonl")).unwrap().len() > 0);
        request(&home, "trace_stop").unwrap();
        let metadata: Value =
            serde_json::from_slice(&fs::read(latest.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["complete"], true);
        assert_eq!(fs::read_dir(output.join("traces")).unwrap().count(), 1);
        let trace = fs::read_to_string(latest.join("trace-0001.jsonl")).unwrap();
        assert!(!trace.contains(&secret));
        assert!(trace.contains("IPC_REQUEST_PARSED"));
        assert!(trace.contains("request_id"));
        let records: Vec<Value> = trace
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let subscriber_request = records
            .iter()
            .find(|v| v["event"] == "IPC_REQUEST_PARSED" && v["fields"]["command"] == "subscribe")
            .unwrap()["context"]["request_id"]
            .clone();
        assert_eq!(
            records
                .iter()
                .filter(|v| v["context"]["request_id"] == subscriber_request
                    && v["event"] == "IPC_RESPONSE_SENT")
                .count(),
            1
        );
        assert!(records
            .iter()
            .any(|v| v["context"]["request_id"] == subscriber_request
                && v["event"] == "IPC_STREAM_ITEM_SENT"));
        let write_failure = records
            .iter()
            .find(|v| {
                v["event"] == "IPC_WRITE_FAILED"
                    && v["context"]["request_id"] == "broken-write-test"
            })
            .unwrap();
        assert_eq!(
            write_failure["fields"]["error"]["operation"],
            "send_status_response"
        );
        assert_eq!(write_failure["fields"]["error"]["os_kind"], "BrokenPipe");
        assert_eq!(write_failure["fields"]["error"]["os_code"], 32);
        assert!(
            write_failure["fields"]["error"]["chain"]
                .as_array()
                .unwrap()
                .len()
                >= 1
        );
        assert!(!records
            .iter()
            .any(|v| v["context"]["request_id"] == "broken-write-test"
                && v["event"] == "IPC_RESPONSE_SENT"));
        let status_request = records
            .iter()
            .find(|v| v["event"] == "IPC_REQUEST_PARSED" && v["fields"]["command"] == "status")
            .unwrap()["context"]["request_id"]
            .clone();
        let request_records: Vec<&Value> = records
            .iter()
            .filter(|v| v["context"]["request_id"] == status_request)
            .collect();
        for event in [
            "IPC_ACCEPT",
            "IPC_REQUEST_RECEIVED",
            "IPC_REQUEST_PARSED",
            "IPC_RESPONSE_START",
            "IPC_RESPONSE_SENT",
            "IPC_CLIENT_CLOSED",
        ] {
            assert_eq!(
                request_records
                    .iter()
                    .filter(|v| v["event"] == event)
                    .count(),
                1,
                "{event}"
            );
        }
        assert_eq!(
            request(&home, "trace_status").unwrap().data.unwrap()["active"],
            false
        );

        let mut stream = UnixStream::connect(&socket).unwrap();
        send_line(
            &mut stream,
            &Request {
                version: 2,
                command: "status".into(),
                stream: None,
                args: Value::Null,
            },
        )
        .unwrap();
        let reply: Reply = receive_line(&mut stream).unwrap();
        assert!(!reply.ok);
        assert!(reply.error.unwrap().contains("incompatible"));

        for name in ["logs", "events"] {
            let mut stream = UnixStream::connect(&socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            send_line(
                &mut stream,
                &Request {
                    version: 1,
                    command: "subscribe".into(),
                    stream: Some(name.into()),
                    args: Value::Null,
                },
            )
            .unwrap();
            let reply: Reply = receive_line(&mut stream).unwrap();
            assert!(reply.ok);
            let line: String = receive_line(&mut stream).unwrap();
            assert!(!line.contains(&secret));
            assert!(!line.is_empty());
        }
        stop(&home).unwrap();
        worker.join().unwrap().unwrap();
        assert!(!socket.exists());
    }

    #[test]
    fn subscribers_receive_new_logs_and_events() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().to_path_buf();
        let logs = Arc::new(Bus::new(stream_path(&home, "logs").unwrap()).unwrap());
        let events = Arc::new(Bus::new(stream_path(&home, "events").unwrap()).unwrap());
        let state = Arc::new(Runtime {
            home,
            started: now(),
            mode: "foreground".into(),
            stop: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            connected: Arc::new(AtomicBool::new(false)),
            logs: logs.clone(),
            events: events.clone(),
        });
        for (name, bus, line) in [
            ("logs", logs, "new log"),
            ("events", events, "{\"kind\":\"change\"}"),
        ] {
            let (server, mut client) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let runtime = state.clone();
            let worker = std::thread::spawn(move || runtime.handle(server));
            send_line(
                &mut client,
                &Request {
                    version: 1,
                    command: "subscribe".into(),
                    stream: Some(name.into()),
                    args: Value::Null,
                },
            )
            .unwrap();
            let reply: Reply = receive_line(&mut client).unwrap();
            assert!(reply.ok);
            bus.emit(line.into());
            let received: String = receive_line(&mut client).unwrap();
            assert_eq!(received, line);
            state.stop.store(true, Ordering::Relaxed);
            worker.join().unwrap().unwrap();
            state.stop.store(false, Ordering::Relaxed);
        }
    }
}
