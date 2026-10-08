#[cfg(test)]
use crate::hash_reader;
#[cfg(test)]
use crate::storage::atomic_json;
use crate::{
    model::{
        conflict_path, reconcile, validate_cross_peer_namespace, validate_manifest, Action, Entry,
        Invitation, VERSION,
    },
    protocol::{self, Message},
    storage::{atomic_write, Snapshot, Store, VerifiedStaged},
    trace,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::Path,
    path::PathBuf,
    sync::mpsc::sync_channel,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;

#[derive(Debug)]
pub struct ScanDeferred;
impl std::fmt::Display for ScanDeferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "audit deferred")
    }
}
impl std::error::Error for ScanDeferred {}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub transferred: usize,
    pub conflicts: usize,
    #[serde(default)]
    pub shares_processed: usize,
    #[serde(default)]
    pub pending_wakes: Vec<String>,
    #[serde(default)]
    pub round_deferred: bool,
    #[serde(default)]
    pub metrics: ShareMetrics,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareMetrics {
    pub activation_ms: u128,
    pub share_queue_wait_ms: u128,
    pub configure_ms: u128,
    pub scan_ms: u128,
    pub total_ms: u128,
    pub files_enumerated: u64,
    pub manifest_entries: u64,
    pub paths_reconciled: u64,
    pub manifest_bytes: u64,
    pub manifest_ms: u128,
    pub files_hashed: u64,
    pub bytes_hashed: u64,
    pub staging_copies: u64,
    pub bytes_transferred: u64,
    pub control_messages: u64,
    pub ack_messages: u64,
    pub ack_entries: u64,
    pub full_scans: u64,
    pub first_transfer_ms: Option<u128>,
    pub first_byte_ms: Option<u128>,
    pub snapshot_ms: u128,
    pub install_ms: u128,
    pub reconcile_ms: u128,
    pub state_persist_ms: u128,
    pub state_persist_count: u64,
    pub state_bytes_written: u64,
    pub transfer_ms: u128,
    pub wait_peer_ms: u128,
    pub socket_idle_ms: u128,
    pub peak_in_flight_files: u64,
    pub peak_staged_bytes: u64,
    pub hash_stream_ms: u128,
    pub transfers_completed_before_hash_end: u64,
    pub transfers_started_before_hash_end: u64,
    pub files_received_for_deferred_install: u64,
    pub time_to_first_transfer_ms: Option<u128>,
    #[serde(flatten)]
    pub scan_stream: crate::storage::ScanStreamMetrics,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub conflicts: BTreeSet<String>,
    #[serde(default)]
    pub last_sync: Option<u64>,
    pub version: u32,
    pub pair_id: String,
    #[serde(alias = "folder_id")]
    pub share_id: String,
    pub peer_root: Option<String>,
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub blake3: bool,
    /// Old committed bases remain authoritative until their path is reconciled successfully.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub legacy_files: BTreeMap<String, String>,
    #[serde(default, skip_serializing)]
    pub base_token: Option<String>,
}
const COMMITTED_BASE_PENDING: &[u8] = b"committed base preserved";

impl State {
    pub fn load(path: &Path, pair_id: &str, share_id: &str) -> Result<Self> {
        let mut state: Self = if path.try_exists()? {
            serde_json::from_reader(File::open(path)?)?
        } else {
            Self {
                conflicts: BTreeSet::new(),
                last_sync: None,
                version: VERSION,
                pair_id: pair_id.into(),
                share_id: share_id.into(),
                peer_root: None,
                files: BTreeMap::new(),
                blake3: true,
                legacy_files: BTreeMap::new(),
                base_token: None,
            }
        };
        ensure!(
            state.version == VERSION && state.pair_id == pair_id && state.share_id == share_id,
            "state belongs to another Share/pair"
        );
        if path.with_extension("pending").try_exists()?
            && std::fs::read(path.with_extension("pending"))? != COMMITTED_BASE_PENDING
        {
            // Older versions wrote the candidate directly into state.json.
            state.files.clear();
            state.legacy_files.clear();
            state.last_sync = None;
            session_tokens().lock().unwrap().remove(path);
            retry_paths().lock().unwrap().remove(path);
        }
        if !state.blake3 {
            ensure!(state.legacy_files.is_empty(), "ambiguous legacy base");
            state.legacy_files = std::mem::take(&mut state.files);
            state.blake3 = true;
            state.base_token = None;
            session_tokens().lock().unwrap().remove(path);
        }
        Ok(state)
    }
}

fn session_tokens() -> &'static Mutex<BTreeMap<PathBuf, String>> {
    static TOKENS: OnceLock<Mutex<BTreeMap<PathBuf, String>>> = OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn retry_paths() -> &'static Mutex<BTreeMap<PathBuf, BTreeSet<String>>> {
    static PATHS: OnceLock<Mutex<BTreeMap<PathBuf, BTreeSet<String>>>> = OnceLock::new();
    PATHS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn persist_state(
    path: &Path,
    state: &State,
    metrics: &mut ShareMetrics,
    file: Option<&str>,
) -> Result<()> {
    let started = Instant::now();
    crate::trace_legacy_event!(
        "sync",
        "state_persist_start",
        Some(&state.share_id),
        file,
        None,
        None,
        None,
    );
    let bytes = serde_json::to_vec(state)?;
    atomic_write(path, &bytes).map_err(|error| {
        crate::trace_event!(trace::Level::Error,trace::Component::StateStore,"STATE_PERSIST_FAILED",serde_json::json!({"error":trace::TraceError::new("state_store","atomic_write_state",&error)}));error.context("persist Share state")
    })?;
    metrics.state_persist_ms += started.elapsed().as_millis();
    metrics.state_persist_count += 1;
    metrics.state_bytes_written += bytes.len() as u64;
    crate::trace_legacy_event!(
        "sync",
        "state_persist_end",
        Some(&state.share_id),
        file,
        Some(bytes.len() as u64),
        Some(started),
        None,
    );
    Ok(())
}

fn receive_blob(io: &mut impl Read, entry: &Entry, directory: Option<&Path>) -> Result<Snapshot> {
    let mut temp = match directory {
        Some(directory) => NamedTempFile::new_in(directory),
        None => NamedTempFile::new(),
    }
    .context(protocol::IncompleteFrame)?;
    anyhow::ensure!(entry.size <= crate::model::MAX_FILE, "file too large");
    let (hash, size) =
        crate::copy_and_hash(io.take(entry.size), &mut temp).context(protocol::IncompleteFrame)?;
    ensure!(
        size == entry.size,
        "truncated transfer: {size}/{}",
        entry.size
    );
    VerifiedStaged::from_digest(temp, entry, &hash, size)
}
fn remote_snapshot(
    share_id: &str,
    io: &mut (impl Read + Write),
    path: &str,
    entry: &Entry,
    first_byte_ms: &mut Option<u128>,
    round_started: Instant,
    directory: Option<&Path>,
) -> Result<Snapshot> {
    let _transfer = trace::transfer_context(share_id, path).enter();
    crate::trace_legacy_event!(
        "sync",
        "get_sent",
        Some(share_id),
        Some(path),
        Some(entry.size),
        None,
        None,
    );
    protocol::send_for(
        io,
        share_id,
        Message::Get {
            path: path.into(),
            entry: entry.clone(),
        },
    )?;
    let Message::Blob { entry: actual } = protocol::receive_for(io, share_id)? else {
        anyhow::bail!("expected blob")
    };
    if actual != *entry {
        return Err(anyhow::anyhow!("STALE_SOURCE").context(protocol::IncompleteFrame));
    }
    first_byte_ms.get_or_insert_with(|| round_started.elapsed().as_millis());
    crate::trace_legacy_event!(
        "sync",
        "blob_receive_start",
        Some(share_id),
        Some(path),
        Some(entry.size),
        None,
        None,
    );
    let started = Instant::now();
    let blob = receive_blob(io, entry, directory).map_err(|e| {
        trace::record_error(
            trace::Component::Transfer,
            "TRANSFER_FAILED",
            "transfer",
            "receive_blob",
            e,
        )
    })?;
    crate::trace_legacy_event!(
        "sync",
        "blob_receive_end",
        Some(share_id),
        Some(path),
        Some(entry.size),
        Some(started),
        None,
    );
    Ok(blob)
}
fn remote_install(
    share_id: &str,
    io: &mut (impl Read + Write),
    path: &str,
    expected: Option<&str>,
    entry: &Entry,
    staged: &Snapshot,
) -> Result<()> {
    let _transfer = trace::transfer_context(share_id, path).enter();
    ensure!(
        staged.entry() == entry,
        "staged entry does not match transfer"
    );
    crate::trace_legacy_event!(
        "sync",
        "put_sent",
        Some(share_id),
        Some(path),
        Some(entry.size),
        None,
        None,
    );
    let mut source = File::open(staged.path())?;
    protocol::send_for(
        io,
        share_id,
        Message::Put {
            path: path.into(),
            expected: expected.map(String::from),
            entry: entry.clone(),
        },
    )?;
    crate::trace_legacy_event!(
        "sync",
        "blob_send_start",
        Some(share_id),
        Some(path),
        Some(entry.size),
        None,
        None,
    );
    let sending = Instant::now();
    protocol::copy_exact(&mut source, io, entry.size)?;
    crate::trace_legacy_event!(
        "sync",
        "blob_send_end",
        Some(share_id),
        Some(path),
        Some(entry.size),
        Some(sending),
        None,
    );
    protocol::send_for(io, share_id, Message::PutBatchEnd)?;
    ensure!(
        matches!(protocol::receive_for(io, share_id)?, Message::Accept),
        "expected file confirmation"
    );
    crate::trace_legacy_event!(
        "sync",
        "accept_received",
        Some(share_id),
        Some(path),
        None,
        None,
        None,
    );
    Ok(())
}

const MAX_IN_FLIGHT_FILES: usize = 4;
const MAX_STAGED_BYTES: u64 = 8 * 1024 * 1024;

struct PendingPut {
    path: String,
    entry: Entry,
    staged: Snapshot,
}

struct StagedPut {
    sequence: u64,
    path: String,
    entry: Entry,
}

struct ReceivedStage {
    job: IncomingPut,
    staged: Snapshot,
}

/// A receipt proves a private, verified copy only. InstallStaged is the separate
/// operation that performs the target precondition check and returns its ACK.
fn remote_stage(
    io: &mut (impl Read + Write),
    share_id: &str,
    scan_id: &str,
    sequence: u64,
    path: &str,
    expected: Option<&str>,
    entry: &Entry,
    staged: &Snapshot,
) -> Result<()> {
    let mut source = File::open(staged.path())?;
    protocol::send_for(
        io,
        share_id,
        Message::StagePut {
            scan_id: scan_id.into(),
            sequence,
            path: path.into(),
            entry: entry.clone(),
            expected: expected.map(str::to_owned),
        },
    )?;
    // There is deliberately no control callback between the header and its bytes.
    protocol::copy_exact(&mut source, io, entry.size)?;
    ensure!(
        matches!(protocol::receive_for(io, share_id)?, Message::StageReceived {
        scan_id: id, sequence: actual,
    } if id == scan_id && actual == sequence),
        "misaligned staging receipt"
    );
    Ok(())
}

fn install_remote_stages(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    scan_id: &str,
    stages: &mut Vec<StagedPut>,
    report: &mut Report,
) -> Result<()> {
    for staged in stages.drain(..) {
        protocol::send_for(
            io,
            &state.share_id,
            Message::InstallStaged {
                scan_id: scan_id.into(),
                sequence: staged.sequence,
            },
        )?;
        let waiting = Instant::now();
        ensure!(
            matches!(protocol::receive_for(io, &state.share_id)?, Message::Accept),
            "expected staged install ACK"
        );
        report.metrics.wait_peer_ms += waiting.elapsed().as_millis();
        state
            .files
            .insert(staged.path.clone(), staged.entry.hash.clone());
        store.acknowledge(&staged.path, &staged.entry)?;
        report.transferred += 1;
        report.metrics.control_messages += 2;
    }
    Ok(())
}

struct PendingGet {
    path: String,
    entry: Entry,
    expected: Option<String>,
}

struct IncomingPut {
    path: String,
    entry: Entry,
    expected: Option<String>,
}

fn receive_put_batch(
    io: &mut (impl Read + Write + Send),
    store: &mut impl Store,
    share_id: &str,
    first: IncomingPut,
) -> Result<Vec<String>> {
    let staging_directory = store.staging_directory();
    let (sender, receiver) = sync_channel::<(IncomingPut, Snapshot)>(1);
    let mut installed = Vec::new();
    let result = std::thread::scope(|scope| -> Result<()> {
        let reader_io = &mut *io;
        let trace_context = trace::current_context();
        let reader = scope.spawn(move || -> Result<()> {
            let _trace_context = trace_context.enter();
            let mut next = first;
            let mut count = 0;
            let mut bytes = 0u64;
            loop {
                crate::model::validate_path(&next.path)?;
                crate::model::validate_hash(&next.entry.hash)?;
                ensure!(next.entry.size <= crate::model::MAX_FILE, "file too large");
                ensure!(count < MAX_IN_FLIGHT_FILES, "too many staged files");
                ensure!(
                    count == 0 || bytes.saturating_add(next.entry.size) <= MAX_STAGED_BYTES,
                    "too many staged bytes"
                );
                let _transfer = trace::transfer_context(share_id, &next.path).enter();
                count += 1;
                bytes = bytes.saturating_add(next.entry.size);
                crate::trace_legacy_event!(
                    "sync",
                    "blob_receive_start",
                    Some(share_id),
                    Some(&next.path),
                    Some(next.entry.size),
                    None,
                    None,
                );
                let receiving = Instant::now();
                let staged = receive_blob(reader_io, &next.entry, staging_directory.as_deref())
                    .map_err(|e| {
                        trace::record_error(
                            trace::Component::Transfer,
                            "TRANSFER_FAILED",
                            "transfer",
                            "receive_put_blob",
                            e,
                        )
                    })?;
                crate::trace_legacy_event!(
                    "sync",
                    "blob_receive_end",
                    Some(share_id),
                    Some(&next.path),
                    Some(next.entry.size),
                    Some(receiving),
                    None,
                );
                sender
                    .send((next, staged))
                    .map_err(|_| anyhow::anyhow!("install worker stopped"))?;
                next = match protocol::receive_for(reader_io, share_id)? {
                    Message::Put {
                        path,
                        entry,
                        expected,
                    } => IncomingPut {
                        path,
                        entry,
                        expected,
                    },
                    Message::PutBatchEnd => break,
                    _ => anyhow::bail!("expected Put or PutBatchEnd"),
                };
            }
            Ok(())
        });
        let mut install_error = None;
        for (job, staged) in receiver {
            let _transfer = trace::transfer_context(share_id, &job.path).enter();
            if install_error.is_some() {
                continue;
            }
            crate::trace_legacy_event!(
                "sync",
                "install_start",
                Some(share_id),
                Some(&job.path),
                Some(job.entry.size),
                None,
                None,
            );
            let installing = Instant::now();
            match store.install_received(&job.path, job.expected.as_deref(), &job.entry, staged) {
                Ok(()) => {
                    crate::trace_legacy_event!(
                        "sync",
                        "install_end",
                        Some(share_id),
                        Some(&job.path),
                        Some(job.entry.size),
                        Some(installing),
                        None,
                    );
                    installed.push(job.path);
                }
                Err(error) => {
                    install_error = Some(trace::record_error(
                        trace::Component::Transfer,
                        "TRANSFER_FAILED",
                        "filesystem",
                        "install_received_put",
                        protocol::LocalOperation::attach(error, "install", Some(&job.path)),
                    ))
                }
            }
        }
        let read_result = reader
            .join()
            .map_err(|_| anyhow::anyhow!("network receiver panicked"))?;
        read_result.context(protocol::IncompleteFrame)?;
        if let Some(error) = install_error {
            return Err(error);
        }
        Ok(())
    });
    if let Err(error) = &result {
        if error.is::<protocol::LocalOperation>() {
            protocol::send_share_error(
                io,
                error,
                if cfg!(target_os = "android") {
                    "android"
                } else {
                    "responder"
                },
                Some(share_id),
                "install",
                None,
                "filesystem",
            );
        }
    }
    result?;
    Ok(installed)
}

fn drain_puts(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    _state_path: &Path,
    pending: &mut Vec<PendingPut>,
    report: &mut Report,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    protocol::send_for(io, &state.share_id, Message::PutBatchEnd)?;
    for job in pending.drain(..) {
        let _transfer = trace::transfer_context(&state.share_id, &job.path).enter();
        let waiting = Instant::now();
        crate::trace_legacy_event!(
            "sync",
            "peer_wait_start",
            Some(&state.share_id),
            Some(&job.path),
            None,
            None,
            None,
        );
        ensure!(
            matches!(protocol::receive_for(io, &state.share_id)?, Message::Accept),
            "expected file confirmation"
        );
        crate::trace_legacy_event!(
            "sync",
            "accept_received",
            Some(&state.share_id),
            Some(&job.path),
            None,
            None,
            None,
        );
        report.metrics.wait_peer_ms += waiting.elapsed().as_millis();
        crate::trace_legacy_event!(
            "sync",
            "peer_wait_end",
            Some(&state.share_id),
            Some(&job.path),
            None,
            Some(waiting),
            None,
        );
        state.files.insert(job.path.clone(), job.entry.hash.clone());
        store.acknowledge(&job.path, &job.entry)?;
        report.metrics.bytes_transferred += job.entry.size;
        report.metrics.control_messages += 2;
        report.transferred += 1;
        drop(job.staged);
    }
    Ok(())
}

