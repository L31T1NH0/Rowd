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
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::Path,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::Instant,
};
use tempfile::NamedTempFile;

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
}

#[derive(Serialize, Deserialize)]
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
    #[serde(default, skip_serializing)]
    pub base_token: Option<String>,
}
impl State {
    pub fn load(path: &Path, pair_id: &str, share_id: &str) -> Result<Self> {
        let state: Self = if path.try_exists()? {
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
                base_token: None,
            }
        };
        ensure!(
            state.version == VERSION && state.pair_id == pair_id && state.share_id == share_id,
            "state belongs to another Share/pair"
        );
        Ok(state)
    }
}

fn session_tokens() -> &'static Mutex<BTreeMap<PathBuf, String>> {
    static TOKENS: OnceLock<Mutex<BTreeMap<PathBuf, String>>> = OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn persist_state(path: &Path, state: &State, metrics: &mut ShareMetrics) -> Result<()> {
    let started = Instant::now();
    let bytes = serde_json::to_vec(state)?;
    atomic_write(path, &bytes)?;
    metrics.state_persist_ms += started.elapsed().as_millis();
    metrics.state_persist_count += 1;
    metrics.state_bytes_written += bytes.len() as u64;
    Ok(())
}

fn receive_blob(io: &mut impl Read, entry: &Entry) -> Result<Snapshot> {
    let mut temp = NamedTempFile::new()?;
    anyhow::ensure!(entry.size <= crate::model::MAX_FILE, "file too large");
    let (hash, size) = crate::copy_and_hash(io.take(entry.size), &mut temp)?;
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
) -> Result<Snapshot> {
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
    ensure!(actual == *entry, "STALE_SOURCE");
    first_byte_ms.get_or_insert_with(|| round_started.elapsed().as_millis());
    receive_blob(io, entry)
}
fn remote_install(
    share_id: &str,
    io: &mut (impl Read + Write),
    path: &str,
    expected: Option<&str>,
    entry: &Entry,
    staged: &Snapshot,
) -> Result<()> {
    ensure!(
        staged.entry() == entry,
        "staged entry does not match transfer"
    );
    protocol::send_for(
        io,
        share_id,
        Message::Put {
            path: path.into(),
            expected: expected.map(String::from),
            entry: entry.clone(),
        },
    )?;
    protocol::copy_exact(&mut File::open(staged.path())?, io, entry.size)?;
    ensure!(
        matches!(protocol::receive_for(io, share_id)?, Message::Accept),
        "expected file confirmation"
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

struct PendingGet {
    path: String,
    entry: Entry,
    expected: Option<String>,
}

fn drain_puts(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    state_path: &Path,
    pending: &mut Vec<PendingPut>,
    report: &mut Report,
) -> Result<()> {
    for job in pending.drain(..) {
        let waiting = Instant::now();
        ensure!(
            matches!(protocol::receive_for(io, &state.share_id)?, Message::Accept),
            "expected file confirmation"
        );
        report.metrics.wait_peer_ms += waiting.elapsed().as_millis();
        state.files.insert(job.path.clone(), job.entry.hash.clone());
        store.acknowledge(&job.path, &job.entry)?;
        persist_state(state_path, state, &mut report.metrics)?;
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
    state_path: &Path,
    pending: &mut Vec<PendingGet>,
    ack_batch: &mut crate::model::Manifest,
    report: &mut Report,
    round_started: Instant,
) -> Result<()> {
    let mut staged = Vec::with_capacity(pending.len());
    for job in pending.drain(..) {
        let waiting = Instant::now();
        let Message::Blob { entry: actual } = protocol::receive_for(io, &state.share_id)? else {
            anyhow::bail!("expected blob")
        };
        ensure!(actual == job.entry, "STALE_SOURCE");
        report.metrics.wait_peer_ms += waiting.elapsed().as_millis();
        report
            .metrics
            .first_byte_ms
            .get_or_insert_with(|| round_started.elapsed().as_millis());
        let transferring = Instant::now();
        let snapshot = receive_blob(io, &job.entry)?;
        report.metrics.transfer_ms += transferring.elapsed().as_millis();
        staged.push((job, snapshot));
    }
    for (job, snapshot) in staged {
        let installing = Instant::now();
        store.install(&job.path, job.expected.as_deref(), &job.entry, &snapshot)?;
        report.metrics.install_ms += installing.elapsed().as_millis();
        ack_batch.insert(job.path.clone(), job.entry.clone());
        if ack_batch.len() == protocol::MANIFEST_CHUNK_FILES {
            flush_ack_batch(io, &state.share_id, ack_batch, &mut report.metrics)?;
        }
        state.files.insert(job.path, job.entry.hash);
        persist_state(state_path, state, &mut report.metrics)?;
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
        metrics.ack_entries += batch.len() as u64;
        protocol::send_for(
            io,
            share_id,
            Message::AckBatch {
                entries: std::mem::take(batch),
            },
        )?;
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
    mut progress: impl FnMut(&str, usize, usize),
) -> Result<Report> {
    let started = Instant::now();
    let share_id = state.share_id.clone();
    let store_metrics_before = store.metrics();
    let previous_token = session_tokens().lock().unwrap().remove(state_path);
    let session_matches = previous_token.is_some();
    state.base_token = None;
    let manifest_started = Instant::now();
    let mut delta = None;
    if remap.is_none() && state.last_sync.is_some() && session_matches {
        if let Some(dirty) = store.delta_paths()? {
            if dirty.len() <= 1024 && dirty.iter().all(|p| crate::model::validate_path(p).is_ok()) {
                let request = Message::Scoped {
                    share_id: share_id.clone(),
                    message: Box::new(Message::DeltaScan {
                        base_token: previous_token.unwrap(),
                        paths: dirty,
                    }),
                };
                let request_bytes = serde_json::to_vec(&request)?.len() as u64 + 4;
                protocol::send(io, &request)?;
                let response = protocol::receive_for(io, &share_id)?;
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
                        if let Some(pc) = store.scan_paths(&paths).ok().flatten() {
                            let missing_known = paths.iter().any(|p| {
                                state.files.contains_key(p)
                                    && (!pc.contains_key(p) || !files.contains_key(p))
                            });
                            if !missing_known {
                                delta = Some((pc, files, paths, metrics, bytes));
                            }
                        }
                    }
                }
            }
        }
    }
    let used_delta = delta.is_some();
    let (pc, android, paths, peer_scan_metrics, manifest_bytes) = if let Some(delta) = delta {
        delta
    } else {
        store.require_full_scan()?;
        protocol::send_for(io, &share_id, Message::Scan)?;
        let pc = store.scan()?;
        let (android, metrics, bytes) = protocol::receive_manifest_with_metrics(io, &share_id)?;
        let paths = pc
            .keys()
            .chain(android.keys())
            .chain(state.files.keys())
            .cloned()
            .collect();
        (pc, android, paths, metrics, bytes)
    };
    let android_count = android.len();
    let received_chunks = if used_delta {
        0
    } else {
        android.len().div_ceil(protocol::MANIFEST_CHUNK_FILES)
    };
    let android: crate::model::Manifest = android
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
    if !used_delta {
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
    let mut put_bytes = 0u64;
    let mut get_bytes = 0u64;
    let total = paths.len();
    for (index, path) in paths.into_iter().enumerate() {
        if store.excluded(&path) {
            continue;
        }
        let p = pc.get(&path);
        let a = android.get(&path);
        let ph = p.map(|e| e.hash.as_str());
        let ah = a.map(|e| e.hash.as_str());
        let previous_base = state.files.get(&path).cloned();
        let bootstrap = remap.filter(|_| previous_base.is_none());
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
            persist_state(state_path, state, &mut report.metrics)?;
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
                }
            }
            Action::ToAndroid => {
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
                let temp = store.snapshot(&path, entry)?;
                report.metrics.snapshot_ms += preparing.elapsed().as_millis();
                report.metrics.socket_idle_ms += preparing.elapsed().as_millis();
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
                    protocol::copy_exact(&mut File::open(temp.path())?, io, entry.size)?;
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
                if entry.size > MAX_STAGED_BYTES {
                    let started_transfer = Instant::now();
                    let temp = remote_snapshot(
                        &share_id,
                        io,
                        &path,
                        entry,
                        &mut report.metrics.first_byte_ms,
                        started,
                    )?;
                    let installing = Instant::now();
                    store.install(&path, ph, entry, &temp)?;
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
                    .first_transfer_ms
                    .get_or_insert_with(|| started.elapsed().as_millis());
                let pe = p.unwrap();
                let ae = a.unwrap();
                let pc_snapshot = store.snapshot(&path, pe)?;
                let android_snapshot = remote_snapshot(
                    &share_id,
                    io,
                    &path,
                    ae,
                    &mut report.metrics.first_byte_ms,
                    started,
                )?;
                let conflict = conflict_path(&path, &ae.hash);
                // Preserve Android on BOTH sides before changing its original path.
                // Missing precondition prevents overwriting a manually edited conflict copy.
                let installing = Instant::now();
                store.install(&conflict, None, ae, &android_snapshot)?;
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
        // An unchanged scan should not rewrite the entire state once per file.
        if state.files.get(&path) != previous_base.as_ref() {
            persist_state(state_path, state, &mut report.metrics)?;
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
    progress("", total, total);
    let first_sync = state.last_sync.is_none();
    state.last_sync = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    );
    let new_token = crate::random_id()?;
    if first_sync {
        persist_state(state_path, state, &mut report.metrics)?;
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
    session_tokens()
        .lock()
        .unwrap()
        .insert(state_path.to_path_buf(), new_token);
    Ok(report)
}

pub fn respond(io: &mut (impl Read + Write), store: &mut impl Store) -> Result<Report> {
    respond_share(io, store, None)
}

pub fn respond_share(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    expected_share: Option<&str>,
) -> Result<Report> {
    let mut share = expected_share.map(str::to_owned);
    let result = (|| -> Result<Report> {
        loop {
            let Message::Scoped { share_id, message } = protocol::receive(io)? else {
                anyhow::bail!("missing Share context")
            };
            if let Some(expected) = &share {
                ensure!(expected == &share_id, "wrong Share context");
            } else {
                share = Some(share_id.clone());
            }
            match *message {
                Message::AckBatch { entries } => {
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
                    store.set_base_token(None);
                    store.require_full_scan()?;
                    let before = store.metrics();
                    let files = store.scan()?;
                    let after = store.metrics();
                    protocol::send_manifest_with_metrics(
                        io,
                        &share_id,
                        &files,
                        crate::storage::StoreMetrics {
                            files_enumerated: after
                                .files_enumerated
                                .saturating_sub(before.files_enumerated),
                            files_hashed: after.files_hashed.saturating_sub(before.files_hashed),
                            bytes_hashed: after.bytes_hashed.saturating_sub(before.bytes_hashed),
                            full_scans: after.full_scans.saturating_sub(before.full_scans),
                            ..Default::default()
                        },
                    )?;
                }
                Message::DeltaScan { base_token, paths } => {
                    let old = store.base_token();
                    store.set_base_token(None);
                    let before = store.metrics();
                    let candidate = if old.as_deref() == Some(&base_token)
                        && paths.len() <= 1024
                        && paths.iter().all(|p| crate::model::validate_path(p).is_ok())
                    {
                        match store.delta_paths() {
                            Ok(Some(dirty)) if dirty.len() <= 1024 => {
                                let union: BTreeSet<_> = paths.union(&dirty).cloned().collect();
                                if union.len() <= 1024 {
                                    store
                                        .scan_paths(&union)
                                        .ok()
                                        .flatten()
                                        .map(|files| (union, files))
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
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
                    }
                }
                Message::Get { path, entry } => {
                    crate::model::validate_path(&path)?;
                    let temp = store.snapshot(&path, &entry)?;
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::Blob {
                            entry: entry.clone(),
                        },
                    )?;
                    protocol::copy_exact(&mut File::open(temp.path())?, io, entry.size)?;
                }
                Message::Put {
                    path,
                    entry,
                    expected,
                } => {
                    crate::model::validate_path(&path)?;
                    crate::model::validate_hash(&entry.hash)?;
                    let temp = receive_blob(io, &entry)?;
                    store.install(&path, expected.as_deref(), &entry, &temp)?;
                    protocol::send_for(io, &share_id, Message::Accept)?;
                }
                Message::Done {
                    transferred,
                    conflicts,
                    base_token,
                } => {
                    crate::model::validate_hash(&base_token)?;
                    store.set_base_token(Some(base_token));
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
    if let Err(ref e) = result {
        store.set_base_token(None);
        let _ = protocol::send(
            io,
            &Message::Error {
                message: e.to_string(),
            },
        );
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
            assert_eq!(report.metrics.full_scans, 2, "{scenario}");
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
    fn interrupted_base_token_exchange_forces_full_scan() {
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
                2
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
                assert!(coordinate(&mut x, &mut p, &mut state, &path).is_err());
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
        assert_eq!(report.metrics.peak_in_flight_files, 1);
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
