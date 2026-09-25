use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();

struct Session {
    side: &'static str,
    started: Instant,
    writer: BufWriter<File>,
}

fn session() -> &'static Mutex<Option<Session>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn enable(path: &Path, side: &'static str) -> Result<()> {
    let file = File::create(path)?;
    let mut guard = session().lock().unwrap();
    ENABLED.store(false, Ordering::Relaxed);
    *guard = Some(Session {
        side,
        started: Instant::now(),
        writer: BufWriter::new(file),
    });
    ENABLED.store(true, Ordering::Relaxed);
    Ok(())
}

pub fn flush() -> Result<()> {
    if let Some(session) = session().lock().unwrap().as_mut() {
        session.writer.flush()?;
    }
    Ok(())
}

pub fn disable() -> Result<()> {
    ENABLED.store(false, Ordering::Relaxed);
    if let Some(mut session) = session().lock().unwrap().take() {
        session.writer.flush()?;
    }
    Ok(())
}

#[derive(Serialize)]
struct Event<'a> {
    wall_ms: u128,
    elapsed_us: u128,
    side: &'a str,
    component: &'a str,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    share_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_us: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

pub fn event(
    component: &str,
    name: &str,
    share_id: Option<&str>,
    path: Option<&str>,
    bytes: Option<u64>,
    duration: Option<Instant>,
    detail: Option<&str>,
) {
    if !enabled() {
        return;
    }
    let mut guard = session().lock().unwrap();
    let Some(session) = guard.as_mut() else {
        return;
    };
    let file_id = share_id.zip(path).map(|(share, path)| {
        let mut hash = Sha256::new();
        hash.update(share.as_bytes());
        hash.update([0]);
        hash.update(path.as_bytes());
        hex::encode(&hash.finalize()[..8])
    });
    let value = Event {
        wall_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        elapsed_us: session.started.elapsed().as_micros(),
        side: session.side,
        component,
        event: name,
        share_id,
        file_id,
        bytes,
        duration_us: duration.map(|start| start.elapsed().as_micros()),
        detail,
    };
    if serde_json::to_writer(&mut session.writer, &value).is_ok() {
        let _ = session.writer.write_all(b"\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jsonl_order_and_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        event("sync", "before", None, None, None, None, None);
        assert!(!path.exists());
        enable(&path, "pc").unwrap();
        event(
            "sync",
            "first",
            Some("share"),
            Some("a"),
            Some(1),
            None,
            None,
        );
        event("sync", "second", Some("share"), Some("a"), None, None, None);
        disable().unwrap();
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "first");
        assert_eq!(lines[1]["event"], "second");
        assert_eq!(lines[0]["file_id"], lines[1]["file_id"]);
        assert!(lines[0]["elapsed_us"].as_u64() <= lines[1]["elapsed_us"].as_u64());
    }
}