fn drain_gets(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    _state_path: &Path,
    pending: &mut Vec<PendingGet>,
    ack_batch: &mut crate::model::Manifest,
    report: &mut Report,
    round_started: Instant,
) -> Result<()> {
    let mut staged = Vec::with_capacity(pending.len());
    for job in pending.drain(..) {
        let _transfer = trace::transfer_context(&state.share_id, &job.path).enter();
        let waiting = Instant::now();
        crate::trace_legacy_event!(
            "sync",
            "peer_wait_start",
            Some(&state.share_id),
            Some(&job.path),
            None,
            None,
            None,
        );
        let Message::Blob { entry: actual } = protocol::receive_for(io, &state.share_id)? else {
            anyhow::bail!("expected blob")
        };
        crate::trace_legacy_event!(
            "sync",
            "peer_wait_end",
            Some(&state.share_id),
            Some(&job.path),
            None,
            Some(waiting),
            None,
        );
        if actual != job.entry {
            return Err(anyhow::anyhow!("STALE_SOURCE").context(protocol::IncompleteFrame));
        }
        report.metrics.wait_peer_ms += waiting.elapsed().as_millis();
        report
            .metrics
            .first_byte_ms
            .get_or_insert_with(|| round_started.elapsed().as_millis());
        let transferring = Instant::now();
        crate::trace_legacy_event!(
            "sync",
            "blob_receive_start",
            Some(&state.share_id),
            Some(&job.path),
            Some(job.entry.size),
            None,
            None,
        );
        let snapshot =
            receive_blob(io, &job.entry, store.staging_directory().as_deref()).map_err(|e| {
                trace::record_error(
                    trace::Component::Transfer,
                    "TRANSFER_FAILED",
                    "transfer",
                    "receive_get_blob",
                    e,
                )
            })?;
        crate::trace_legacy_event!(
            "sync",
            "blob_receive_end",
            Some(&state.share_id),
            Some(&job.path),
            Some(job.entry.size),
            Some(transferring),
            None,
        );
        report.metrics.transfer_ms += transferring.elapsed().as_millis();
        staged.push((job, snapshot));
    }
    for (job, snapshot) in staged {
        let _transfer = trace::transfer_context(&state.share_id, &job.path).enter();
        let installing = Instant::now();
        crate::trace_legacy_event!(
            "sync",
            "install_start",
            Some(&state.share_id),
            Some(&job.path),
            Some(job.entry.size),
            None,
            None,
        );
        store
            .install_received(&job.path, job.expected.as_deref(), &job.entry, snapshot)
            .map_err(|e| {
                trace::record_error(
                    trace::Component::Transfer,
                    "TRANSFER_FAILED",
                    "filesystem",
                    "install_received_file",
                    protocol::LocalOperation::attach(e, "install", Some(&job.path)),
                )
            })?;
        crate::trace_legacy_event!(
            "sync",
            "install_end",
            Some(&state.share_id),
            Some(&job.path),
            Some(job.entry.size),
            Some(installing),
            None,
        );
        report.metrics.install_ms += installing.elapsed().as_millis();
        ack_batch.insert(job.path.clone(), job.entry.clone());
        if ack_batch.len() == protocol::MANIFEST_CHUNK_FILES {
            flush_ack_batch(io, &state.share_id, ack_batch, &mut report.metrics)?;
        }
        state.files.insert(job.path.clone(), job.entry.hash);
        report.metrics.bytes_transferred += job.entry.size;
        report.metrics.control_messages += 2;
        report.transferred += 1;
    }
    Ok(())
}

fn flush_ack_batch(
    io: &mut impl Write,
    share_id: &str,
    batch: &mut crate::model::Manifest,
    metrics: &mut ShareMetrics,
) -> Result<()> {
    if !batch.is_empty() {
        crate::trace_legacy_event!(
            "sync",
            "ack_sent",
            Some(share_id),
            None,
            Some(batch.len() as u64),
            None,
            None,
        );
        metrics.ack_entries += batch.len() as u64;
        let acknowledged = if trace::enabled() {
            batch.keys().cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        protocol::send_for(
            io,
            share_id,
            Message::AckBatch {
                entries: std::mem::take(batch),
            },
        )?;
        for path in acknowledged {
            trace::acknowledge_file(share_id, &path, "ack_batch_sent");
        }
        metrics.ack_messages += 1;
        metrics.control_messages += 1;
    }
    Ok(())
}

pub fn coordinate(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    state_path: &Path,
) -> Result<Report> {
    coordinate_mode(
        io,
        store,
        state,
        state_path,
        crate::config::SyncMode::Bidirectional,
    )
}

pub fn coordinate_mode(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    state_path: &Path,
    mode: crate::config::SyncMode,
) -> Result<Report> {
    coordinate_with_progress(io, store, state, state_path, mode, None, |_, _, _| {})
}

pub fn coordinate_with_progress(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    state_path: &Path,
    mode: crate::config::SyncMode,
    remap: Option<crate::config::RemapPolicy>,
    progress: impl FnMut(&str, usize, usize),
) -> Result<Report> {
    coordinate_with_progress_and_audit_control(
        io,
        store,
        state,
        state_path,
        mode,
        remap,
        progress,
        |_| None,
    )
}

#[derive(Clone, Copy)]
pub enum AuditWait {
    Start,
    Tick,
    End,
}

fn receive_scan_gate(
    io: &mut (impl Read + Write),
    share_id: &str,
    audit_control: &mut impl FnMut(AuditWait) -> Option<Vec<String>>,
) -> Result<Option<Vec<String>>> {
    receive_scan_gate_with_clock(
        io,
        share_id,
        audit_control,
        || Instant::now(),
        Duration::from_secs(90),
    )
}

fn receive_scan_gate_with_clock(
    io: &mut (impl Read + Write),
    share_id: &str,
    audit_control: &mut impl FnMut(AuditWait) -> Option<Vec<String>>,
    clock: impl Fn() -> Instant,
    inactivity: Duration,
) -> Result<Option<Vec<String>>> {
    struct Waiting<'a, T, F> {
        io: &'a mut T,
        share_id: &'a str,
        control: &'a mut F,
        sent: Vec<String>,
        last_alive: Instant,
        clock: &'a dyn Fn() -> Instant,
        inactivity: Duration,
    }
    impl<T: Read + Write, F: FnMut(AuditWait) -> Option<Vec<String>>> Read for Waiting<'_, T, F> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            loop {
                if (self.clock)().duration_since(self.last_alive) >= self.inactivity {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "scan liveness timeout",
                    ));
                }
                match crate::io_retry::poll("scan_gate_read", || self.io.read(buf)) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        if (self.clock)().duration_since(self.last_alive) >= self.inactivity {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "scan liveness timeout",
                            ));
                        }
                        if self.sent.is_empty() {
                            if let Some(shares) =
                                (self.control)(AuditWait::Tick).filter(|s| !s.is_empty())
                            {
                                protocol::send_for(
                                    self.io,
                                    self.share_id,
                                    Message::AuditPreempt {
                                        shares: shares.clone(),
                                    },
                                )
                                .map_err(std::io::Error::other)?;
                                self.sent = shares;
                            }
                        }
                    }
                    result => return result,
                }
            }
        }
    }
    let mut waiting = Waiting {
        io,
        share_id,
        control: audit_control,
        sent: Vec::new(),
        last_alive: clock(),
        clock: &clock,
        inactivity,
    };
    loop {
        match protocol::receive_for(&mut waiting, share_id)? {
            Message::ScanAlive => {
                waiting.last_alive = clock();
                continue;
            }
            Message::ScanDeferred => return Ok(Some(waiting.sent)),
            Message::ScanReady => {
                if waiting.sent.is_empty() {
                    if let Some(shares) =
                        (waiting.control)(AuditWait::Tick).filter(|s| !s.is_empty())
                    {
                        protocol::send_for(
                            waiting.io,
                            share_id,
                            Message::AuditPreempt {
                                shares: shares.clone(),
                            },
                        )?;
                        waiting.sent = shares;
                    }
                }
                if !waiting.sent.is_empty() {
                    ensure!(
                        matches!(
                            protocol::receive_for(&mut waiting, share_id)?,
                            Message::ScanDeferred
                        ),
                        "expected deferred scan"
                    );
                    return Ok(Some(waiting.sent));
                } else {
                    protocol::send_for(waiting.io, share_id, Message::ScanContinue)?;
                    return Ok(None);
                }
            }
            _ => anyhow::bail!("expected scan readiness"),
        }
    }
}

pub fn coordinate_with_progress_and_audit_control(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    state_path: &Path,
    mode: crate::config::SyncMode,
    remap: Option<crate::config::RemapPolicy>,
    mut progress: impl FnMut(&str, usize, usize),
    mut audit_control: impl FnMut(AuditWait) -> Option<Vec<String>>,
) -> Result<Report> {
    let mut candidate = state.clone();
    let result = coordinate_candidate(
        io,
        store,
        &mut candidate,
        state_path,
        mode,
        remap,
        &mut progress,
        &mut audit_control,
    );
    if let Err(error) = &result {
        crate::trace_event!(
            trace::Level::Error,
            trace::Component::Transfer,
            "TRANSFER_FAILED",
            serde_json::json!({"error":trace::TraceError::new("transfer","coordinate_share",error)})
        );
    }
    // Clean per-round physical scan evidence on every outcome, without promoting base.
    // Cache persistence is best effort after the protocol's own result is settled.
    let _ = store.discard_scan();
    if result.as_ref().is_ok_and(|report| !report.round_deferred) {
        *state = candidate;
    } else {
        let _ = std::fs::remove_file(state_path.with_extension("next"));
    }
    result
}

fn coordinate_candidate(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    state_path: &Path,
    mode: crate::config::SyncMode,
    remap: Option<crate::config::RemapPolicy>,
    mut progress: impl FnMut(&str, usize, usize),
    mut audit_control: impl FnMut(AuditWait) -> Option<Vec<String>>,
) -> Result<Report> {
    let started = Instant::now();
    let share_id = state.share_id.clone();
    let pending_state = state_path.with_extension("pending");
    crate::trace_legacy_event!(
        "sync",
        "share_sync_start",
        Some(&share_id),
        None,
        None,
        None,
        None,
    );
    let store_metrics_before = store.metrics();
    let previous_token = session_tokens().lock().unwrap().get(state_path).cloned();
    let session_matches = previous_token.is_some();
    state.base_token = None;
    let manifest_started = Instant::now();
    crate::trace_legacy_event!(
        "sync",
        "manifest_start",
        Some(&share_id),
        None,
        None,
        None,
        None,
    );
    crate::trace_legacy_event!(
        "sync",
        "scan_start",
        Some(&share_id),
        None,
        None,
        None,
        None,
    );
    let mut delta = None;
    let mut fallback_reason = if remap.is_some() {
        Some("scheduled_audit")
    } else if state.last_sync.is_none() {
        Some("no_last_sync")
    } else if !session_matches {
        Some("no_session_token")
    } else {
        None
    };
    if remap.is_none() && state.last_sync.is_some() && session_matches {
        let retry = retry_paths()
            .lock()
            .unwrap()
            .get(state_path)
            .cloned()
            .unwrap_or_default();
        match store.delta_paths()?.map(|mut dirty| {
            dirty.extend(retry);
            dirty
        }) {
            None => fallback_reason = Some("cache_untrusted"),
            Some(dirty) if dirty.len() > 1024 => fallback_reason = Some("too_many_dirty_paths"),
            Some(dirty)
                if dirty
                    .iter()
                    .any(|p| crate::model::validate_path(p).is_err()) =>
            {
                fallback_reason = Some("invalid_dirty_paths")
            }
            Some(dirty) => {
                retry_paths()
                    .lock()
                    .unwrap()
                    .insert(state_path.to_path_buf(), dirty.clone());
                let request = Message::Scoped {
                    share_id: share_id.clone(),
                    message: Box::new(Message::DeltaScan {
                        base_token: previous_token.unwrap(),
                        paths: dirty,
                    }),
                };
                let request_bytes = serde_json::to_vec(&request)?.len() as u64 + 4;
                protocol::send(io, &request)?;
                let remote_started = Instant::now();
                crate::trace_legacy_event!(
                    "sync",
                    "remote_scan_wait_start",
                    Some(&share_id),
                    None,
                    None,
                    None,
                    None,
                );
                audit_control(AuditWait::Start);
                let gate = receive_scan_gate(io, &share_id, &mut audit_control);
                audit_control(AuditWait::End);
                if let Some(shares) = gate? {
                    return Ok(Report {
                        pending_wakes: shares,
                        round_deferred: true,
                        ..Default::default()
                    });
                }
                let response = protocol::receive_for(io, &share_id)?;
                crate::trace_legacy_event!(
                    "sync",
                    "remote_scan_wait_end",
                    Some(&share_id),
                    None,
                    None,
                    Some(remote_started),
                    None,
                );
                let bytes = serde_json::to_vec(&Message::Scoped {
                    share_id: share_id.clone(),
                    message: Box::new(match &response {
                        Message::DeltaManifest {
                            paths,
                            files,
                            metrics,
                        } => Message::DeltaManifest {
                            paths: paths.clone(),
                            files: files.clone(),
                            metrics: *metrics,
                        },
                        _ => Message::NeedFullScan,
                    }),
                })?
                .len() as u64
                    + 4
                    + request_bytes;
                if let Message::DeltaManifest {
                    paths,
                    files,
                    metrics,
                } = response
                {
                    if paths.len() <= 1024
                        && paths.iter().all(|p| crate::model::validate_path(p).is_ok())
                        && files.keys().all(|p| paths.contains(p))
                        && validate_manifest(&files).is_ok()
                    {
                        fallback_reason = Some("scan_paths_failed");
                        let local_started = Instant::now();
                        crate::trace_legacy_event!(
                            "sync",
                            "local_scan_start",
                            Some(&share_id),
                            None,
                            None,
                            None,
                            None,
                        );
                        let scanned = store.scan_paths(&paths).ok().flatten();
                        crate::trace_legacy_event!(
                            "sync",
                            "local_scan_end",
                            Some(&share_id),
                            None,
                            None,
                            Some(local_started),
                            None,
                        );
                        if let Some(pc) = scanned {
                            let missing_known = paths.iter().any(|p| {
                                state.files.contains_key(p)
                                    && (!pc.contains_key(p) || !files.contains_key(p))
                            });
                            if !missing_known {
                                retry_paths()
                                    .lock()
                                    .unwrap()
                                    .insert(state_path.to_path_buf(), paths.clone());
                                delta = Some((pc, files, paths, metrics, bytes));
                                fallback_reason = None;
                            } else {
                                fallback_reason = Some("missing_known_path");
                            }
                        }
                    } else {
                        fallback_reason = Some("invalid_delta_manifest");
                    }
                } else {
                    fallback_reason = Some("peer_need_full_scan");
                }
            }
        }
    }
    let used_delta = delta.is_some();
    let mut hash_stream: Option<protocol::ScanStream> = None;
    let mut namespace_ms = 0;
    let mut hash_started = None;
    let mut scan_binding = None;
    let (pc, mut android, paths, mut peer_scan_metrics, manifest_bytes) = if let Some(delta) = delta
    {
        delta
    } else {
        protocol::send_for(io, &share_id, Message::Scan)?;
        audit_control(AuditWait::Start);
        let gate = receive_scan_gate(io, &share_id, &mut audit_control);
        audit_control(AuditWait::End);
        if let Some(shares) = gate? {
            return Ok(Report {
                pending_wakes: shares,
                round_deferred: true,
                ..Report::default()
            });
        }
        let stream = match protocol::ScanStream::receive_namespace(io, &share_id) {
            Ok(stream) => stream,
            Err(error) if error.is::<ScanDeferred>() => {
                return Ok(Report {
                    round_deferred: true,
                    ..Default::default()
                })
            }
            Err(error) => return Err(error),
        };
        let local_binding = store.scan_binding()?;
        let local_namespace = store.stream_namespace(io)?;
        let pc = if local_namespace.is_none() {
            store.scan()?
        } else {
            crate::model::Manifest::new()
        };
        let local_namespace =
            local_namespace.unwrap_or_else(|| crate::model::manifest_namespace(&pc));
        crate::model::validate_cross_namespace(&local_namespace, &stream.namespace)?;
        // The entire structural union, including committed paths, is checked before
        // starting hashing or scheduling any transfer.
        let paths: BTreeSet<String> = local_namespace
            .iter()
            .chain(stream.namespace.iter())
            .filter(|(_, e)| !e.directory)
            .map(|(p, _)| p.clone())
            .chain(state.files.keys().cloned())
            .chain(state.legacy_files.keys().cloned())
            .collect();
        let mut folded = BTreeSet::new();
        for path in &paths {
            ensure!(folded.insert(path.to_lowercase()), "case collision: {path}");
        }
        namespace_ms = u64::try_from(started.elapsed().as_millis())
            .context("namespace duration exceeds wire milliseconds range")?;
        store.require_full_scan()?;
        let pc = if pc.is_empty() { store.scan()? } else { pc };
        scan_binding = Some(local_binding.clone());
        validate_manifest(&pc)?;
        ensure!(
            store.scan_binding()? == local_binding,
            "scan binding changed"
        );
        ensure!(
            pc.len() == local_namespace.values().filter(|e| !e.directory).count()
                && pc.iter().all(|(p, e)| local_namespace
                    .get(p)
                    .is_some_and(|n| !n.directory && n.size == e.size)),
            "STALE_SOURCE: PC namespace changed during scan"
        );
        store.validate_scan_snapshot()?;
        let hints: Vec<String> = stream
            .namespace
            .iter()
            .filter(|(p, e)| {
                !e.directory
                    && !store.excluded(p)
                    && mode != crate::config::SyncMode::ToAndroid
                    && (pc.get(*p).is_none()
                        || pc
                            .get(*p)
                            .is_some_and(|e| state.files.get(*p) == Some(&e.hash)))
            })
            .map(|(p, _)| p.clone())
            .collect();
        for paths in hints.chunks(protocol::SCAN_STREAM_CHUNK_FILES) {
            protocol::send_for(
                io,
                &share_id,
                Message::HashStageHint {
                    scan_id: stream.scan_id.clone(),
                    paths: paths.iter().cloned().collect(),
                },
            )?;
        }
        protocol::send_for(
            io,
            &share_id,
            Message::HashStreamBegin {
                scan_id: stream.scan_id.clone(),
                binding: stream.binding.clone(),
            },
        )?;
        hash_started = Some(Instant::now());
        crate::trace_event!(
            trace::Level::Info,
            trace::Component::Scanner,
            "HASH_STREAM_BEGIN",
            serde_json::json!({"scan_id":stream.scan_id})
        );
        crate::trace_event!(
            trace::Level::Info,
            trace::Component::Scanner,
            "NAMESPACE_END",
            serde_json::json!({"namespace_ms":namespace_ms,"entries":stream.namespace.len()})
        );
        hash_stream = Some(stream);
        (
            pc,
            crate::model::Manifest::new(),
            paths,
            crate::storage::StoreMetrics::default(),
            0,
        )
    };
    if !used_delta {
        let local_audits = store
            .metrics()
            .full_scans
            .saturating_sub(store_metrics_before.full_scans);
        crate::trace_legacy_event!(
            "sync",
            "manifest_source",
            Some(&share_id),
            None,
            None,
            None,
            Some(if local_audits + peer_scan_metrics.full_scans > 0 {
                "physical_audit"
            } else {
                "full_manifest"
            }),
        );
    }
    let android_count = hash_stream.as_ref().map_or(android.len(), |s| {
        s.namespace.values().filter(|e| !e.directory).count()
    });
    crate::trace_legacy_event!(
        "sync",
        "scan_end",
        Some(&share_id),
        None,
        None,
        Some(manifest_started),
        if used_delta {
            Some("delta")
        } else {
            fallback_reason
        },
    );
    crate::trace_legacy_event!(
        "sync",
        "manifest_end",
        Some(&share_id),
        None,
        Some(manifest_bytes),
        Some(manifest_started),
        None,
    );
    let received_chunks = if used_delta {
        0
    } else {
        android.len().div_ceil(protocol::MANIFEST_CHUNK_FILES)
    };
    android = android
        .into_iter()
        .filter(|(p, _)| !store.excluded(p))
        .collect();
    validate_cross_peer_namespace(&pc, &android)?;
    if used_delta {
        for path in pc.keys().chain(android.keys()) {
            if state.files.contains_key(path) {
                continue;
            }
            for (index, _) in path.match_indices('/') {
                ensure!(
                    !state.files.contains_key(&path[..index]),
                    "file/directory collision: {path}"
                );
            }
            ensure!(
                !state
                    .files
                    .range(format!("{path}/")..)
                    .next()
                    .is_some_and(|(p, _)| p.starts_with(&format!("{path}/"))),
                "file/directory collision: {path}"
            );
            ensure!(
                !state
                    .files
                    .keys()
                    .any(|old| old != path && old.to_lowercase() == path.to_lowercase()),
                "case collision: {path}"
            );
        }
    }
    if !used_delta && hash_stream.is_none() {
        state
            .conflicts
            .retain(|path| pc.contains_key(path) || android.contains_key(path));
    }
    // Cross-device case collisions would alias on some SAF providers. Stop before any write.
    let mut folded = BTreeSet::new();
    for path in &paths {
        ensure!(folded.insert(path.to_lowercase()), "case collision: {path}");
    }
    let mut report = Report::default();
    report.metrics.scan_ms = started.elapsed().as_millis();
    let scan_metrics = store.metrics();
    report.metrics.full_scans = peer_scan_metrics.full_scans
        + scan_metrics
            .full_scans
            .saturating_sub(store_metrics_before.full_scans);
    report.metrics.files_enumerated = peer_scan_metrics.files_enumerated
        + scan_metrics
            .files_enumerated
            .saturating_sub(store_metrics_before.files_enumerated);
    report.metrics.manifest_entries = android_count as u64;
    report.metrics.paths_reconciled = paths.len() as u64;
    report.metrics.manifest_bytes = manifest_bytes;
    report.metrics.manifest_ms = manifest_started.elapsed().as_millis();
    report.metrics.control_messages = 3 + received_chunks as u64;
    let mut ack_batch = crate::model::Manifest::new();
    let mut pending_puts = Vec::new();
    let mut pending_gets = Vec::new();
    let mut staged_puts = Vec::new();
    let mut deferred_stage_bytes = 0u64;
    let mut put_bytes = 0u64;
    let mut get_bytes = 0u64;
    let total = paths.len();
    crate::trace_legacy_event!(
        "sync",
        "reconcile_start",
        Some(&share_id),
        None,
        Some(total as u64),
        None,
        None,
    );
    let mut work = std::collections::VecDeque::new();
    let mut deferred = BTreeSet::new();
    let mut processed = BTreeSet::new();
    if let Some(current) = &hash_stream {
        // Absence is authoritative now: PC-only paths need no Android hash.
        work.extend(
            paths
                .iter()
                .filter(|p| !current.namespace.contains_key(*p))
                .cloned(),
        );
    } else {
        work.extend(paths.iter().cloned());
    }
    let mut index = 0;
    let mut last_chunk_paths = BTreeSet::new();
    let mut requested_preempt = None;
    loop {
        if hash_stream.as_ref().is_some_and(|s| !s.ended) && requested_preempt.is_none() {
            requested_preempt = audit_control(AuditWait::Tick).filter(|s| !s.is_empty());
            if requested_preempt.is_some() {
                crate::trace_event!(
                    trace::Level::Info,
                    trace::Component::Scanner,
                    "STREAM_PREEMPT_REQUESTED",
                    serde_json::json!({"phase":"transfer_boundary","frame_boundary":true})
                );
                work.clear();
            }
        }
        if work.is_empty() {
            // Finish all outstanding Blob frames before reading the next hash frame
            // or emitting a control frame. No partial committed base is persisted.
            drain_puts(io, store, state, state_path, &mut pending_puts, &mut report)?;
            drain_gets(
                io,
                store,
                state,
                state_path,
                &mut pending_gets,
                &mut ack_batch,
                &mut report,
                started,
            )?;
            put_bytes = 0;
            get_bytes = 0;
            if let Some(current) = hash_stream.as_mut().filter(|s| !s.ended) {
                if !last_chunk_paths.is_empty() {
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::HashRelease {
                            scan_id: current.scan_id.clone(),
                            paths: std::mem::take(&mut last_chunk_paths),
                        },
                    )?;
                }
                if let Some(shares) = requested_preempt
                    .take()
                    .or_else(|| audit_control(AuditWait::Tick).filter(|s| !s.is_empty()))
                {
                    flush_ack_batch(io, &share_id, &mut ack_batch, &mut report.metrics)?;
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::AuditPreempt {
                            shares: shares.clone(),
                        },
                    )?;
                    ensure!(
                        matches!(protocol::receive_for(io, &share_id)?, Message::ScanDeferred),
                        "expected stream deferred"
                    );
                    crate::trace_event!(
                        trace::Level::Info,
                        trace::Component::Scanner,
                        "STREAM_PREEMPT_APPLIED",
                        serde_json::json!({"phase":"transfer_boundary","frame_boundary":true})
                    );
                    return Ok(Report {
                        pending_wakes: shares,
                        round_deferred: true,
                        ..report
                    });
                }
                audit_control(AuditWait::Start);
                let next = current
                    .next_with_control(io, &share_id, &mut || audit_control(AuditWait::Tick));
                audit_control(AuditWait::End);
                let (chunk, metrics, pipeline) = match next {
                    Ok(chunk) => chunk,
                    Err(error) if error.is::<ScanDeferred>() => {
                        return Ok(Report {
                            round_deferred: true,
                            ..report
                        })
                    }
                    Err(error) => return Err(error),
                };
                if let Some(files) = chunk {
                    report.metrics.manifest_bytes += serde_json::to_vec(&files)?.len() as u64;
                    report.metrics.control_messages += 2;
                    last_chunk_paths.extend(files.keys().cloned());
                    work.extend(files.keys().cloned());
                    android.extend(files.into_iter().filter(|(p, _)| !store.excluded(p)));
                    continue;
                }
                peer_scan_metrics = metrics;
                report.metrics.scan_stream = pipeline;
                report.metrics.scan_stream.namespace_ms = namespace_ms.max(pipeline.namespace_ms);
                report.metrics.hash_stream_ms =
                    hash_started.map_or(0, |instant| instant.elapsed().as_millis());
                report.metrics.transfers_completed_before_hash_end = report.transferred as u64;
                // commit_scan has already installed the scan cache on the peer.
                // Only now can the private incoming stages mutate the SAF tree.
                install_remote_stages(
                    io,
                    store,
                    state,
                    &current.scan_id,
                    &mut staged_puts,
                    &mut report,
                )?;
                deferred_stage_bytes = 0;
                work.extend(paths.iter().filter(|p| !processed.contains(*p)).cloned());
                work.extend(std::mem::take(&mut deferred));
                state
                    .conflicts
                    .retain(|p| pc.contains_key(p) || android.contains_key(p));
                continue;
            }
            break;
        }
        let path = work.pop_front().unwrap();
        if processed.insert(path.clone()) {
            index += 1;
        }
        if store.excluded(&path) {
            continue;
        }
        let p = pc.get(&path);
        let a = android.get(&path);
        let ph = p.map(|e| e.hash.as_str());
        let ah = a.map(|e| e.hash.as_str());
        let legacy_base = state
            .legacy_files
            .get(&path)
            .filter(|_| !state.files.contains_key(&path))
            .cloned();
        // Migration checks are control requests; finish streaming and drain FIFO replies first.
        if legacy_base.is_some() && p.is_some() && a.is_some() && ph != ah {
            if hash_stream.as_ref().is_some_and(|s| !s.ended) {
                deferred.insert(path);
                continue;
            }
            drain_puts(io, store, state, state_path, &mut pending_puts, &mut report)?;
            put_bytes = 0;
            drain_gets(
                io,
                store,
                state,
                state_path,
                &mut pending_gets,
                &mut ack_batch,
                &mut report,
                started,
            )?;
            get_bytes = 0;
            flush_ack_batch(io, &share_id, &mut ack_batch, &mut report.metrics)?;
            protocol::send_for(io, &share_id, Message::ScanAlive)?;
            let mut alive = Instant::now();
            let local = store.snapshot(&path, p.unwrap())?;
            let old_hash =
                crate::legacy_hash_reader_with_control(File::open(local.path())?, || {
                    if alive.elapsed() >= Duration::from_secs(5) {
                        protocol::send_for(io, &share_id, Message::ScanAlive)?;
                        alive = Instant::now();
                    }
                    Ok(())
                })?
                .0;
            if Some(&old_hash) == legacy_base.as_ref() {
                state.files.insert(path.clone(), p.unwrap().hash.clone());
            } else {
                protocol::send_for(
                    io,
                    &share_id,
                    Message::LegacyHash {
                        path: path.clone(),
                        entry: a.unwrap().clone(),
                    },
                )?;
                let hash = loop {
                    match protocol::receive_for(io, &share_id)? {
                        Message::ScanAlive => continue,
                        Message::LegacyHashResult { hash } => break hash,
                        _ => anyhow::bail!("expected legacy base verification"),
                    }
                };
                crate::model::validate_hash(&hash)?;
                if Some(&hash) == legacy_base.as_ref() {
                    state.files.insert(path.clone(), a.unwrap().hash.clone());
                }
            }
        }
        let previous_base = state.files.get(&path).cloned();
        let bootstrap = remap.filter(|_| previous_base.is_none() && legacy_base.is_none());
        let reconciling = Instant::now();
        let action = match (bootstrap, p, a) {
            (Some(crate::config::RemapPolicy::Pc), Some(_), Some(_)) if ph != ah => {
                Action::ToAndroid
            }
            (Some(crate::config::RemapPolicy::Android), Some(_), Some(_)) if ph != ah => {
                Action::ToPc
            }
            (Some(crate::config::RemapPolicy::Compare), Some(_), Some(_)) if ph != ah => {
                Action::Conflict
            }
            _ => reconcile(state.files.get(&path).map(String::as_str), ph, ah),
        };
        if hash_stream.as_ref().is_some_and(|s| !s.ended)
            && (action == Action::Conflict
                || (action == Action::ToAndroid
                    && p.is_some_and(|e| {
                        e.size > MAX_STAGED_BYTES
                            || staged_puts.len() == MAX_IN_FLIGHT_FILES
                            || deferred_stage_bytes.saturating_add(e.size) > MAX_STAGED_BYTES
                    })))
        {
            deferred.insert(path);
            continue;
        }
        let prohibited = bootstrap.is_none()
            && match mode {
                crate::config::SyncMode::Bidirectional => false,
                crate::config::SyncMode::ToAndroid => {
                    matches!(action, Action::ToPc | Action::Conflict)
                }
                crate::config::SyncMode::ToPc => {
                    matches!(action, Action::ToAndroid | Action::Conflict)
                }
            };
        let _file = trace::current_context().file(&share_id, &path).enter();
        crate::trace_event!(
            trace::Level::Debug,
            trace::Component::Scheduler,
            "FILE_RECONCILE_DECISION",
            serde_json::json!({"relative_path":path,"action":format!("{action:?}"),"mode":format!("{mode:?}"),"prohibited":prohibited,"reason":if prohibited {"direction_policy"}else if bootstrap.is_some(){"remap_bootstrap"}else{"compare_committed_base_with_both_sides"},"base_hash":previous_base,"local_hash":ph,"remote_hash":ah})
        );
        // A failed scan or idle round has not changed either side's committed base.
        // Once a transfer can begin, retain the crash marker until Done succeeds.
        if !prohibited
            && matches!(action, Action::ToAndroid | Action::ToPc | Action::Conflict)
            && !pending_state.try_exists()?
        {
            atomic_write(&pending_state, COMMITTED_BASE_PENDING)?;
        }
        report.metrics.reconcile_ms += reconciling.elapsed().as_millis();
        if !pending_puts.is_empty() && (action != Action::ToAndroid || prohibited) {
            drain_puts(io, store, state, state_path, &mut pending_puts, &mut report)?;
            put_bytes = 0;
        }
        if !pending_gets.is_empty() && (action != Action::ToPc || prohibited) {
            drain_gets(
                io,
                store,
                state,
                state_path,
                &mut pending_gets,
                &mut ack_batch,
                &mut report,
                started,
            )?;
            get_bytes = 0;
        }
        if action != Action::None {
            progress(&path, index, total);
        }
        if prohibited {
            state.conflicts.insert(path.clone());
            report.conflicts += 1;
            continue;
        }
        if action != Action::Conflict && !path.starts_with("Rowd Conflicts/") {
            state.conflicts.remove(&path);
        }
        match action {
            Action::None => {
                if let Some(entry) = p {
                    store.acknowledge(&path, entry)?;
                    ack_batch.insert(path.clone(), entry.clone());
                    if ack_batch.len() == protocol::MANIFEST_CHUNK_FILES {
                        flush_ack_batch(io, &share_id, &mut ack_batch, &mut report.metrics)?;
                    }
                }
                if let Some(hash) = ph {
                    state.files.insert(path.clone(), hash.into());
                } else {
                    state.files.remove(&path);
                    state.legacy_files.remove(&path);
                }
            }
            Action::ToAndroid => {
                let _transfer = trace::transfer_context(&share_id, &path).enter();
                report
                    .metrics
                    .first_transfer_ms
                    .get_or_insert_with(|| started.elapsed().as_millis());
                let entry = p.unwrap();
                if pending_puts.len() == MAX_IN_FLIGHT_FILES
                    || put_bytes.saturating_add(entry.size) > MAX_STAGED_BYTES
                {
                    drain_puts(io, store, state, state_path, &mut pending_puts, &mut report)?;
                    put_bytes = 0;
                }
                let preparing = Instant::now();
                crate::trace_legacy_event!(
                    "sync",
                    "snapshot_start",
                    Some(&share_id),
                    Some(&path),
                    Some(entry.size),
                    None,
                    None,
                );
                let temp = store.snapshot(&path, entry).map_err(|e| {
                    trace::record_error(
                        trace::Component::Filesystem,
                        "SNAPSHOT_FAILED",
                        "filesystem",
                        "snapshot_local_file",
                        protocol::LocalOperation::attach(e, "snapshot", Some(&path)),
                    )
                })?;
                crate::trace_legacy_event!(
                    "sync",
                    "snapshot_end",
                    Some(&share_id),
                    Some(&path),
                    Some(entry.size),
                    Some(preparing),
                    None,
                );
                report.metrics.snapshot_ms += preparing.elapsed().as_millis();
                report.metrics.socket_idle_ms += preparing.elapsed().as_millis();
                if let Some(current) = hash_stream.as_ref().filter(|s| !s.ended) {
                    let sequence = staged_puts.len() as u64;
                    let sending = Instant::now();
                    report
                        .metrics
                        .time_to_first_transfer_ms
                        .get_or_insert_with(|| hash_started.unwrap().elapsed().as_millis());
                    report
                        .metrics
                        .first_byte_ms
                        .get_or_insert_with(|| started.elapsed().as_millis());
                    remote_stage(
                        io,
                        &share_id,
                        &current.scan_id,
                        sequence,
                        &path,
                        ah,
                        entry,
                        &temp,
                    )?;
                    report.metrics.transfer_ms += sending.elapsed().as_millis();
                    report.metrics.transfers_started_before_hash_end += 1;
                    report.metrics.files_received_for_deferred_install += 1;
                    report.metrics.bytes_transferred += entry.size;
                    report.metrics.control_messages += 2;
                    deferred_stage_bytes += entry.size;
                    staged_puts.push(StagedPut {
                        sequence,
                        path: path.clone(),
                        entry: entry.clone(),
                    });
                    report.metrics.peak_in_flight_files = report
                        .metrics
                        .peak_in_flight_files
                        .max(staged_puts.len() as u64);
                    report.metrics.peak_staged_bytes =
                        report.metrics.peak_staged_bytes.max(deferred_stage_bytes);
                    continue;
                }
                report
                    .metrics
                    .time_to_first_transfer_ms
                    .get_or_insert_with(|| {
                        hash_started.map_or_else(
                            || started.elapsed().as_millis(),
                            |instant| instant.elapsed().as_millis(),
                        )
                    });
                if entry.size > MAX_STAGED_BYTES {
                    let started_transfer = Instant::now();
                    report
                        .metrics
                        .first_byte_ms
                        .get_or_insert_with(|| started.elapsed().as_millis());
                    remote_install(&share_id, io, &path, ah, entry, &temp)?;
                    report.metrics.transfer_ms += started_transfer.elapsed().as_millis();
                    state.files.insert(path.clone(), entry.hash.clone());
                    store.acknowledge(&path, entry)?;
                    report.transferred += 1;
                    report.metrics.bytes_transferred += entry.size;
                    report.metrics.control_messages += 2;
                } else {
                    let started_transfer = Instant::now();
                    crate::trace_legacy_event!(
                        "sync",
                        "put_sent",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        None,
                        None,
                    );
                    let mut source = File::open(temp.path())?;
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::Put {
                            path: path.clone(),
                            expected: ah.map(str::to_owned),
                            entry: entry.clone(),
                        },
                    )?;
                    report
                        .metrics
                        .first_byte_ms
                        .get_or_insert_with(|| started.elapsed().as_millis());
                    crate::trace_legacy_event!(
                        "sync",
                        "blob_send_start",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        None,
                        None,
                    );
                    let sending = Instant::now();
                    protocol::copy_exact(&mut source, io, entry.size)?;
                    crate::trace_legacy_event!(
                        "sync",
                        "blob_send_end",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        Some(sending),
                        None,
                    );
                    report.metrics.transfer_ms += started_transfer.elapsed().as_millis();
                    put_bytes += entry.size;
                    pending_puts.push(PendingPut {
                        path: path.clone(),
                        entry: entry.clone(),
                        staged: temp,
                    });
                    report.metrics.peak_in_flight_files = report
                        .metrics
                        .peak_in_flight_files
                        .max(pending_puts.len() as u64);
                    report.metrics.peak_staged_bytes =
                        report.metrics.peak_staged_bytes.max(put_bytes);
                }
            }
            Action::ToPc => {
                report
                    .metrics
                    .time_to_first_transfer_ms
                    .get_or_insert_with(|| {
                        hash_started.map_or_else(
                            || started.elapsed().as_millis(),
                            |instant| instant.elapsed().as_millis(),
                        )
                    });
                if hash_stream.as_ref().is_some_and(|s| !s.ended) {
                    report.metrics.transfers_started_before_hash_end += 1;
                }
                let _transfer = trace::transfer_context(&share_id, &path).enter();
                report
                    .metrics
                    .first_transfer_ms
                    .get_or_insert_with(|| started.elapsed().as_millis());
                let entry = a.unwrap();
                if pending_gets.len() == MAX_IN_FLIGHT_FILES
                    || get_bytes.saturating_add(entry.size) > MAX_STAGED_BYTES
                {
                    drain_gets(
                        io,
                        store,
                        state,
                        state_path,
                        &mut pending_gets,
                        &mut ack_batch,
                        &mut report,
                        started,
                    )?;
                    get_bytes = 0;
                }
                report
                    .metrics
                    .time_to_first_transfer_ms
                    .get_or_insert_with(|| {
                        hash_started.map_or_else(
                            || started.elapsed().as_millis(),
                            |instant| instant.elapsed().as_millis(),
                        )
                    });
                if entry.size > MAX_STAGED_BYTES {
                    let started_transfer = Instant::now();
                    let temp = remote_snapshot(
                        &share_id,
                        io,
                        &path,
                        entry,
                        &mut report.metrics.first_byte_ms,
                        started,
                        store.staging_directory().as_deref(),
                    )?;
                    let installing = Instant::now();
                    crate::trace_legacy_event!(
                        "sync",
                        "install_start",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        None,
                        None,
                    );
                    store
                        .install_received(&path, ph, entry, temp)
                        .map_err(|e| protocol::LocalOperation::attach(e, "install", Some(&path)))?;
                    crate::trace_legacy_event!(
                        "sync",
                        "install_end",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        Some(installing),
                        None,
                    );
                    report.metrics.install_ms += installing.elapsed().as_millis();
                    report.metrics.transfer_ms += started_transfer.elapsed().as_millis();
                    ack_batch.insert(path.clone(), entry.clone());
                    if ack_batch.len() == protocol::MANIFEST_CHUNK_FILES {
                        flush_ack_batch(io, &share_id, &mut ack_batch, &mut report.metrics)?;
                    }
                    state.files.insert(path.clone(), entry.hash.clone());
                    report.transferred += 1;
                    report.metrics.bytes_transferred += entry.size;
                    report.metrics.control_messages += 2;
                } else {
                    crate::trace_legacy_event!(
                        "sync",
                        "get_sent",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        None,
                        None,
                    );
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::Get {
                            path: path.clone(),
                            entry: entry.clone(),
                        },
                    )?;
                    get_bytes += entry.size;
                    pending_gets.push(PendingGet {
                        path: path.clone(),
                        entry: entry.clone(),
                        expected: ph.map(str::to_owned),
                    });
                    report.metrics.peak_in_flight_files = report
                        .metrics
                        .peak_in_flight_files
                        .max(pending_gets.len() as u64);
                    report.metrics.peak_staged_bytes =
                        report.metrics.peak_staged_bytes.max(get_bytes);
                }
            }
            Action::Conflict => {
                report
                    .metrics
                    .time_to_first_transfer_ms
                    .get_or_insert_with(|| {
                        hash_started.map_or_else(
                            || started.elapsed().as_millis(),
                            |instant| instant.elapsed().as_millis(),
                        )
                    });
                report
                    .metrics
                    .first_transfer_ms
                    .get_or_insert_with(|| started.elapsed().as_millis());
                let pe = p.unwrap();
                let ae = a.unwrap();
                let pc_snapshot = store
                    .snapshot(&path, pe)
                    .map_err(|e| protocol::LocalOperation::attach(e, "snapshot", Some(&path)))?;
                let android_snapshot = remote_snapshot(
                    &share_id,
                    io,
                    &path,
                    ae,
                    &mut report.metrics.first_byte_ms,
                    started,
                    store.staging_directory().as_deref(),
                )?;
                let conflict = conflict_path(&path, &ae.hash);
                // Preserve Android on BOTH sides before changing its original path.
                // Missing precondition prevents overwriting a manually edited conflict copy.
                let installing = Instant::now();
                store
                    .install(&conflict, None, ae, &android_snapshot)
                    .map_err(|e| protocol::LocalOperation::attach(e, "install", Some(&conflict)))?;
                report.metrics.install_ms += installing.elapsed().as_millis();
                remote_install(&share_id, io, &conflict, None, ae, &android_snapshot)?;
                remote_install(&share_id, io, &path, ah, pe, &pc_snapshot)?;
                store.acknowledge(&path, pe)?;
                state.conflicts.insert(conflict.clone());
                state.files.insert(conflict, ae.hash.clone());
                state.files.insert(path.clone(), pe.hash.clone());
                report.conflicts += 1;
                report.transferred += 3;
                report.metrics.bytes_transferred += ae.size * 2 + pe.size;
                report.metrics.control_messages += 6;
            }
        }
    }
    drain_puts(io, store, state, state_path, &mut pending_puts, &mut report)?;
    drain_gets(
        io,
        store,
        state,
        state_path,
        &mut pending_gets,
        &mut ack_batch,
        &mut report,
        started,
    )?;
    flush_ack_batch(io, &share_id, &mut ack_batch, &mut report.metrics)?;
    crate::trace_legacy_event!(
        "sync",
        "reconcile_end",
        Some(&share_id),
        None,
        Some(total as u64),
        Some(started),
        None,
    );
    progress("", total, total);
    if let Some(binding) = scan_binding {
        store.validate_scan_snapshot()?;
        ensure!(
            store.scan_binding()? == binding,
            "STALE_SOURCE: PC binding changed during stream"
        );
    }
    if hash_stream.is_none() {
        report.metrics.time_to_first_transfer_ms = report.metrics.first_transfer_ms;
    }
    state
        .legacy_files
        .retain(|path, _| !state.files.contains_key(path));
    state.last_sync = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    );
    let new_token = crate::random_id()?;
    if !pending_state.try_exists()? {
        atomic_write(&pending_state, COMMITTED_BASE_PENDING)?;
    }
    let candidate_path = state_path.with_extension("next");
    persist_state(&candidate_path, state, &mut report.metrics, None)?;
    if hash_stream.is_some() {
        report.metrics.full_scans += peer_scan_metrics.full_scans;
        report.metrics.files_enumerated += peer_scan_metrics.files_enumerated;
    }
    let store_metrics = store.metrics();
    report.metrics.files_hashed = store_metrics
        .files_hashed
        .saturating_sub(store_metrics_before.files_hashed)
        + peer_scan_metrics.files_hashed;
    report.metrics.bytes_hashed = store_metrics
        .bytes_hashed
        .saturating_sub(store_metrics_before.bytes_hashed)
        + peer_scan_metrics.bytes_hashed;
    report.metrics.staging_copies = store_metrics
        .staging_copies
        .saturating_sub(store_metrics_before.staging_copies);
    report.metrics.control_messages += 1;
    report.metrics.total_ms = started.elapsed().as_millis();
    protocol::send_for(
        io,
        &share_id,
        Message::Done {
            transferred: report.transferred,
            conflicts: report.conflicts,
            base_token: new_token.clone(),
        },
    )?;
    std::fs::rename(&candidate_path, state_path)?;
    std::fs::remove_file(&pending_state)?;
    session_tokens()
        .lock()
        .unwrap()
        .insert(state_path.to_path_buf(), new_token);
    retry_paths().lock().unwrap().remove(state_path);
    crate::trace_legacy_event!(
        "sync",
        "share_sync_end",
        Some(&share_id),
        None,
        Some(report.metrics.bytes_transferred),
        Some(started),
        None,
    );
    Ok(report)
}

pub fn respond(io: &mut (impl Read + Write + Send), store: &mut impl Store) -> Result<Report> {
    respond_share(io, store, None)
}

struct ResponderStream {
    scan_id: String,
    binding: String,
    namespace: crate::model::Namespace,
    cached: Option<crate::model::Manifest>,
    sequence: u64,
    seen: BTreeSet<String>,
    hints: BTreeSet<String>,
    hashing: bool,
    before: crate::storage::StoreMetrics,
}

pub fn respond_share(
    io: &mut (impl Read + Write + Send),
    store: &mut impl Store,
    expected_share: Option<&str>,
) -> Result<Report> {
    let mut stream: Option<ResponderStream> = None;
    let mut stream_completed = false;
    let mut completed_scan: Option<(String, String)> = None;
    let mut incoming_stages = BTreeMap::<u64, ReceivedStage>::new();
    let mut stage_sequence = 0;
    let mut install_sequence = 0;
    let mut incoming_bytes = 0u64;
    let mut share = expected_share.map(str::to_owned);
    let started = Instant::now();
    let mut operation = "receive";
    let mut relative_path: Option<String> = None;
    let mut frame_boundary = true;
    let mut stage_body_incomplete = false;
    let mut error_kind = "protocol";
    crate::trace_legacy_event!(
        "sync",
        "share_sync_start",
        expected_share,
        None,
        None,
        None,
        None,
    );
    let result = (|| -> Result<Report> {
        loop {
            operation = "receive";
            error_kind = "transport";
            let Message::Scoped { share_id, message } = protocol::receive(io)? else {
                anyhow::bail!("missing Share context")
            };
            if let Some(expected) = &share {
                ensure!(expected == &share_id, "wrong Share context");
            } else {
                share = Some(share_id.clone());
            }
            operation = message.trace_type();
            error_kind = "protocol";
            match protocol::check_peer_error(*message)? {
                Message::AckBatch { entries } => {
                    crate::trace_legacy_event!(
                        "sync",
                        "ack_received",
                        Some(&share_id),
                        None,
                        Some(entries.len() as u64),
                        None,
                        None,
                    );
                    ensure!(
                        !entries.is_empty() && entries.len() <= protocol::MANIFEST_CHUNK_FILES,
                        "invalid ACK batch"
                    );
                    validate_manifest(&entries)?;
                    for (path, entry) in entries {
                        store.acknowledge(&path, &entry)?;
                    }
                }
                Message::Scan => {
                    ensure!(
                        stream.is_none() && incoming_stages.is_empty(),
                        "scan already active or stages pending"
                    );
                    stream_completed = false;
                    completed_scan = None;
                    stage_sequence = 0;
                    install_sequence = 0;
                    let before = store.metrics();
                    let binding = store.scan_binding()?;
                    let physical_namespace = match store.stream_namespace(io) {
                        Ok(namespace) => namespace,
                        Err(error) if error.is::<ScanDeferred>() => {
                            store.discard_scan()?;
                            protocol::send_for(io, &share_id, Message::ScanDeferred)?;
                            return Ok(Report {
                                round_deferred: true,
                                ..Default::default()
                            });
                        }
                        Err(error) => return Err(error),
                    };
                    let (namespace, files) = match physical_namespace {
                        Some(namespace) => (namespace, None),
                        None => {
                            let files = match store.scan_with_control(io) {
                                Ok(files) => files,
                                Err(error) if error.is::<ScanDeferred>() => {
                                    store.discard_scan()?;
                                    protocol::send_for(io, &share_id, Message::ScanDeferred)?;
                                    return Ok(Report {
                                        round_deferred: true,
                                        ..Default::default()
                                    });
                                }
                                Err(error) => return Err(error),
                            };
                            (crate::model::manifest_namespace(&files), Some(files))
                        }
                    };
                    crate::model::validate_namespace(&namespace)?;
                    protocol::send_for(io, &share_id, Message::ScanReady)?;
                    match protocol::receive_for(io, &share_id)? {
                        Message::ScanContinue => {}
                        Message::AuditPreempt { shares } => {
                            for id in shares {
                                crate::model::validate_hash(&id)?;
                            }
                            store.discard_scan()?;
                            protocol::send_for(io, &share_id, Message::ScanDeferred)?;
                            return Ok(Report {
                                round_deferred: true,
                                ..Default::default()
                            });
                        }
                        _ => anyhow::bail!("unexpected namespace gate"),
                    }
                    let scan_id = crate::random_id()?;
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::ScanStreamBegin {
                            scan_id: scan_id.clone(),
                            binding: binding.clone(),
                        },
                    )?;
                    let mut sequence = 0;
                    let mut chunk = crate::model::Namespace::new();
                    for (path, entry) in &namespace {
                        chunk.insert(path.clone(), entry.clone());
                        if chunk.len() == protocol::SCAN_STREAM_CHUNK_FILES {
                            protocol::send_for(
                                io,
                                &share_id,
                                Message::NamespaceChunk {
                                    scan_id: scan_id.clone(),
                                    sequence,
                                    entries: std::mem::take(&mut chunk),
                                },
                            )?;
                            sequence += 1;
                        }
                    }
                    if !chunk.is_empty() {
                        protocol::send_for(
                            io,
                            &share_id,
                            Message::NamespaceChunk {
                                scan_id: scan_id.clone(),
                                sequence,
                                entries: chunk,
                            },
                        )?;
                        sequence += 1;
                    }
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::NamespaceEnd {
                            scan_id: scan_id.clone(),
                            last_sequence: sequence,
                            total_entries: namespace.len(),
                            namespace_digest: protocol::namespace_digest(&namespace)?,
                        },
                    )?;
                    stream = Some(ResponderStream {
                        scan_id,
                        binding,
                        namespace,
                        cached: files,
                        sequence: 0,
                        seen: Default::default(),
                        hints: Default::default(),
                        hashing: false,
                        before,
                    });
                }
                Message::HashStageHint { scan_id, paths } => {
                    let current = stream.as_mut().context("stage hint without namespace")?;
                    ensure!(
                        !current.hashing
                            && scan_id == current.scan_id
                            && paths.len() <= protocol::SCAN_STREAM_CHUNK_FILES,
                        "invalid stage hint"
                    );
                    for path in paths {
                        ensure!(
                            current.namespace.get(&path).is_some_and(|e| !e.directory)
                                && current.hints.insert(path),
                            "invalid or duplicate stage hint path"
                        );
                    }
                }
                Message::HashRelease { scan_id, paths } => {
                    let current = stream.as_ref().context("hash release without stream")?;
                    ensure!(
                        current.hashing
                            && scan_id == current.scan_id
                            && paths.len() <= protocol::SCAN_STREAM_CHUNK_FILES
                            && paths.iter().all(|p| current.seen.contains(p)),
                        "invalid hash release"
                    );
                    store.release_hash_staging(&paths)?;
                }
                Message::HashStreamBegin { scan_id, binding } => {
                    let current = stream.as_mut().context("hash begin without namespace")?;
                    ensure!(
                        !current.hashing
                            && scan_id == current.scan_id
                            && binding == current.binding
                            && store.scan_binding()? == binding,
                        "stale hash begin"
                    );
                    store.start_hash_stream(&current.hints)?;
                    current.hashing = true;
                }
                Message::StagePut {
                    scan_id,
                    sequence,
                    path,
                    entry,
                    expected,
                } => {
                    // StagePut owns raw Blob bytes immediately after its header.
                    // Any early semantic error closes this connection; never inject
                    // a ShareError into an unread body.
                    frame_boundary = false;
                    stage_body_incomplete = true;
                    let current = stream
                        .as_ref()
                        .context("staging without active hash stream")?;
                    ensure!(
                        current.hashing && scan_id == current.scan_id && sequence == stage_sequence,
                        "misaligned staged transfer"
                    );
                    ensure!(
                        store.scan_binding()? == current.binding,
                        "STALE_TARGET: scan binding changed"
                    );
                    crate::model::validate_path(&path)?;
                    crate::model::validate_hash(&entry.hash)?;
                    if let Some(hash) = &expected {
                        crate::model::validate_hash(hash)?;
                    }
                    ensure!(!store.excluded(&path), "ignored staged path");
                    ensure!(
                        incoming_stages.len() < MAX_IN_FLIGHT_FILES
                            && entry.size <= MAX_STAGED_BYTES
                            && incoming_bytes.saturating_add(entry.size) <= MAX_STAGED_BYTES,
                        "deferred staging limit exceeded"
                    );
                    ensure!(
                        !incoming_stages.values().any(|s| s.job.path == path),
                        "duplicate staged path"
                    );
                    ensure!(
                        if current.namespace.contains_key(&path) {
                            expected.is_some() && current.seen.contains(&path)
                        } else {
                            expected.is_none()
                        },
                        "staged target has not been reconciled"
                    );
                    operation = "receive_staged_blob";
                    relative_path = Some(path.clone());
                    error_kind = "filesystem";
                    let staged = receive_blob(io, &entry, store.staging_directory().as_deref())?;
                    frame_boundary = true;
                    stage_body_incomplete = false;
                    ensure!(
                        store.scan_binding()? == current.binding,
                        "STALE_TARGET: scan binding changed during receipt"
                    );
                    incoming_bytes += entry.size;
                    incoming_stages.insert(
                        sequence,
                        ReceivedStage {
                            job: IncomingPut {
                                path: path.clone(),
                                entry,
                                expected,
                            },
                            staged,
                        },
                    );
                    stage_sequence += 1;
                    crate::trace_event!(
                        trace::Level::Debug,
                        trace::Component::Transfer,
                        "PC_TO_ANDROID_STAGED",
                        serde_json::json!({"scan_id":scan_id,"sequence":sequence,"relative_path":path,"staged_bytes":incoming_bytes,"install_deferred":true})
                    );
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::StageReceived { scan_id, sequence },
                    )?;
                }
                Message::InstallStaged { scan_id, sequence } => {
                    ensure!(
                        stream.is_none() && stream_completed && sequence == install_sequence,
                        "staged installation before scan commit or out of order"
                    );
                    let (completed_id, binding) = completed_scan
                        .as_ref()
                        .context("missing completed scan identity")?;
                    ensure!(
                        scan_id == *completed_id && store.scan_binding()? == *binding,
                        "STALE_TARGET: staged scan binding changed"
                    );
                    let staged = incoming_stages
                        .remove(&sequence)
                        .context("missing or duplicate staged installation")?;
                    incoming_bytes -= staged.job.entry.size;
                    operation = "install_staged";
                    relative_path = Some(staged.job.path.clone());
                    error_kind = "filesystem";
                    store
                        .install_received(
                            &staged.job.path,
                            staged.job.expected.as_deref(),
                            &staged.job.entry,
                            staged.staged,
                        )
                        .map_err(|e| {
                            protocol::LocalOperation::attach(e, "install", Some(&staged.job.path))
                        })?;
                    install_sequence += 1;
                    crate::trace_event!(
                        trace::Level::Debug,
                        trace::Component::Transfer,
                        "PC_TO_ANDROID_STAGE_INSTALLED",
                        serde_json::json!({"scan_id":scan_id,"sequence":sequence,"relative_path":staged.job.path})
                    );
                    protocol::send_for(io, &share_id, Message::Accept)?;
                }
                Message::HashNext { scan_id, sequence } => {
                    let current = stream.as_mut().context("hash request without stream")?;
                    ensure!(
                        current.hashing
                            && scan_id == current.scan_id
                            && sequence == current.sequence,
                        "misaligned hash request"
                    );
                    let chunk = if let Some(files) = &mut current.cached {
                        let keys: Vec<_> = files
                            .keys()
                            .take(protocol::SCAN_STREAM_CHUNK_FILES)
                            .cloned()
                            .collect();
                        if keys.is_empty() {
                            None
                        } else {
                            Some(
                                keys.into_iter()
                                    .map(|path| {
                                        let entry = files.remove(&path).unwrap();
                                        (path, entry)
                                    })
                                    .collect(),
                            )
                        }
                    } else {
                        match store.next_hash_chunk(io) {
                            Ok(chunk) => chunk,
                            Err(error) if error.is::<ScanDeferred>() => {
                                store.discard_scan()?;
                                protocol::send_for(io, &share_id, Message::ScanDeferred)?;
                                return Ok(Report {
                                    round_deferred: true,
                                    ..Default::default()
                                });
                            }
                            Err(error) => return Err(error),
                        }
                    };
                    if let Some(files) = chunk {
                        validate_manifest(&files)?;
                        ensure!(
                            !files.is_empty() && files.len() <= protocol::SCAN_STREAM_CHUNK_FILES,
                            "invalid produced chunk"
                        );
                        for (path, entry) in &files {
                            ensure!(
                                current
                                    .namespace
                                    .get(path)
                                    .is_some_and(|e| !e.directory && e.size == entry.size)
                                    && current.seen.insert(path.clone()),
                                "hash namespace mismatch"
                            );
                        }
                        protocol::send_for(
                            io,
                            &share_id,
                            Message::HashChunk {
                                scan_id,
                                sequence,
                                files,
                            },
                        )?;
                        current.sequence += 1;
                    } else {
                        ensure!(
                            current.seen.len()
                                == current.namespace.values().filter(|e| !e.directory).count()
                                && store.scan_binding()? == current.binding,
                            "incomplete or stale hash stream"
                        );
                        store.commit_scan()?;
                        store.set_base_token(None);
                        let after = store.metrics();
                        let pipeline = store.scan_stream_metrics()?;
                        completed_scan = Some((current.scan_id.clone(), current.binding.clone()));
                        protocol::send_for(
                            io,
                            &share_id,
                            Message::HashStreamEnd {
                                scan_id,
                                last_sequence: sequence,
                                total_entries: current.seen.len(),
                                metrics: crate::storage::StoreMetrics {
                                    files_enumerated: after
                                        .files_enumerated
                                        .saturating_sub(current.before.files_enumerated),
                                    files_hashed: after
                                        .files_hashed
                                        .saturating_sub(current.before.files_hashed),
                                    bytes_hashed: after
                                        .bytes_hashed
                                        .saturating_sub(current.before.bytes_hashed),
                                    full_scans: 1,
                                    ..Default::default()
                                },
                                pipeline,
                            },
                        )?;
                        stream = None;
                        stream_completed = true;
                    }
                }
                Message::AuditPreempt { shares } => {
                    ensure!(
                        stream.is_some() || stream_completed,
                        "preempt outside stream"
                    );
                    for id in shares {
                        crate::model::validate_hash(&id)?;
                    }
                    store.discard_scan()?;
                    protocol::send_for(io, &share_id, Message::ScanDeferred)?;
                    return Ok(Report {
                        round_deferred: true,
                        ..Default::default()
                    });
                }
                Message::DeltaScan { base_token, paths } => {
                    let old = store.base_token();
                    let before = store.metrics();
                    let mut fallback_reason = if old.as_deref() != Some(&base_token) {
                        Some("no_session_token")
                    } else if paths.len() > 1024 {
                        Some("too_many_dirty_paths")
                    } else if paths
                        .iter()
                        .any(|p| crate::model::validate_path(p).is_err())
                    {
                        Some("invalid_dirty_paths")
                    } else {
                        None
                    };
                    let candidate = if old.as_deref() == Some(&base_token)
                        && paths.len() <= 1024
                        && paths.iter().all(|p| crate::model::validate_path(p).is_ok())
                    {
                        match store.delta_scan_with_control(io, &paths) {
                            Ok(Some(candidate)) => Some(candidate),
                            Ok(None) => {
                                fallback_reason = Some("dirty_unavailable");
                                None
                            }
                            Err(error) if error.is::<ScanDeferred>() => {
                                store.discard_scan()?;
                                protocol::send_for(io, &share_id, Message::ScanDeferred)?;
                                return Ok(Report {
                                    round_deferred: true,
                                    ..Default::default()
                                });
                            }
                            Err(error) => return Err(error),
                        }
                    } else {
                        None
                    };
                    protocol::send_for(io, &share_id, Message::ScanReady)?;
                    match protocol::receive_for(io, &share_id)? {
                        Message::ScanContinue => {}
                        Message::AuditPreempt { shares } => {
                            for id in shares {
                                crate::model::validate_hash(&id)?;
                            }
                            store.discard_scan()?;
                            protocol::send_for(io, &share_id, Message::ScanDeferred)?;
                            return Ok(Report {
                                round_deferred: true,
                                ..Default::default()
                            });
                        }
                        _ => anyhow::bail!("unexpected delta scan gate"),
                    }
                    if let Some((paths, files)) = candidate {
                        let after = store.metrics();
                        protocol::send_for(
                            io,
                            &share_id,
                            Message::DeltaManifest {
                                paths,
                                files,
                                metrics: crate::storage::StoreMetrics {
                                    files_enumerated: after
                                        .files_enumerated
                                        .saturating_sub(before.files_enumerated),
                                    files_hashed: after
                                        .files_hashed
                                        .saturating_sub(before.files_hashed),
                                    bytes_hashed: after
                                        .bytes_hashed
                                        .saturating_sub(before.bytes_hashed),
                                    ..Default::default()
                                },
                            },
                        )?;
                    } else {
                        protocol::send_for(io, &share_id, Message::NeedFullScan)?;
                        crate::trace_legacy_event!(
                            "sync",
                            "delta_fallback",
                            Some(&share_id),
                            None,
                            None,
                            None,
                            fallback_reason.or(Some("cache_untrusted")),
                        );
                    }
                }
                Message::ScanAlive => {
                    // The coordinator can verify a legacy local base between stream completion and transfer.
                    ensure!(
                        stream.is_none(),
                        "unexpected coordinator liveness during stream"
                    );
                }
                Message::LegacyHash { path, entry } => {
                    ensure!(stream.is_none(), "legacy verification during hash stream");
                    crate::model::validate_path(&path)?;
                    crate::model::validate_hash(&entry.hash)?;
                    ensure!(
                        entry.size <= crate::model::MAX_FILE,
                        "legacy source too large"
                    );
                    operation = "legacy_base_verification";
                    relative_path = Some(path.clone());
                    error_kind = "filesystem";
                    let hash = store.legacy_hash_with_control(io, &path, &entry)?;
                    protocol::send_for(io, &share_id, Message::LegacyHashResult { hash })?;
                }
                Message::Get { path, entry } => {
                    let _transfer = trace::transfer_context(&share_id, &path).enter();
                    crate::model::validate_path(&path)?;
                    crate::trace_legacy_event!(
                        "sync",
                        "get_received",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        None,
                        None,
                    );
                    let snapshot_started = Instant::now();
                    crate::trace_legacy_event!(
                        "sync",
                        "snapshot_start",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        None,
                        None,
                    );
                    operation = "snapshot";
                    relative_path = Some(path.clone());
                    error_kind = "filesystem";
                    let temp = store.snapshot(&path, &entry)?;
                    let mut source = File::open(temp.path())?;
                    crate::trace_legacy_event!(
                        "sync",
                        "snapshot_end",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        Some(snapshot_started),
                        None,
                    );
                    operation = "blob_send";
                    error_kind = "transport";
                    frame_boundary = false;
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::Blob {
                            entry: entry.clone(),
                        },
                    )?;
                    crate::trace_legacy_event!(
                        "sync",
                        "blob_send_start",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        None,
                        None,
                    );
                    let sending = Instant::now();
                    protocol::copy_exact(&mut source, io, entry.size)?;
                    frame_boundary = true;
                    crate::trace_legacy_event!(
                        "sync",
                        "blob_send_end",
                        Some(&share_id),
                        Some(&path),
                        Some(entry.size),
                        Some(sending),
                        None,
                    );
                }
                Message::Put {
                    path,
                    entry,
                    expected,
                } => {
                    ensure!(stream.is_none(), "SAF install forbidden during hash stream");
                    operation = "receive_put_batch";
                    relative_path = Some(path.clone());
                    error_kind = "filesystem";
                    frame_boundary = false;
                    let installed = receive_put_batch(
                        io,
                        store,
                        &share_id,
                        IncomingPut {
                            path,
                            entry,
                            expected,
                        },
                    )?;
                    frame_boundary = true;
                    for path in installed {
                        let _transfer = trace::transfer_context(&share_id, &path).enter();
                        protocol::send_for(io, &share_id, Message::Accept)?;
                        crate::trace_legacy_event!(
                            "sync",
                            "accept_sent",
                            Some(&share_id),
                            Some(&path),
                            None,
                            None,
                            None,
                        );
                    }
                }
                Message::Done {
                    transferred,
                    conflicts,
                    base_token,
                } => {
                    ensure!(
                        stream.is_none() && incoming_stages.is_empty(),
                        "Done before hash stream end or staged installs"
                    );
                    crate::model::validate_hash(&base_token)?;
                    store.discard_scan()?;
                    store.set_base_token(Some(base_token));
                    crate::trace_legacy_event!(
                        "sync",
                        "share_sync_end",
                        Some(&share_id),
                        None,
                        Some(transferred as u64),
                        Some(started),
                        None,
                    );
                    return Ok(Report {
                        pending_wakes: Vec::new(),
                        round_deferred: false,
                        transferred,
                        conflicts,
                        shares_processed: 1,
                        metrics: ShareMetrics::default(),
                    });
                }
                _ => anyhow::bail!("unexpected protocol message"),
            }
        }
    })();
    let result = if stage_body_incomplete {
        result.map_err(|e| e.context(protocol::IncompleteFrame))
    } else {
        result
    };
    if result.is_err() {
        let _ = store.discard_scan();
    }
    if let Err(ref e) = result {
        let context = if let Some(share) = share.as_deref() {
            trace::current_context().with("share_id", share)
        } else {
            trace::current_context()
        };
        let _scope = context.enter();
        crate::trace_event!(
            trace::Level::Error,
            trace::Component::Round,
            "SHARE_FAILED",
            serde_json::json!({"reason":if e.to_string().starts_with("STALE_"){"stale"}else{"round_failed"},"operation":operation,"relative_path":relative_path,"kind":error_kind,"frame_boundary":frame_boundary,"error":trace::TraceError::new(error_kind,operation,e)})
        );
        if frame_boundary {
            protocol::send_share_error(
                io,
                e,
                if cfg!(target_os = "android") {
                    "android"
                } else {
                    "responder"
                },
                share.as_deref(),
                operation,
                relative_path.as_deref(),
                error_kind,
            );
        }
    }
    result
}

pub fn client_round(
    invitation: &Invitation,
    root_id: &str,
    store: &mut impl Store,
) -> Result<Report> {
    let mut io = crate::tls::connect(invitation)?;
    protocol::client_auth(&mut io, &invitation.pair_id, &invitation.secret, root_id)
        .context("pairing authentication")?;
    respond(&mut io, store)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    fn pair() -> (
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        for socket in [&a, &b] {
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
        }
        (a, b)
    }
    #[test]
    #[cfg(unix)]
    fn truncated_or_corrupt_transfer_leaves_destination_untouched() {
        use crate::storage::LocalStore;
        use std::net::Shutdown;
        for truncated in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a"), b"original").unwrap();
            let mut store = LocalStore::open(dir.path()).unwrap();
            let old = store.scan().unwrap()["a"].clone();
            let (hash, size) = hash_reader(&b"correct"[..]).unwrap();
            let (mut sender, mut receiver) = pair();
            std::thread::scope(|scope| {
                let task = scope.spawn(|| respond(&mut receiver, &mut store));
                protocol::send_for(
                    &mut sender,
                    "test",
                    Message::Put {
                        path: "a".into(),
                        entry: Entry { hash, size },
                        expected: Some(old.hash),
                    },
                )
                .unwrap();
                sender
                    .write_all(if truncated { b"bad" } else { b"invalid" })
                    .unwrap();
                sender.shutdown(Shutdown::Write).unwrap();
                assert!(protocol::receive(&mut sender).is_err());
                assert!(task.join().unwrap().is_err());
            });
            assert_eq!(std::fs::read(dir.path().join("a")).unwrap(), b"original");
        }
    }

    #[test]
    #[cfg(unix)]
    fn partial_ack_batch_never_completes_a_round() {
        use crate::storage::LocalStore;
        use std::net::Shutdown;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a"), b"content").unwrap();
        let mut store = LocalStore::open(directory.path()).unwrap();
        let entry = store.scan().unwrap()["a"].clone();
        let (mut sender, mut receiver) = pair();
        std::thread::scope(|scope| {
            let responder = scope.spawn(|| respond_share(&mut receiver, &mut store, Some("share")));
            protocol::send_for(
                &mut sender,
                "share",
                Message::AckBatch {
                    entries: crate::model::Manifest::from([("a".into(), entry)]),
                },
            )
            .unwrap();
            sender.shutdown(Shutdown::Write).unwrap();
            assert!(responder.join().unwrap().is_err());
        });
        let (mut sender, mut receiver) = pair();
        std::thread::scope(|scope| {
            let responder = scope.spawn(|| respond_share(&mut receiver, &mut store, Some("share")));
            protocol::send_for(
                &mut sender,
                "share",
                Message::AckBatch {
                    entries: crate::model::Manifest::new(),
                },
            )
            .unwrap();
            sender.shutdown(Shutdown::Write).unwrap();
            assert!(responder.join().unwrap().is_err());
        });
    }

    #[test]
    fn state_is_bound_to_pair_and_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic_json(&path, &State::load(&path, "pair", "folder").unwrap()).unwrap();
        assert!(State::load(&path, "other", "folder").is_err());
        assert!(State::load(&path, "pair", "other").is_err());
    }

    #[test]
    fn interrupted_round_keeps_committed_reconciliation_base() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = State::load(&path, "pair", "share").unwrap();
        state.files.insert("a".into(), "old".into());
        state.last_sync = Some(1);
        atomic_json(&path, &state).unwrap();
        atomic_write(&path.with_extension("pending"), COMMITTED_BASE_PENDING).unwrap();
        let recovered = State::load(&path, "pair", "share").unwrap();
        assert_eq!(recovered.files["a"], "old");
        assert_eq!(recovered.last_sync, Some(1));
        assert_eq!(
            reconcile(
                recovered.files.get("a").map(String::as_str),
                Some("new"),
                Some("old")
            ),
            Action::ToAndroid
        );
    }

    #[test]
    #[cfg(unix)]
    fn edit_after_manifest_is_preserved_then_becomes_a_conflict() {
        use crate::storage::LocalStore;
        struct EditingStore {
            inner: LocalStore,
            edit: bool,
        }
        impl Store for EditingStore {
            fn scan(&mut self) -> Result<crate::model::Manifest> {
                self.inner.scan()
            }
            fn snapshot(&mut self, path: &str, entry: &Entry) -> Result<Snapshot> {
                self.inner.snapshot(path, entry)
            }
            fn install(
                &mut self,
                path: &str,
                expected: Option<&str>,
                entry: &Entry,
                staged: &VerifiedStaged,
            ) -> Result<()> {
                if self.edit {
                    self.edit = false;
                    std::fs::write(self.inner.root().join(path), b"edited during transfer")?;
                }
                self.inner.install(path, expected, entry, staged)
            }
        }
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("a"), b"pc version").unwrap();
        std::fs::write(phone.path().join("a"), b"base").unwrap();
        let mut p = LocalStore::open(pc.path()).unwrap();
        let mut a = EditingStore {
            inner: LocalStore::open(phone.path()).unwrap(),
            edit: true,
        };
        let path = p.private().join("state.json");
        let mut state = State::load(&path, "pair", "folder").unwrap();
        state
            .files
            .insert("a".into(), hash_reader(&b"base"[..]).unwrap().0);
        let (mut x, mut y) = pair();
        std::thread::scope(|scope| {
            let task = scope.spawn(|| respond(&mut y, &mut a));
            assert!(coordinate(&mut x, &mut p, &mut state, &path)
                .unwrap_err()
                .to_string()
                .contains("STALE_TARGET"));
            assert!(task.join().unwrap().is_err());
        });
        assert_eq!(
            std::fs::read(phone.path().join("a")).unwrap(),
            b"edited during transfer"
        );
        let (mut x, mut y) = pair();
        std::thread::scope(|scope| {
            let task = scope.spawn(|| respond(&mut y, &mut a));
            assert_eq!(
                coordinate(&mut x, &mut p, &mut state, &path)
                    .unwrap()
                    .conflicts,
                1
            );
            task.join().unwrap().unwrap();
        });
        assert_eq!(p.scan().unwrap(), a.scan().unwrap());
        assert_eq!(p.scan().unwrap().len(), 2);
    }
    #[test]
    #[cfg(unix)]
    fn bidirectional_conflict_converges_and_replay_is_noop() {
        use crate::storage::LocalStore;
        let pc = tempfile::tempdir().unwrap();
        let android = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("a.txt"), b"pc").unwrap();
        std::fs::write(android.path().join("a.txt"), b"phone").unwrap();
        std::fs::write(android.path().join("only-phone"), b"new").unwrap();
        let mut p = LocalStore::open(pc.path()).unwrap();
        let mut a = LocalStore::open(android.path()).unwrap();
        let state_path = pc.path().join(".rowd/state.json");
        let mut state = State::load(&state_path, "pair", "folder").unwrap();
        for round in 0..3 {
            let (mut x, mut y) = pair();
            let report = std::thread::scope(|scope| {
                let responder = scope.spawn(|| respond(&mut y, &mut a));
                let report = coordinate(&mut x, &mut p, &mut state, &state_path).unwrap();
                responder.join().unwrap().unwrap();
                report
            });
            if round == 0 {
                assert_eq!(report.conflicts, 1);
                assert!(report.metrics.bytes_transferred >= 5);
                assert!(
                    report.metrics.files_hashed > 0,
                    "report={}, store={}",
                    report.metrics.files_hashed,
                    p.metrics().files_hashed
                );
                assert_eq!(report.metrics.full_scans, 2);
            } else {
                assert_eq!(report.transferred, 0);
                assert_eq!(report.metrics.bytes_transferred, 0);
            }
            assert_eq!(p.scan().unwrap(), a.scan().unwrap());
        }
        std::fs::remove_file(android.path().join("a.txt")).unwrap();
        let (mut x, mut y) = pair();
        std::thread::scope(|s| {
            let h = s.spawn(|| respond(&mut y, &mut a));
            coordinate(&mut x, &mut p, &mut state, &state_path).unwrap();
            h.join().unwrap().unwrap();
        });
        assert_eq!(p.scan().unwrap(), a.scan().unwrap());
    }
    #[test]
    #[cfg(unix)]
    fn cross_peer_file_directory_collision_stops_before_transfer() {
        use crate::storage::LocalStore;
        let pc = tempfile::tempdir().unwrap();
        let android = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("a"), b"pc").unwrap();
        std::fs::create_dir(android.path().join("a")).unwrap();
        std::fs::write(android.path().join("a/b"), b"android").unwrap();
        let mut p = LocalStore::open(pc.path()).unwrap();
        let mut a = LocalStore::open(android.path()).unwrap();
        let state_path = pc.path().join(".rowd/state.json");
        let mut state = State::load(&state_path, "pair", "share").unwrap();
        let (mut x, mut y) = pair();
        std::thread::scope(|scope| {
            let responder = scope.spawn(|| respond(&mut y, &mut a));
            assert!(coordinate(&mut x, &mut p, &mut state, &state_path)
                .unwrap_err()
                .to_string()
                .contains("file/directory collision"));
            drop(x);
            assert!(responder.join().unwrap().is_err());
        });
        assert_eq!(std::fs::read(pc.path().join("a")).unwrap(), b"pc");
        assert_eq!(
            std::fs::read(android.path().join("a/b")).unwrap(),
            b"android"
        );
    }
}

#[cfg(all(test, unix))]
mod v2_tests {
    use super::*;
    use crate::{
        config::{RemapPolicy, SyncMode},
        storage::LocalStore,
    };
    use std::{
        os::unix::net::UnixStream,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };
    fn sockets() -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        for s in [&a, &b] {
            s.set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            s.set_write_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
        }
        (a, b)
    }
    fn local_round(
        pc: &mut LocalStore,
        phone: &mut LocalStore,
        state: &mut State,
        path: &Path,
        remap: Option<RemapPolicy>,
    ) -> Report {
        let (mut a, mut b) = sockets();
        std::thread::scope(|scope| {
            let responder = scope.spawn(|| respond(&mut b, phone));
            let report = coordinate_with_progress(
                &mut a,
                pc,
                state,
                path,
                SyncMode::Bidirectional,
                remap,
                |_, _, _| {},
            )
            .unwrap();
            responder.join().unwrap().unwrap();
            report
        })
    }
    #[test]
    fn sha256_base_migrates_without_false_conflicts_or_wrong_direction() {
        for (pc_bytes, phone_bytes, conflicts, transfers) in [
            (b"base".as_slice(), b"base".as_slice(), 0, 0),
            (b"pc edit".as_slice(), b"base".as_slice(), 0, 1),
            (b"base".as_slice(), b"phone edit".as_slice(), 0, 1),
            (b"pc edit".as_slice(), b"phone edit".as_slice(), 1, 3),
        ] {
            let pc = tempfile::tempdir().unwrap();
            let phone = tempfile::tempdir().unwrap();
            std::fs::write(pc.path().join("file"), pc_bytes).unwrap();
            std::fs::write(phone.path().join("file"), phone_bytes).unwrap();
            let mut p = LocalStore::open(pc.path()).unwrap();
            let mut a = LocalStore::open(phone.path()).unwrap();
            let path = pc.path().join(".rowd/state.json");
            let old = crate::legacy_hash_reader(b"base".as_slice()).unwrap().0;
            atomic_json(
                &path,
                &serde_json::json!({"version":1,"pair_id":"pair","share_id":"share",
                "peer_root":null,"files":{"file":old}}),
            )
            .unwrap();
            let mut state = State::load(&path, "pair", "share").unwrap();
            assert!(state.files.is_empty());
            assert_eq!(state.legacy_files["file"], old);
            let report = local_round(&mut p, &mut a, &mut state, &path, None);
            assert_eq!(report.conflicts, conflicts);
            assert_eq!(report.transferred, transfers);
            assert_eq!(p.scan().unwrap(), a.scan().unwrap());
            let expected = if pc_bytes == b"base" {
                phone_bytes
            } else {
                pc_bytes
            };
            assert_eq!(std::fs::read(pc.path().join("file")).unwrap(), expected);
            let mut reloaded = State::load(&path, "pair", "share").unwrap();
            assert!(reloaded.blake3 && reloaded.legacy_files.is_empty());
            assert_eq!(
                reloaded.files["file"],
                crate::hash_reader(expected).unwrap().0
            );
            let report = local_round(&mut p, &mut a, &mut reloaded, &path, None);
            assert_eq!(report.transferred, 0);
        }
    }

    #[test]
    fn ignored_legacy_base_survives_until_the_path_can_be_reconciled() {
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("file.tmp"), b"pc edit").unwrap();
        std::fs::write(phone.path().join("file.tmp"), b"base").unwrap();
        let mut p = LocalStore::open_with_policy(pc.path(), "*.tmp").unwrap();
        let mut a = LocalStore::open_with_policy(phone.path(), "*.tmp").unwrap();
        let path = pc.path().join(".rowd/state.json");
        let old = crate::legacy_hash_reader(b"base".as_slice()).unwrap().0;
        atomic_json(
            &path,
            &serde_json::json!({"version":1,"pair_id":"pair","share_id":"share",
            "peer_root":null,"files":{"file.tmp":old}}),
        )
        .unwrap();
        let mut state = State::load(&path, "pair", "share").unwrap();
        assert_eq!(
            local_round(&mut p, &mut a, &mut state, &path, None).transferred,
            0
        );
        assert_eq!(
            State::load(&path, "pair", "share").unwrap().legacy_files["file.tmp"],
            old
        );
        drop(p);
        drop(a);
        let mut p = LocalStore::open_with_policy(pc.path(), "").unwrap();
        let mut a = LocalStore::open_with_policy(phone.path(), "").unwrap();
        let report = local_round(&mut p, &mut a, &mut state, &path, None);
        assert_eq!(report.conflicts, 0);
        assert_eq!(report.transferred, 1);
        assert_eq!(
            std::fs::read(phone.path().join("file.tmp")).unwrap(),
            b"pc edit"
        );
    }

    #[test]
    fn delta_falls_back_when_trust_is_lost() {
        for scenario in [
            "restart",
            "token",
            "peer_token",
            "cache",
            "ignore",
            "delete",
            "rename",
            "remap",
        ] {
            let pc = tempfile::tempdir().unwrap();
            let phone = tempfile::tempdir().unwrap();
            std::fs::write(pc.path().join("a"), b"same").unwrap();
            std::fs::write(phone.path().join("a"), b"same").unwrap();
            let mut p = LocalStore::open(pc.path()).unwrap();
            let mut a = LocalStore::open(phone.path()).unwrap();
            p.allow_incremental_scan();
            a.allow_incremental_scan();
            let path = p.private().join("state.json");
            let mut state = State::load(&path, "pair", "share").unwrap();
            assert_eq!(
                local_round(&mut p, &mut a, &mut state, &path, None)
                    .metrics
                    .full_scans,
                2
            );
            match scenario {
                "restart" => {
                    session_tokens().lock().unwrap().remove(&path);
                }
                "token" => {
                    session_tokens()
                        .lock()
                        .unwrap()
                        .insert(path.clone(), "bad".into());
                }
                "peer_token" => a.set_base_token(None),
                "cache" => p.invalidate(),
                "ignore" => std::fs::write(pc.path().join(".rowdignore"), "ignored\n").unwrap(),
                "delete" => {
                    std::fs::remove_file(pc.path().join("a")).unwrap();
                    p.invalidate_path("a");
                }
                "rename" => {
                    std::fs::rename(pc.path().join("a"), pc.path().join("new")).unwrap();
                    p.invalidate_path("a");
                    p.invalidate_path("new");
                }
                _ => {}
            }
            let remap = (scenario == "remap").then_some(RemapPolicy::Compare);
            let report = local_round(&mut p, &mut a, &mut state, &path, remap);
            // Full streaming always audits both structural namespaces; physical
            // hash evidence still prevents rereading unchanged content.
            assert_eq!(report.metrics.full_scans, 2, "{scenario}");
            if matches!(scenario, "restart" | "token" | "peer_token" | "remap") {
                assert_eq!(report.metrics.files_hashed, 0, "{scenario}");
            }
        }
    }
    #[test]
    fn created_paths_use_delta_in_both_directions() {
        for (pc_bytes, android_bytes, transfers, conflicts) in [
            (Some(b"pc".as_slice()), None, 1, 0),
            (None, Some(b"android".as_slice()), 1, 0),
            (Some(b"same".as_slice()), Some(b"same".as_slice()), 0, 0),
            (Some(b"pc".as_slice()), Some(b"android".as_slice()), 3, 1),
        ] {
            let pc = tempfile::tempdir().unwrap();
            let phone = tempfile::tempdir().unwrap();
            let mut p = LocalStore::open(pc.path()).unwrap();
            let mut a = LocalStore::open(phone.path()).unwrap();
            p.allow_incremental_scan();
            a.allow_incremental_scan();
            let path = p.private().join("state.json");
            let mut state = State::load(&path, "pair", "share").unwrap();
            local_round(&mut p, &mut a, &mut state, &path, None);
            if let Some(bytes) = pc_bytes {
                std::fs::write(pc.path().join("new.txt"), bytes).unwrap();
                p.invalidate_path("new.txt");
            }
            if let Some(bytes) = android_bytes {
                std::fs::write(phone.path().join("new.txt"), bytes).unwrap();
                a.invalidate_path("new.txt");
            }
            let report = local_round(&mut p, &mut a, &mut state, &path, None);
            assert_eq!(report.metrics.full_scans, 0);
            assert_eq!(report.metrics.paths_reconciled, 1);
            assert!(report.metrics.manifest_entries <= 1);
            assert!(report.metrics.files_enumerated <= 2);
            assert_eq!(report.transferred, transfers);
            assert_eq!(report.conflicts, conflicts);
            assert_eq!(
                std::fs::read(pc.path().join("new.txt")).unwrap(),
                pc_bytes.or(android_bytes).unwrap()
            );
            if conflicts == 0 {
                assert_eq!(
                    std::fs::read(phone.path().join("new.txt")).unwrap(),
                    pc_bytes.or(android_bytes).unwrap()
                );
            } else {
                assert_eq!(std::fs::read(phone.path().join("new.txt")).unwrap(), b"pc");
                let conflict = crate::model::conflict_path(
                    "new.txt",
                    &crate::hash_reader(b"android".as_slice()).unwrap().0,
                );
                assert_eq!(
                    std::fs::read(pc.path().join(&conflict)).unwrap(),
                    b"android"
                );
                assert_eq!(
                    std::fs::read(phone.path().join(&conflict)).unwrap(),
                    b"android"
                );
            }
        }
    }
    #[test]
    fn created_case_collision_is_checked_against_the_base() {
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("A.txt"), b"base").unwrap();
        std::fs::write(phone.path().join("A.txt"), b"base").unwrap();
        let mut p = LocalStore::open(pc.path()).unwrap();
        let mut a = LocalStore::open(phone.path()).unwrap();
        p.allow_incremental_scan();
        a.allow_incremental_scan();
        let path = p.private().join("state.json");
        let mut state = State::load(&path, "pair", "share").unwrap();
        local_round(&mut p, &mut a, &mut state, &path, None);
        std::fs::write(pc.path().join("a.txt"), b"new").unwrap();
        p.invalidate_path("a.txt");
        let (mut x, mut y) = sockets();
        std::thread::scope(|scope| {
            let responder = scope.spawn(|| respond(&mut y, &mut a));
            assert!(coordinate(&mut x, &mut p, &mut state, &path)
                .unwrap_err()
                .to_string()
                .contains("case collision"));
            drop(x);
            assert!(responder.join().unwrap().is_err());
        });
        assert_eq!(std::fs::read(phone.path().join("A.txt")).unwrap(), b"base");
        assert!(!phone.path().join("a.txt").exists());
    }
    #[test]
    fn reloading_base_in_the_same_process_keeps_the_session_delta() {
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("a"), b"same").unwrap();
        std::fs::write(phone.path().join("a"), b"same").unwrap();
        let mut p = LocalStore::open(pc.path()).unwrap();
        let mut a = LocalStore::open(phone.path()).unwrap();
        p.allow_incremental_scan();
        a.allow_incremental_scan();
        let path = p.private().join("state.json");
        let mut state = State::load(&path, "pair", "share").unwrap();
        local_round(&mut p, &mut a, &mut state, &path, None);
        let mut reloaded = State::load(&path, "pair", "share").unwrap();
        assert!(reloaded.base_token.is_none());
        p.invalidate_path("a");
        let report = local_round(&mut p, &mut a, &mut reloaded, &path, None);
        assert_eq!(report.metrics.full_scans, 0);
    }
    #[test]
    fn incremental_create_does_not_cross_a_known_file_directory_boundary() {
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("a"), b"base").unwrap();
        std::fs::write(phone.path().join("a"), b"base").unwrap();
        let mut p = LocalStore::open(pc.path()).unwrap();
        let mut a = LocalStore::open(phone.path()).unwrap();
        p.allow_incremental_scan();
        a.allow_incremental_scan();
        let path = p.private().join("state.json");
        let mut state = State::load(&path, "pair", "share").unwrap();
        local_round(&mut p, &mut a, &mut state, &path, None);
        std::fs::remove_file(pc.path().join("a")).unwrap();
        std::fs::create_dir(pc.path().join("a")).unwrap();
        std::fs::write(pc.path().join("a/b.txt"), b"new").unwrap();
        p.invalidate_path("a/b.txt");
        let (mut x, mut y) = sockets();
        std::thread::scope(|scope| {
            let responder = scope.spawn(|| respond(&mut y, &mut a));
            assert!(coordinate(&mut x, &mut p, &mut state, &path)
                .unwrap_err()
                .to_string()
                .contains("file/directory collision"));
            drop(x);
            assert!(responder.join().unwrap().is_err());
        });
        assert_eq!(std::fs::read(phone.path().join("a")).unwrap(), b"base");
    }
    struct FailMessage {
        inner: UnixStream,
        needle: &'static [u8],
    }
    impl Read for FailMessage {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }
    impl Write for FailMessage {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if buf
                .windows(self.needle.len())
                .any(|part| part == self.needle)
            {
                self.inner.shutdown(std::net::Shutdown::Both)?;
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.inner.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }
    #[test]
    fn interrupted_base_token_exchange_uses_full_manifest() {
        for needle in [b"\"delta_scan\"".as_slice(), b"\"done\"".as_slice()] {
            let pc = tempfile::tempdir().unwrap();
            let phone = tempfile::tempdir().unwrap();
            std::fs::write(pc.path().join("a"), b"same").unwrap();
            std::fs::write(phone.path().join("a"), b"same").unwrap();
            let mut p = LocalStore::open(pc.path()).unwrap();
            let mut a = LocalStore::open(phone.path()).unwrap();
            p.allow_incremental_scan();
            a.allow_incremental_scan();
            let path = p.private().join("state.json");
            let mut state = State::load(&path, "pair", "share").unwrap();
            local_round(&mut p, &mut a, &mut state, &path, None);
            let (x, mut y) = sockets();
            let mut x = FailMessage { inner: x, needle };
            std::thread::scope(|scope| {
                let responder = scope.spawn(|| respond(&mut y, &mut a));
                assert!(coordinate(&mut x, &mut p, &mut state, &path).is_err());
                assert!(responder.join().unwrap().is_err());
            });
            let persisted = State::load(&path, "pair", "share").unwrap();
            assert!(persisted.base_token.is_none());
            assert_eq!(
                local_round(&mut p, &mut a, &mut state, &path, None)
                    .metrics
                    .full_scans,
                0
            );
        }
    }
    #[test]
    fn fifo_window_respects_file_and_byte_limits() {
        for to_phone in [true, false] {
            let pc = tempfile::tempdir().unwrap();
            let phone = tempfile::tempdir().unwrap();
            let source = if to_phone { pc.path() } else { phone.path() };
            for index in 0..10 {
                std::fs::write(source.join(format!("file-{index:02}")), [b'x'; 1024]).unwrap();
            }
            let mut p = LocalStore::open(pc.path()).unwrap();
            let mut a = LocalStore::open(phone.path()).unwrap();
            let path = p.private().join("state.json");
            let mut state = State::load(&path, "pair", "share").unwrap();
            let report = local_round(&mut p, &mut a, &mut state, &path, None);
            assert_eq!(report.transferred, 10);
            assert_eq!(
                report.metrics.peak_in_flight_files,
                MAX_IN_FLIGHT_FILES as u64
            );
            assert!(report.metrics.peak_staged_bytes <= MAX_STAGED_BYTES);
            assert_eq!(report.metrics.state_persist_count, 1);
            assert_eq!(p.scan().unwrap(), a.scan().unwrap());
        }
    }
    #[test]
    fn oversized_file_is_serial() {
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        std::fs::write(
            pc.path().join("large"),
            vec![b'x'; MAX_STAGED_BYTES as usize + 1],
        )
        .unwrap();
        let mut p = LocalStore::open(pc.path()).unwrap();
        let mut a = LocalStore::open(phone.path()).unwrap();
        let path = p.private().join("state.json");
        let mut state = State::load(&path, "pair", "share").unwrap();
        let report = local_round(&mut p, &mut a, &mut state, &path, None);
        assert_eq!(report.transferred, 1);
        assert_eq!(report.metrics.peak_in_flight_files, 0);
        assert_eq!(p.scan().unwrap(), a.scan().unwrap());
    }
    #[test]
    fn fifo_window_applies_byte_backpressure() {
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        for index in 0..5 {
            std::fs::write(
                pc.path().join(format!("file-{index}")),
                vec![b'x'; 3 * 1024 * 1024],
            )
            .unwrap();
        }
        let mut p = LocalStore::open(pc.path()).unwrap();
        let mut a = LocalStore::open(phone.path()).unwrap();
        let path = p.private().join("state.json");
        let mut state = State::load(&path, "pair", "share").unwrap();
        let report = local_round(&mut p, &mut a, &mut state, &path, None);
        assert_eq!(report.transferred, 5);
        assert_eq!(report.metrics.peak_in_flight_files, 2);
        assert_eq!(report.metrics.peak_staged_bytes, 6 * 1024 * 1024);
    }
    struct FailSecond {
        inner: LocalStore,
        installs: usize,
        snapshots: usize,
        fail_install: bool,
    }
    impl Store for FailSecond {
        fn scan(&mut self) -> Result<crate::model::Manifest> {
            self.inner.scan()
        }
        fn snapshot(&mut self, path: &str, entry: &Entry) -> Result<Snapshot> {
            self.snapshots += 1;
            if !self.fail_install && self.snapshots == 2 {
                anyhow::bail!("injected snapshot failure")
            }
            self.inner.snapshot(path, entry)
        }
        fn install(
            &mut self,
            path: &str,
            expected: Option<&str>,
            entry: &Entry,
            staged: &VerifiedStaged,
        ) -> Result<()> {
            self.installs += 1;
            if self.fail_install && self.installs == 2 {
                anyhow::bail!("injected install failure")
            }
            self.inner.install(path, expected, entry, staged)
        }
    }
    #[test]
    fn interrupted_fifo_batch_recovers_in_both_directions() {
        for to_phone in [true, false] {
            let pc = tempfile::tempdir().unwrap();
            let phone = tempfile::tempdir().unwrap();
            let source = if to_phone { pc.path() } else { phone.path() };
            for name in ["a", "b", "c"] {
                std::fs::write(source.join(name), name).unwrap();
            }
            let mut p = LocalStore::open(pc.path()).unwrap();
            let mut a = FailSecond {
                inner: LocalStore::open(phone.path()).unwrap(),
                installs: 0,
                snapshots: 0,
                fail_install: to_phone,
            };
            let path = p.private().join("state.json");
            let mut state = State::load(&path, "pair", "share").unwrap();
            let (mut x, mut y) = sockets();
            std::thread::scope(|scope| {
                let responder = scope.spawn(|| respond(&mut y, &mut a));
                let remote = coordinate(&mut x, &mut p, &mut state, &path).unwrap_err();
                let detail = remote
                    .downcast_ref::<protocol::ShareError>()
                    .expect("cause must reach the PC, not EOF");
                assert_eq!(detail.kind, "filesystem");
                assert_eq!(
                    detail.operation,
                    if to_phone { "install" } else { "snapshot" }
                );
                assert_eq!(detail.relative_path.as_deref(), Some("b"));
                assert!(!detail.stream_reusable);
                assert!(responder.join().unwrap().is_err());
            });
            if to_phone {
                assert_eq!(std::fs::read(phone.path().join("a")).unwrap(), b"a");
                assert!(!phone.path().join("b").exists());
            } else {
                assert!(!pc.path().join("a").exists());
            }
            let mut phone_store = a.inner;
            let report = local_round(&mut p, &mut phone_store, &mut state, &path, None);
            assert!(report.transferred > 0);
            assert_eq!(p.scan().unwrap(), phone_store.scan().unwrap());
        }
    }
    #[test]
    fn conflict_drains_fifo_window() {
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(pc.path().join(name), b"base").unwrap();
            std::fs::write(phone.path().join(name), b"base").unwrap();
        }
        let mut p = LocalStore::open(pc.path()).unwrap();
        let mut a = LocalStore::open(phone.path()).unwrap();
        let path = p.private().join("state.json");
        let mut state = State::load(&path, "pair", "share").unwrap();
        local_round(&mut p, &mut a, &mut state, &path, None);
        for name in ["a", "b", "c"] {
            std::fs::write(pc.path().join(name), b"pc").unwrap();
        }
        std::fs::write(phone.path().join("b"), b"phone").unwrap();
        let report = local_round(&mut p, &mut a, &mut state, &path, None);
        assert_eq!(report.conflicts, 1);
        assert_eq!(report.transferred, 5);
        // Streaming may stage the two independent files before the deferred conflict.
        assert!(report.metrics.peak_in_flight_files <= MAX_IN_FLIGHT_FILES as u64);
        assert_eq!(p.scan().unwrap(), a.scan().unwrap());
    }
    #[test]
    #[ignore = "scale baseline; run with ROWD_BENCH_FILES=50000 --ignored --nocapture"]
    fn large_share_one_small_change_baseline() {
        let count = std::env::var("ROWD_BENCH_FILES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(10_000);
        let change_bytes = std::env::var("ROWD_BENCH_CHANGE_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(4096);
        let to_phone = std::env::var("ROWD_BENCH_DIRECTION").as_deref() != Ok("android_to_pc");
        let create = std::env::var("ROWD_BENCH_CREATE").as_deref() == Ok("1");
        benchmark_scenario(count, change_bytes, to_phone, create, false);
    }

    fn benchmark_scenario(
        count: usize,
        change_bytes: usize,
        to_phone: bool,
        create: bool,
        force_full: bool,
    ) {
        assert!((1..=50_000).contains(&count));
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        let (empty_hash, _) = hash_reader(b"".as_slice()).unwrap();
        let mut files = BTreeMap::new();
        for index in 0..count {
            let name = format!("file-{index:05}.bin");
            std::fs::write(pc.path().join(&name), b"").unwrap();
            std::fs::write(phone.path().join(&name), b"").unwrap();
            files.insert(name, empty_hash.clone());
        }
        // Prime the verified hash caches; the measured round is the common warm case.
        {
            let mut store = LocalStore::open(pc.path()).unwrap();
            store.scan().unwrap();
        }
        {
            let mut store = LocalStore::open(phone.path()).unwrap();
            store.scan().unwrap();
        }
        let mut pc_store = LocalStore::open(pc.path()).unwrap();
        pc_store.allow_incremental_scan();
        let mut phone_store = LocalStore::open(phone.path()).unwrap();
        phone_store.allow_incremental_scan();
        let state_path = pc_store.private().join("state.json");
        let mut state = State {
            blake3: true,
            legacy_files: BTreeMap::new(),
            conflicts: BTreeSet::new(),
            last_sync: None,
            version: VERSION,
            pair_id: "pair".into(),
            share_id: "share".into(),
            peer_root: None,
            files,
            base_token: None,
        };
        let (mut first_pc, mut first_phone) = sockets();
        std::thread::scope(|scope| {
            let responder = scope.spawn(|| respond(&mut first_phone, &mut phone_store));
            coordinate(&mut first_pc, &mut pc_store, &mut state, &state_path).unwrap();
            responder.join().unwrap().unwrap();
        });
        drop(pc_store);
        let event_started = Instant::now();
        if change_bytes > 0 {
            let changed = if to_phone { pc.path() } else { phone.path() };
            let name = if create { "new.bin" } else { "file-00000.bin" };
            std::fs::write(changed.join(name), vec![b'x'; change_bytes]).unwrap();
            if to_phone {
                LocalStore::queue_cache_invalidation(
                    &pc.path().canonicalize().unwrap(),
                    false,
                    Some(&BTreeSet::from([name.into()])),
                )
                .unwrap();
            } else {
                phone_store.invalidate_path(name);
            }
        }
        let event_ms = event_started.elapsed().as_millis();
        let mut pc_store = LocalStore::open(pc.path()).unwrap();
        pc_store.allow_incremental_scan();
        if force_full {
            if to_phone {
                pc_store.invalidate();
            } else {
                phone_store.invalidate();
            }
        }
        let (mut x, mut y) = sockets();
        for socket in [&x, &y] {
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(300)))
                .unwrap();
            socket
                .set_write_timeout(Some(std::time::Duration::from_secs(300)))
                .unwrap();
        }
        std::thread::scope(|scope| {
            let responder = scope.spawn(|| respond(&mut y, &mut phone_store));
            let mut progress_events = 0;
            let report = coordinate_with_progress(
                &mut x,
                &mut pc_store,
                &mut state,
                &state_path,
                SyncMode::Bidirectional,
                None,
                |_, _, _| progress_events += 1,
            )
            .unwrap();
            responder.join().unwrap().unwrap();
            let changed = u64::from(change_bytes > 0);
            assert_eq!(report.transferred as u64, changed);
            assert_eq!(report.metrics.bytes_transferred, change_bytes as u64);
            if force_full {
                assert_eq!(report.metrics.full_scans, 2);
                assert_eq!(report.metrics.paths_reconciled, count as u64 + changed);
                assert!(report.metrics.manifest_entries >= count as u64);
                assert!(report.metrics.files_enumerated >= 2 * count as u64);
            } else {
                assert_eq!(
                    report.metrics.files_enumerated,
                    changed * if create { 1 } else { 2 }
                );
                assert_eq!(
                    report.metrics.manifest_entries,
                    changed * u64::from(!to_phone || !create)
                );
                assert_eq!(report.metrics.paths_reconciled, changed);
                assert_eq!(
                    report.metrics.ack_entries,
                    if to_phone { 0 } else { changed }
                );
                assert_eq!(report.metrics.full_scans, 0);
            }
            assert_eq!(progress_events, changed as usize + 1);
            eprintln!(
                "files={count} create={create} force_full={force_full} change_bytes={change_bytes} direction={} event_ms={event_ms} progress_events={progress_events} metrics={}",
                if to_phone { "pc_to_android" } else { "android_to_pc" },
                serde_json::to_string(&report.metrics).unwrap()
            );
        });
    }

    #[test]
    #[ignore = "benchmark matrix; run --ignored --nocapture"]
    fn benchmark_matrix() {
        for (files, bytes, to_phone) in [
            (1, 0, true),
            (1, 4096, true),
            (1, 16 * 1024 * 1024, true),
            (10_000, 4096, true),
            (50_000, 4096, true),
            (10_000, 4096, false),
            (50_000, 4096, false),
        ] {
            benchmark_scenario(files, bytes, to_phone, false, false);
        }
        for files in [1, 10_000, 50_000] {
            for to_phone in [true, false] {
                benchmark_scenario(files, 4096, to_phone, true, false);
            }
        }
    }
    #[test]
    #[ignore = "simulated old CREATE full path; run --ignored --nocapture"]
    fn benchmark_create_full_baseline() {
        for files in [1, 10_000, 50_000] {
            for to_phone in [true, false] {
                benchmark_scenario(files, 4096, to_phone, true, true);
            }
        }
    }
    #[test]
    #[cfg(unix)]
    fn unchanged_paths_emit_only_completion_progress() {
        use crate::storage::LocalStore;
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("same"), b"content").unwrap();
        std::fs::write(phone.path().join("same"), b"content").unwrap();
        let mut pc_store = LocalStore::open(pc.path()).unwrap();
        let mut phone_store = LocalStore::open(phone.path()).unwrap();
        let entry = pc_store.scan().unwrap()["same"].clone();
        let state_path = pc_store.private().join("state.json");
        let mut state = State::load(&state_path, "pair", "share").unwrap();
        state.files.insert("same".into(), entry.hash);
        let (mut x, mut y) = sockets();
        let mut events = Vec::new();
        std::thread::scope(|scope| {
            let responder = scope.spawn(|| respond(&mut y, &mut phone_store));
            coordinate_with_progress(
                &mut x,
                &mut pc_store,
                &mut state,
                &state_path,
                SyncMode::Bidirectional,
                None,
                |path, done, total| events.push((path.to_owned(), done, total)),
            )
            .unwrap();
            responder.join().unwrap().unwrap();
        });
        assert_eq!(events, vec![(String::new(), 1, 1)]);
    }
    #[test]
    fn remap_policy_bootstraps_content_without_overriding_steady_state_mode() {
        for (policy, expected, conflicts) in [
            (RemapPolicy::Pc, b"pc".as_slice(), 0),
            (RemapPolicy::Android, b"android".as_slice(), 0),
            (RemapPolicy::Compare, b"pc".as_slice(), 1),
        ] {
            let pc = tempfile::tempdir().unwrap();
            let phone = tempfile::tempdir().unwrap();
            std::fs::write(pc.path().join("both"), b"pc").unwrap();
            std::fs::write(phone.path().join("both"), b"android").unwrap();
            let mut p = LocalStore::open(pc.path()).unwrap();
            let mut a = LocalStore::open(phone.path()).unwrap();
            let file = p.private().join("state.json");
            let mut state = State::load(&file, "pair", "share").unwrap();
            let (mut x, mut y) = sockets();
            std::thread::scope(|scope| {
                let responder = scope.spawn(|| respond(&mut y, &mut a));
                let report = coordinate_with_progress(
                    &mut x,
                    &mut p,
                    &mut state,
                    &file,
                    SyncMode::ToPc,
                    Some(policy),
                    |_, _, _| {},
                )
                .unwrap();
                assert_eq!(report.conflicts, conflicts);
                responder.join().unwrap().unwrap();
            });
            assert_eq!(std::fs::read(pc.path().join("both")).unwrap(), expected);
            assert_eq!(std::fs::read(phone.path().join("both")).unwrap(), expected);
            if policy == RemapPolicy::Compare {
                assert!(pc.path().join("Rowd Conflicts").is_dir());
                assert!(phone.path().join("Rowd Conflicts").is_dir());
            }
        }
    }
    #[test]
    fn directional_modes_preserve_prohibited_edits_and_resume() {
        for mode in [SyncMode::Bidirectional, SyncMode::ToPc, SyncMode::ToAndroid] {
            let pc = tempfile::tempdir().unwrap();
            let phone = tempfile::tempdir().unwrap();
            std::fs::write(pc.path().join("pc-only"), b"pc").unwrap();
            std::fs::write(phone.path().join("phone-only"), b"phone").unwrap();
            let mut p = LocalStore::open(pc.path()).unwrap();
            let mut a = LocalStore::open(phone.path()).unwrap();
            let file = p.private().join("state.json");
            let mut state = State::load(&file, "pair", "share").unwrap();
            for step in 0..3 {
                if step == 1 {
                    std::fs::write(pc.path().join("both"), b"pc edit").unwrap();
                    std::fs::write(phone.path().join("both"), b"phone edit").unwrap();
                }
                let (mut x, mut y) = sockets();
                std::thread::scope(|scope| {
                    let client = scope.spawn(|| respond(&mut y, &mut a));
                    coordinate_mode(&mut x, &mut p, &mut state, &file, mode).unwrap();
                    client.join().unwrap().unwrap();
                });
                // Reload at each boundary, including the conflicting round.
                state = State::load(&file, "pair", "share").unwrap();
            }
            if mode == SyncMode::Bidirectional {
                assert_eq!(p.scan().unwrap(), a.scan().unwrap());
            } else {
                assert_eq!(std::fs::read(pc.path().join("both")).unwrap(), b"pc edit");
                assert_eq!(
                    std::fs::read(phone.path().join("both")).unwrap(),
                    b"phone edit"
                );
                assert!(state.conflicts.contains("both"));
                if mode == SyncMode::ToPc {
                    assert!(!phone.path().join("pc-only").exists());
                    assert!(pc.path().join("phone-only").exists());
                } else {
                    assert!(!pc.path().join("phone-only").exists());
                    assert!(phone.path().join("pc-only").exists());
                }
            }
        }
    }
    struct DisconnectAfterInstall {
        inner: LocalStore,
        fail: Arc<AtomicBool>,
    }
    impl Store for DisconnectAfterInstall {
        fn scan(&mut self) -> Result<crate::model::Manifest> {
            self.inner.scan()
        }
        fn snapshot(&mut self, p: &str, e: &Entry) -> Result<Snapshot> {
            self.inner.snapshot(p, e)
        }
        fn install(
            &mut self,
            p: &str,
            x: Option<&str>,
            e: &Entry,
            s: &VerifiedStaged,
        ) -> Result<()> {
            self.inner.install(p, x, e, s)?;
            self.fail.store(true, Ordering::SeqCst);
            Ok(())
        }
    }
    struct LostAck {
        inner: UnixStream,
        fail: Arc<AtomicBool>,
    }
    impl Read for LostAck {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(b)
        }
    }
    impl Write for LostAck {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.fail.load(Ordering::SeqCst) {
                self.inner.shutdown(std::net::Shutdown::Both)?;
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.inner.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }
    #[test]
    fn lost_ack_replays_from_physical_state_and_base() {
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("a"), b"latest").unwrap();
        let base = pc.path().join(".rowd/state.json");
        let mut p = LocalStore::open(pc.path()).unwrap();
        let fail = Arc::new(AtomicBool::new(false));
        let mut a = DisconnectAfterInstall {
            inner: LocalStore::open(phone.path()).unwrap(),
            fail: fail.clone(),
        };
        let mut state = State::load(&base, "pair", "share").unwrap();
        let (mut x, y) = sockets();
        let mut y = LostAck { inner: y, fail };
        std::thread::scope(|s| {
            let client = s.spawn(|| respond(&mut y, &mut a));
            assert!(coordinate(&mut x, &mut p, &mut state, &base).is_err());
            assert!(client.join().unwrap().is_err());
        });
        drop(p);
        assert!(state.files.is_empty());
        let mut p = LocalStore::open(pc.path()).unwrap();
        let (mut x, mut y) = sockets();
        std::thread::scope(|s| {
            let client = s.spawn(|| respond(&mut y, &mut a.inner));
            assert_eq!(
                coordinate(&mut x, &mut p, &mut state, &base)
                    .unwrap()
                    .transferred,
                0
            );
            client.join().unwrap().unwrap();
        });
        assert_eq!(
            state.files["a"],
            crate::hash_reader(b"latest".as_slice()).unwrap().0
        );
        assert_eq!(p.scan().unwrap(), a.scan().unwrap());
    }
}

#[cfg(test)]
mod scan_resilience_tests {
    use super::*;
    use crate::model::Manifest;
    use std::{collections::VecDeque, io::Cursor};

    enum Step {
        Idle(u64),
        Frame(Vec<u8>),
    }
    struct Peer<'a> {
        clock: &'a std::sync::atomic::AtomicU64,
        steps: VecDeque<Step>,
        frame: Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }
    impl Read for Peer<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.frame.position() < self.frame.get_ref().len() as u64 {
                return self.frame.read(buf);
            }
            match self.steps.pop_front() {
                Some(Step::Frame(bytes)) => {
                    self.frame = Cursor::new(bytes);
                    self.frame.read(buf)
                }
                step => {
                    let seconds = match step {
                        Some(Step::Idle(s)) => s,
                        _ => 30,
                    };
                    self.clock
                        .fetch_add(seconds, std::sync::atomic::Ordering::Relaxed);
                    Err(std::io::ErrorKind::WouldBlock.into())
                }
            }
        }
    }
    impl Write for Peer<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.sent.extend(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn frame(message: Message) -> Step {
        let mut bytes = vec![];
        protocol::send_for(&mut bytes, "share", message).unwrap();
        Step::Frame(bytes)
    }
    fn peer(clock: &std::sync::atomic::AtomicU64, steps: Vec<Step>) -> Peer<'_> {
        Peer {
            clock,
            steps: steps.into(),
            frame: Cursor::new(vec![]),
            sent: vec![],
        }
    }
    #[test]
    fn scan_over_ninety_seconds_then_manifest_and_second_round_stay_aligned() {
        let clock = std::sync::atomic::AtomicU64::new(0);
        let epoch = Instant::now();
        let mut steps = vec![];
        for _ in 0..12 {
            steps.push(Step::Idle(30));
            steps.push(frame(Message::ScanAlive));
        }
        steps.push(frame(Message::ScanReady));
        steps.push(frame(Message::ManifestBegin { count: 0 }));
        steps.push(frame(Message::ManifestEnd {
            metrics: Default::default(),
        }));
        steps.push(frame(Message::Done {
            transferred: 0,
            conflicts: 0,
            base_token: "a".repeat(64),
        }));
        steps.push(frame(Message::ScanAlive));
        steps.push(frame(Message::ScanReady));
        let mut io = peer(&clock, steps);
        assert_eq!(
            receive_scan_gate_with_clock(
                &mut io,
                "share",
                &mut |_| None,
                || epoch + Duration::from_secs(clock.load(std::sync::atomic::Ordering::Relaxed)),
                Duration::from_secs(90)
            )
            .unwrap(),
            None
        );
        assert_eq!(clock.load(std::sync::atomic::Ordering::Relaxed), 360);
        assert!(protocol::receive_manifest(&mut io, "share")
            .unwrap()
            .is_empty());
        assert!(matches!(
            protocol::receive_for(&mut io, "share").unwrap(),
            Message::Done { .. }
        ));
        assert_eq!(
            receive_scan_gate_with_clock(
                &mut io,
                "share",
                &mut |_| None,
                || epoch + Duration::from_secs(clock.load(std::sync::atomic::Ordering::Relaxed)),
                Duration::from_secs(90)
            )
            .unwrap(),
            None
        );
        let mut sent = Cursor::new(io.sent);
        for _ in 0..2 {
            assert!(matches!(
                protocol::receive_for(&mut sent, "share").unwrap(),
                Message::ScanContinue
            ));
        }
        assert_eq!(sent.position(), sent.get_ref().len() as u64);
    }
    #[test]
    fn dead_peer_expires_after_inactivity_even_after_previous_liveness() {
        let clock = std::sync::atomic::AtomicU64::new(0);
        let epoch = Instant::now();
        let mut io = peer(&clock, vec![Step::Idle(60), frame(Message::ScanAlive)]);
        let error = receive_scan_gate_with_clock(
            &mut io,
            "share",
            &mut |_| None,
            || epoch + Duration::from_secs(clock.load(std::sync::atomic::Ordering::Relaxed)),
            Duration::from_secs(90),
        )
        .unwrap_err();
        assert_eq!(clock.load(std::sync::atomic::Ordering::Relaxed), 150);
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert!(protocol::transport_dead(&error));
        assert!(error.to_string().contains("liveness timeout"));
    }
    #[test]
    fn slow_delta_gate_keeps_response_and_next_gate_aligned() {
        let clock = std::sync::atomic::AtomicU64::new(0);
        let epoch = Instant::now();
        let mut steps = vec![];
        for _ in 0..12 {
            steps.push(Step::Idle(30));
            steps.push(frame(Message::ScanAlive));
        }
        steps.push(frame(Message::ScanReady));
        steps.push(frame(Message::DeltaManifest {
            paths: BTreeSet::from(["file".into()]),
            files: Manifest::new(),
            metrics: Default::default(),
        }));
        steps.push(frame(Message::ScanReady));
        let mut io = peer(&clock, steps);
        assert_eq!(
            receive_scan_gate_with_clock(
                &mut io,
                "share",
                &mut |_| None,
                || epoch + Duration::from_secs(clock.load(std::sync::atomic::Ordering::Relaxed)),
                Duration::from_secs(90)
            )
            .unwrap(),
            None
        );
        assert_eq!(clock.load(std::sync::atomic::Ordering::Relaxed), 360);
        assert!(matches!(
            protocol::receive_for(&mut io, "share").unwrap(),
            Message::DeltaManifest { .. }
        ));
        assert_eq!(
            receive_scan_gate(&mut io, "share", &mut |_| None).unwrap(),
            None
        );
        let mut sent = Cursor::new(io.sent);
        for _ in 0..2 {
            assert!(matches!(
                protocol::receive_for(&mut sent, "share").unwrap(),
                Message::ScanContinue
            ));
        }
        assert_eq!(sent.position(), sent.get_ref().len() as u64);
    }
    #[test]
    fn delta_responder_gates_success_fallback_and_cancellation() {
        struct DeltaStore {
            inner: crate::storage::LocalStore,
            mode: u8,
            calls: usize,
            discarded: bool,
        }
        impl Store for DeltaStore {
            fn base_token(&self) -> Option<String> {
                self.inner.base_token()
            }
            fn set_base_token(&mut self, token: Option<String>) {
                self.inner.set_base_token(token)
            }
            fn delta_scan_with_control(
                &mut self,
                io: &mut (impl Read + Write),
                paths: &BTreeSet<String>,
            ) -> Result<Option<(BTreeSet<String>, Manifest)>> {
                self.calls += 1;
                protocol::send_for(io, "share", Message::ScanAlive)?;
                match self.mode {
                    1 => Ok(None),
                    2 => Err(ScanDeferred.into()),
                    _ => Ok(Some((paths.clone(), Manifest::new()))),
                }
            }
            fn discard_scan(&mut self) -> Result<()> {
                self.discarded = true;
                Ok(())
            }
            fn scan(&mut self) -> Result<Manifest> {
                self.inner.scan()
            }
            fn snapshot(&mut self, path: &str, entry: &Entry) -> Result<Snapshot> {
                self.inner.snapshot(path, entry)
            }
            fn install(
                &mut self,
                path: &str,
                expected: Option<&str>,
                entry: &Entry,
                staged: &Snapshot,
            ) -> Result<()> {
                self.inner.install(path, expected, entry, staged)
            }
        }
        for mode in 0..4 {
            let directory = tempfile::tempdir().unwrap();
            let mut store = DeltaStore {
                inner: crate::storage::LocalStore::open(directory.path()).unwrap(),
                mode,
                calls: 0,
                discarded: false,
            };
            store.set_base_token(Some("a".repeat(64)));
            let clock = std::sync::atomic::AtomicU64::new(0);
            let mut steps = vec![frame(Message::DeltaScan {
                base_token: "a".repeat(64),
                paths: BTreeSet::from(["file".into()]),
            })];
            if mode != 2 {
                steps.push(frame(if mode == 3 {
                    Message::AuditPreempt {
                        shares: vec!["b".repeat(64)],
                    }
                } else {
                    Message::ScanContinue
                }));
            }
            if mode < 2 {
                steps.push(frame(Message::Done {
                    transferred: 0,
                    conflicts: 0,
                    base_token: "a".repeat(64),
                }));
            }
            let mut io = peer(&clock, steps);
            let report = respond_share(&mut io, &mut store, Some("share")).unwrap();
            assert_eq!(report.round_deferred, mode >= 2);
            assert_eq!(store.calls, 1);
            assert!(store.discarded);
            let mut sent = Cursor::new(io.sent);
            assert!(matches!(
                protocol::receive_for(&mut sent, "share").unwrap(),
                Message::ScanAlive
            ));
            if mode != 2 {
                assert!(matches!(
                    protocol::receive_for(&mut sent, "share").unwrap(),
                    Message::ScanReady
                ));
            }
            let response = protocol::receive_for(&mut sent, "share").unwrap();
            assert!(match mode {
                0 => matches!(response, Message::DeltaManifest { .. }),
                1 => matches!(response, Message::NeedFullScan),
                _ => matches!(response, Message::ScanDeferred),
            });
            assert_eq!(sent.position(), sent.get_ref().len() as u64);
        }
        #[cfg(unix)]
        {
            let pc = tempfile::tempdir().unwrap();
            let phone = tempfile::tempdir().unwrap();
            let mut local = crate::storage::LocalStore::open(pc.path()).unwrap();
            local.allow_incremental_scan();
            let mut remote = DeltaStore {
                inner: crate::storage::LocalStore::open(phone.path()).unwrap(),
                mode: 0,
                calls: 0,
                discarded: false,
            };
            let path = local.private().join("state.json");
            let mut state = State::load(&path, "pair", "share").unwrap();
            let (mut sender, mut receiver) = std::os::unix::net::UnixStream::pair().unwrap();
            for socket in [&sender, &receiver] {
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
            }
            // Establish a base, defer an incremental round, then reuse the same connection.
            for round in 0..3 {
                remote.mode = if round == 1 { 2 } else { 0 };
                let prior = serde_json::to_value(&state).unwrap();
                let prior_disk = std::fs::read(&path).ok();
                std::thread::scope(|scope| {
                    let worker =
                        scope.spawn(|| respond_share(&mut receiver, &mut remote, Some("share")));
                    let report = coordinate(&mut sender, &mut local, &mut state, &path).unwrap();
                    assert_eq!(report.round_deferred, round == 1);
                    assert_eq!(worker.join().unwrap().unwrap().round_deferred, round == 1);
                });
                if round == 1 {
                    assert_eq!(serde_json::to_value(&state).unwrap(), prior);
                    assert_eq!(std::fs::read(&path).ok(), prior_disk);
                }
            }
            assert_eq!(remote.calls, 2);
        }
    }
    #[test]
    fn audit_preempt_drains_prior_liveness_and_defers_without_scan_continue() {
        let clock = std::sync::atomic::AtomicU64::new(0);
        let epoch = Instant::now();
        let mut io = peer(
            &clock,
            vec![
                Step::Idle(1),
                frame(Message::ScanAlive),
                frame(Message::ScanDeferred),
            ],
        );
        let wanted = vec!["a".repeat(64)];
        assert_eq!(
            receive_scan_gate_with_clock(
                &mut io,
                "share",
                &mut |_| Some(wanted.clone()),
                || epoch + Duration::from_secs(clock.load(std::sync::atomic::Ordering::Relaxed)),
                Duration::from_secs(90)
            )
            .unwrap(),
            Some(wanted.clone())
        );
        let mut sent = Cursor::new(io.sent);
        match protocol::receive_for(&mut sent, "share").unwrap() {
            Message::AuditPreempt { shares } => assert_eq!(shares, wanted),
            other => panic!("{other:?}"),
        }
        assert_eq!(sent.position(), sent.get_ref().len() as u64);
    }
    #[test]
    fn responder_snapshot_failure_reaches_peer_with_cause_share_and_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), b"content").unwrap();
        let mut store = crate::storage::LocalStore::open(dir.path()).unwrap();
        let entry = store.scan().unwrap()["file"].clone();
        std::fs::remove_file(dir.path().join("file")).unwrap();
        let clock = std::sync::atomic::AtomicU64::new(0);
        let mut io = peer(
            &clock,
            vec![frame(Message::Get {
                path: "file".into(),
                entry,
            })],
        );
        let local = respond_share(&mut io, &mut store, Some("share")).unwrap_err();
        let remote = protocol::receive(&mut Cursor::new(io.sent)).unwrap_err();
        let error = remote.downcast_ref::<protocol::ShareError>().unwrap();
        assert_eq!(error.operation, "snapshot");
        assert_eq!(error.relative_path.as_deref(), Some("file"));
        assert_eq!(error.share_id.as_deref(), Some("share"));
        assert_eq!(error.kind, "filesystem");
        assert!(error.message.contains(&local.to_string()));
        assert!(!error.stream_reusable);
    }
    #[test]
    fn peer_error_is_not_reflected_by_responder() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = crate::storage::LocalStore::open(directory.path()).unwrap();
        let clock = std::sync::atomic::AtomicU64::new(0);
        let mut io = peer(
            &clock,
            vec![frame(Message::Error {
                message: "pc failed".into(),
            })],
        );
        let error = respond_share(&mut io, &mut store, Some("share")).unwrap_err();
        assert!(protocol::is_peer_error(&error));
        assert!(io.sent.is_empty());
    }
    #[test]
    #[cfg(unix)]
    fn liveness_rounds_reuse_connection_and_confirmed_files_survive_reconnect() {
        use crate::storage::{LocalStore, Store};
        struct AliveStore {
            inner: LocalStore,
            acknowledged: usize,
            snapshots: usize,
        }
        impl Store for AliveStore {
            fn scan(&mut self) -> Result<Manifest> {
                self.inner.scan()
            }
            fn scan_with_control(&mut self, io: &mut (impl Read + Write)) -> Result<Manifest> {
                for _ in 0..25 {
                    protocol::send_for(io, "share", Message::ScanAlive)?;
                }
                self.scan()
            }
            fn snapshot(&mut self, path: &str, entry: &Entry) -> Result<Snapshot> {
                self.snapshots += 1;
                self.inner.snapshot(path, entry)
            }
            fn install(
                &mut self,
                path: &str,
                expected: Option<&str>,
                entry: &Entry,
                staged: &Snapshot,
            ) -> Result<()> {
                self.inner.install(path, expected, entry, staged)
            }
            fn acknowledge(&mut self, path: &str, entry: &Entry) -> Result<()> {
                self.inner.acknowledge(path, entry)?;
                self.acknowledged += 1;
                Ok(())
            }
        }
        let pc = tempfile::tempdir().unwrap();
        let android = tempfile::tempdir().unwrap();
        for index in 0..9 {
            std::fs::write(
                android.path().join(format!("{index}.png")),
                format!("content-{index}"),
            )
            .unwrap();
        }
        let mut local = LocalStore::open(pc.path()).unwrap();
        let mut remote = AliveStore {
            inner: LocalStore::open(android.path()).unwrap(),
            acknowledged: 0,
            snapshots: 0,
        };
        let state_path = local.private().join("state.json");
        let mut state = State::load(&state_path, "pair", "share").unwrap();
        let (mut sender, mut receiver) = std::os::unix::net::UnixStream::pair().unwrap();
        for round in 0..2 {
            std::thread::scope(|scope| {
                let worker =
                    scope.spawn(|| respond_share(&mut receiver, &mut remote, Some("share")));
                let report = coordinate(&mut sender, &mut local, &mut state, &state_path).unwrap();
                assert_eq!(report.transferred, if round == 0 { 9 } else { 0 });
                worker.join().unwrap().unwrap();
            });
        }
        // Full reconciliation reaffirms equal files via ACK, without another Get/Blob.
        assert_eq!(remote.acknowledged, 18);
        assert_eq!(remote.snapshots, 9);
        drop(sender);
        drop(receiver);
        state = State::load(&state_path, "pair", "share").unwrap();
        let (mut sender, mut receiver) = std::os::unix::net::UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| respond_share(&mut receiver, &mut remote, Some("share")));
            assert_eq!(
                coordinate(&mut sender, &mut local, &mut state, &state_path)
                    .unwrap()
                    .transferred,
                0
            );
            worker.join().unwrap().unwrap();
        });
        assert_eq!(remote.acknowledged, 27);
        assert_eq!(remote.snapshots, 9);
    }
}
