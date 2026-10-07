use crate::{
    model::{Entry, Manifest, MAX_FILE},
    random_id,
};
use anyhow::{ensure, Context, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::io::{Read, Write};

const MAX_FRAME: usize = 16 * 1024 * 1024;
pub const MANIFEST_CHUNK_FILES: usize = 1024;
/// Small batches bound latency to first transfer independently of ACK batching.
pub const SCAN_STREAM_CHUNK_FILES: usize = 32;
pub const PROTOCOL_VERSION: u32 = 15;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    PairRequest {
        version: u32,
        request_id: String,
        device_id: String,
        device_name: String,
    },
    PairPending {
        request_id: String,
        verification_code: String,
    },
    PairAccepted {
        invitation: crate::model::Invitation,
    },
    PairRejected {
        reason: String,
    },
    Scoped {
        share_id: String,
        message: Box<Message>,
    },
    Shares {
        shares: Vec<crate::config::ShareDefinition>,
    },
    Capabilities {
        device_id: String,
        share_requests: Vec<crate::config::ShareRequest>,
        cancel_intents: Vec<String>,
        available_shares: Vec<String>,
        requested_share_ids: Vec<String>,
        audit: bool,
        unlink_requested: bool,
    },
    ShareRequestStatus {
        accepted: Vec<String>,
        #[serde(default)]
        rejected: Vec<String>,
        #[serde(default)]
        cancelled: Vec<String>,
    },
    DeviceUnlinked,
    SelectShare {
        share_id: String,
    },
    StartRound,
    WakeShare {
        share_id: String,
    },
    ShareSkipped {
        share_id: String,
        reason: String,
    },
    SessionDone,
    RoundDeferred {
        shares: Vec<String>,
    },
    AckBatch {
        entries: Manifest,
    },
    Hello {
        version: u32,
        pair_id: String,
        device_id: String,
    },
    Challenge {
        nonce: String,
    },
    Proof {
        mac: String,
    },
    Ready,
    Scan,
    ScanStreamBegin {
        scan_id: String,
        binding: String,
    },
    NamespaceChunk {
        scan_id: String,
        sequence: u64,
        entries: crate::model::Namespace,
    },
    NamespaceEnd {
        scan_id: String,
        last_sequence: u64,
        total_entries: usize,
        namespace_digest: String,
    },
    HashStreamBegin {
        scan_id: String,
        binding: String,
    },
    HashStageHint {
        scan_id: String,
        paths: std::collections::BTreeSet<String>,
    },
    HashRelease {
        scan_id: String,
        paths: std::collections::BTreeSet<String>,
    },
    HashNext {
        scan_id: String,
        sequence: u64,
    },
    HashChunk {
        scan_id: String,
        sequence: u64,
        files: Manifest,
    },
    HashStreamEnd {
        scan_id: String,
        last_sequence: u64,
        total_entries: usize,
        metrics: crate::storage::StoreMetrics,
        pipeline: crate::storage::ScanStreamMetrics,
    },
    /// Receipt only: bytes are verified in private staging, never installed or ACKed.
    StagePut {
        scan_id: String,
        sequence: u64,
        path: String,
        entry: Entry,
        expected: Option<String>,
    },
    StageReceived {
        scan_id: String,
        sequence: u64,
    },
    InstallStaged {
        scan_id: String,
        sequence: u64,
    },
    AuditPreempt {
        shares: Vec<String>,
    },
    ScanAlive,
    ScanReady,
    ScanContinue,
    ScanDeferred,
    DeltaScan {
        base_token: String,
        paths: std::collections::BTreeSet<String>,
    },
    DeltaManifest {
        paths: std::collections::BTreeSet<String>,
        files: Manifest,
        metrics: crate::storage::StoreMetrics,
    },
    NeedFullScan,
    ManifestBegin {
        count: usize,
    },
    ManifestChunk {
        files: Manifest,
    },
    ManifestEnd {
        metrics: crate::storage::StoreMetrics,
    },
    Get {
        path: String,
        entry: Entry,
    },
    Blob {
        entry: Entry,
    },
    Put {
        path: String,
        entry: Entry,
        expected: Option<String>,
    },
    PutBatchEnd,
    Accept,
    Done {
        transferred: usize,
        conflicts: usize,
        base_token: String,
    },
    ShareError {
        error: ShareError,
    },
    Error {
        message: String,
    },
}

impl Message {
    pub fn trace_type(&self) -> &'static str {
        match self {
            Self::PairRequest { .. } => "PairRequest",
            Self::PairPending { .. } => "PairPending",
            Self::PairAccepted { .. } => "PairAccepted",
            Self::PairRejected { .. } => "PairRejected",
            Self::Scoped { .. } => "Scoped",
            Self::Shares { .. } => "Shares",
            Self::Capabilities { .. } => "Capabilities",
            Self::ShareRequestStatus { .. } => "ShareRequestStatus",
            Self::DeviceUnlinked => "DeviceUnlinked",
            Self::SelectShare { .. } => "SelectShare",
            Self::StartRound => "StartRound",
            Self::WakeShare { .. } => "WakeShare",
            Self::ShareSkipped { .. } => "ShareSkipped",
            Self::SessionDone => "SessionDone",
            Self::RoundDeferred { .. } => "RoundDeferred",
            Self::AckBatch { .. } => "AckBatch",
            Self::Hello { .. } => "Hello",
            Self::Challenge { .. } => "Challenge",
            Self::Proof { .. } => "Proof",
            Self::Ready => "Ready",
            Self::Scan => "Scan",
            Self::ScanStreamBegin { .. } => "ScanStreamBegin",
            Self::NamespaceChunk { .. } => "NamespaceChunk",
            Self::NamespaceEnd { .. } => "NamespaceEnd",
            Self::HashStreamBegin { .. } => "HashStreamBegin",
            Self::HashStageHint { .. } => "HashStageHint",
            Self::HashRelease { .. } => "HashRelease",
            Self::HashNext { .. } => "HashNext",
            Self::HashChunk { .. } => "HashChunk",
            Self::HashStreamEnd { .. } => "HashStreamEnd",
            Self::StagePut { .. } => "StagePut",
            Self::StageReceived { .. } => "StageReceived",
            Self::InstallStaged { .. } => "InstallStaged",
            Self::AuditPreempt { .. } => "AuditPreempt",
            Self::ScanAlive => "ScanAlive",
            Self::ShareError { .. } => "ShareError",
            Self::ScanReady => "ScanReady",
            Self::ScanContinue => "ScanContinue",
            Self::ScanDeferred => "ScanDeferred",
            Self::DeltaScan { .. } => "DeltaScan",
            Self::DeltaManifest { .. } => "DeltaManifest",
            Self::NeedFullScan => "NeedFullScan",
            Self::ManifestBegin { .. } => "ManifestBegin",
            Self::ManifestChunk { .. } => "ManifestChunk",
            Self::ManifestEnd { .. } => "ManifestEnd",
            Self::Get { .. } => "Get",
            Self::Blob { .. } => "Blob",
            Self::Put { .. } => "Put",
            Self::PutBatchEnd => "PutBatchEnd",
            Self::Accept => "Accept",
            Self::Done { .. } => "Done",
            Self::Error { .. } => "Error",
        }
    }
    fn trace_metadata(&self, bytes: usize) -> serde_json::Value {
        if let Self::Scoped { share_id, message } = self {
            serde_json::json!({"message_type":message.trace_type(),"share_id":share_id,"payload_size":bytes})
        } else if let Self::ShareError { error } = self {
            serde_json::json!({"message_type":self.trace_type(),"payload_size":bytes,"share_error":error})
        } else {
            serde_json::json!({"message_type":self.trace_type(),"payload_size":bytes})
        }
    }
}

/// Remote errors remain typed through anyhow contexts; never reflect them back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareError {
    pub side: String,
    pub share_id: Option<String>,
    pub operation: String,
    pub relative_path: Option<String>,
    pub kind: String,
    pub message: String,
    pub stream_reusable: bool,
}
impl std::fmt::Display for ShareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "peer: {} share={:?} operation={} path={:?} kind={}: {}",
            self.side, self.share_id, self.operation, self.relative_path, self.kind, self.message
        )
    }
}
impl std::error::Error for ShareError {}

#[derive(Debug)]
struct PeerError(String);
impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "peer: {}", self.0)
    }
}
impl std::error::Error for PeerError {}

/// A failed frame/Blob cannot carry another control frame safely.
#[derive(Debug)]
pub struct IncompleteFrame;
impl std::fmt::Display for IncompleteFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("incomplete transport frame")
    }
}
impl std::error::Error for IncompleteFrame {}

#[derive(Debug)]
pub struct LocalOperation {
    pub operation: String,
    pub path: Option<String>,
    pub kind: String,
    message: String,
}
impl std::fmt::Display for LocalOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {:?}: {}", self.operation, self.path, self.message)
    }
}
impl std::error::Error for LocalOperation {}
impl LocalOperation {
    pub fn attach(error: anyhow::Error, operation: &str, path: Option<&str>) -> anyhow::Error {
        let context = Self {
            operation: operation.into(),
            path: path.map(str::to_owned),
            kind: "filesystem".into(),
            message: format!("{error:#}"),
        };
        error.context(context)
    }
}

#[derive(Debug)]
pub struct ErrorReported(pub String);
impl std::fmt::Display for ErrorReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ErrorReported {}

pub fn is_peer_error(error: &anyhow::Error) -> bool {
    error.is::<ShareError>() || error.is::<PeerError>()
}

pub fn transport_dead(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|e| {
        matches!(
            e.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut
        )
    })
}

/// Best effort only at a control-frame boundary, never inside an unfinished Blob.
pub fn send_share_error(
    io: &mut impl Write,
    error: &anyhow::Error,
    side: &str,
    share_id: Option<&str>,
    operation: &str,
    path: Option<&str>,
    kind: &str,
) {
    if is_peer_error(error)
        || transport_dead(error)
        || error.is::<IncompleteFrame>()
        || error.is::<ErrorReported>()
    {
        return;
    }
    let local = error.downcast_ref::<LocalOperation>();
    let operation = local.map(|l| l.operation.as_str()).unwrap_or(operation);
    let path = local.and_then(|l| l.path.as_deref()).or(path);
    let kind = local.map(|l| l.kind.as_str()).unwrap_or(kind);
    let _ = send(
        io,
        &Message::ShareError {
            error: ShareError {
                side: side.into(),
                share_id: share_id.map(str::to_owned),
                operation: operation.into(),
                relative_path: path.map(str::to_owned),
                kind: kind.into(),
                message: format!("{error:#}"),
                // No recovery handshake exists for a failed, unfinished Share round.
                stream_reusable: false,
            },
        },
    );
}

pub(crate) fn check_peer_error(message: Message) -> Result<Message> {
    match message {
        Message::Error { message } => Err(PeerError(message).into()),
        Message::ShareError { error } => Err(error.into()),
        other => Ok(other),
    }
}

fn trace_message(name: &str, message: &Message, bytes: usize) {
    if !crate::trace::enabled() {
        return;
    }
    let (share, message) = match message {
        Message::Scoped { share_id, message } => (Some(share_id.as_str()), message.as_ref()),
        _ => (None, message),
    };
    let ctx = crate::trace::current_context();
    let share = share.or_else(|| ctx.ids.get("share_id").and_then(serde_json::Value::as_str));
    let context = match (share, message) {
        (Some(share), Message::Get { path, .. } | Message::Put { path, .. }) => {
            crate::trace::transfer_context(share, path)
        }
        _ => share
            .map(|s| ctx.clone().with("share_id", s))
            .unwrap_or_else(|| ctx.clone()),
    };
    let _scope = context.enter();
    crate::trace_event!(
        crate::trace::Level::Trace,
        crate::trace::Component::Protocol,
        name,
        message.trace_metadata(bytes)
    );
    let event = match message {
        Message::ScanStreamBegin { scan_id, binding } => Some((
            "NAMESPACE_BEGIN",
            serde_json::json!({"scan_id":scan_id,"binding":binding}),
        )),
        Message::NamespaceChunk {
            scan_id,
            sequence,
            entries,
        } => Some((
            "NAMESPACE_CHUNK",
            serde_json::json!({"scan_id":scan_id,"sequence":sequence,"entries":entries.len()}),
        )),
        Message::NamespaceEnd {
            scan_id,
            last_sequence,
            total_entries,
            ..
        } => Some((
            "NAMESPACE_END",
            serde_json::json!({"scan_id":scan_id,"last_sequence":last_sequence,"entries":total_entries}),
        )),
        Message::HashStreamBegin { scan_id, .. } => {
            Some(("HASH_STREAM_BEGIN", serde_json::json!({"scan_id":scan_id})))
        }
        Message::HashChunk {
            scan_id,
            sequence,
            files,
        } => Some((
            "HASH_CHUNK",
            serde_json::json!({"scan_id":scan_id,"sequence":sequence,"files":files.len()}),
        )),
        Message::HashStreamEnd {
            scan_id,
            last_sequence,
            total_entries,
            pipeline,
            ..
        } => Some((
            "HASH_STREAM_END",
            serde_json::json!({"scan_id":scan_id,"last_sequence":last_sequence,"files":total_entries,"metrics":pipeline}),
        )),
        _ => None,
    };
    if let Some((event, mut metadata)) = event {
        metadata["protocol_event"] = name.into();
        crate::trace_event!(
            crate::trace::Level::Debug,
            crate::trace::Component::Scanner,
            event,
            metadata
        );
    }
}

pub fn send(io: &mut impl Write, message: &Message) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(bytes.len() <= MAX_FRAME, "frame too large");
    let result = (|| -> Result<()> {
        io.write_all(&(bytes.len() as u32).to_be_bytes())?;
        io.write_all(&bytes)?;
        io.flush()?;
        Ok(())
    })();
    match &result {
        Ok(()) => trace_message("PROTOCOL_SEND", message, bytes.len()),
        Err(error) => crate::trace_event!(
            crate::trace::Level::Error,
            crate::trace::Component::Protocol,
            "PROTOCOL_SEND_FAILED",
            serde_json::json!({"message_type":message.trace_type(),"error":crate::trace::TraceError::new("protocol","send",error)})
        ),
    }
    result.context(IncompleteFrame)?;
    Ok(())
}

fn receive_inner(io: &mut impl Read) -> Result<Message> {
    let mut len = [0; 4];
    io.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    ensure!(len <= MAX_FRAME, "frame too large");
    let mut data = vec![0; len];
    io.read_exact(&mut data)?;
    let message: Message = serde_json::from_slice(&data)?;
    trace_message("PROTOCOL_RECEIVE", &message, len);
    check_peer_error(message)
}

pub fn receive(io: &mut impl Read) -> Result<Message> {
    let result = receive_inner(io);
    if let Err(error) = &result {
        crate::trace_event!(
            crate::trace::Level::Error,
            crate::trace::Component::Protocol,
            "PROTOCOL_RECEIVE_FAILED",
            serde_json::json!({"error":crate::trace::TraceError::new("protocol","receive",error)})
        );
    }
    result
}

pub fn receive_after_first(io: &mut impl Read, first: u8) -> Result<Message> {
    struct Prefixed<'a, R>(&'a mut R, Option<u8>);
    impl<R: Read> Read for Prefixed<'_, R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !buf.is_empty() {
                if let Some(first) = self.1.take() {
                    buf[0] = first;
                    return Ok(1);
                }
            }
            self.0.read(buf)
        }
    }
    receive(&mut Prefixed(io, Some(first)))
}

pub fn receive_for_after_first(io: &mut impl Read, first: u8, share: &str) -> Result<Message> {
    let Message::Scoped { share_id, message } = receive_after_first(io, first)? else {
        anyhow::bail!("missing Share context")
    };
    ensure!(share_id == share, "wrong Share context");
    check_peer_error(*message)
}

pub fn send_for(io: &mut impl Write, share_id: &str, message: Message) -> Result<()> {
    send(
        io,
        &Message::Scoped {
            share_id: share_id.into(),
            message: Box::new(message),
        },
    )
}
pub fn receive_for(io: &mut impl Read, share_id: &str) -> Result<Message> {
    let Message::Scoped {
        share_id: actual,
        message,
    } = receive(io)?
    else {
        anyhow::bail!("missing Share context")
    };
    crate::trace::invariant(
        actual == share_id,
        "protocol_share_context",
        serde_json::json!({"expected_share":share_id,"actual_share":actual}),
    );
    ensure!(actual == share_id, "wrong Share context");
    check_peer_error(*message)
}

pub fn namespace_digest(namespace: &crate::model::Namespace) -> Result<String> {
    use sha2::Digest;
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(namespace)?)))
}

/// Per-round receiver. A new connection never inherits sequence or scan identity.
pub struct ScanStream {
    pub scan_id: String,
    pub binding: String,
    pub namespace: crate::model::Namespace,
    pub sequence: u64,
    pub seen: std::collections::BTreeSet<String>,
    pub ended: bool,
}
impl ScanStream {
    pub fn receive_namespace(io: &mut (impl Read + Write), share: &str) -> Result<Self> {
        let first = loop {
            match receive_for(io, share)? {
                Message::ScanAlive => continue,
                Message::ScanDeferred => return Err(crate::sync::ScanDeferred.into()),
                message => break message,
            }
        };
        let Message::ScanStreamBegin { scan_id, binding } = first else {
            anyhow::bail!("expected scan stream begin")
        };
        crate::model::validate_hash(&scan_id)?;
        ensure!(binding.len() <= 65536, "invalid scan binding");
        let mut stream = Self {
            scan_id,
            binding,
            namespace: Default::default(),
            sequence: 0,
            seen: Default::default(),
            ended: false,
        };
        loop {
            match receive_for(io, share)? {
                Message::NamespaceChunk {
                    scan_id,
                    sequence,
                    entries,
                } => {
                    ensure!(
                        scan_id == stream.scan_id && sequence == stream.sequence,
                        "misaligned namespace chunk"
                    );
                    ensure!(
                        !entries.is_empty() && entries.len() <= SCAN_STREAM_CHUNK_FILES,
                        "invalid namespace chunk size"
                    );
                    for (path, entry) in entries {
                        ensure!(
                            stream.namespace.insert(path, entry).is_none(),
                            "duplicate namespace path"
                        );
                    }
                    ensure!(
                        stream.namespace.len() <= crate::model::MAX_FILES * 2,
                        "too many namespace entries"
                    );
                    stream.sequence += 1;
                }
                Message::NamespaceEnd {
                    scan_id,
                    last_sequence,
                    total_entries,
                    namespace_digest: digest,
                } => {
                    ensure!(
                        scan_id == stream.scan_id
                            && last_sequence == stream.sequence
                            && total_entries == stream.namespace.len(),
                        "misaligned namespace end"
                    );
                    ensure!(
                        digest == namespace_digest(&stream.namespace)?,
                        "namespace digest mismatch"
                    );
                    crate::model::validate_namespace(&stream.namespace)?;
                    stream.sequence = 0;
                    return Ok(stream);
                }
                Message::ScanAlive => {}
                Message::ScanDeferred => return Err(crate::sync::ScanDeferred.into()),
                _ => anyhow::bail!("unexpected namespace frame"),
            }
        }
    }
    pub fn next(
        &mut self,
        io: &mut (impl Read + Write),
        share: &str,
    ) -> Result<(Option<Manifest>, crate::storage::StoreMetrics)> {
        self.next_with_control(io, share, &mut || None)
            .map(|(files, metrics, _)| (files, metrics))
    }
    pub fn next_with_control(
        &mut self,
        io: &mut (impl Read + Write),
        share: &str,
        control: &mut impl FnMut() -> Option<Vec<String>>,
    ) -> Result<(
        Option<Manifest>,
        crate::storage::StoreMetrics,
        crate::storage::ScanStreamMetrics,
    )> {
        struct Waiting<'a, T, F> {
            io: &'a mut T,
            share: &'a str,
            control: &'a mut F,
            preempted: bool,
            alive: std::time::Instant,
        }
        impl<T: Read + Write, F: FnMut() -> Option<Vec<String>>> Read for Waiting<'_, T, F> {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                loop {
                    match crate::io_retry::poll("hash_stream_read", || self.io.read(bytes)) {
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            if self.alive.elapsed() >= std::time::Duration::from_secs(90) {
                                return Err(std::io::ErrorKind::TimedOut.into());
                            }
                            if !self.preempted {
                                if let Some(shares) = (self.control)().filter(|s| !s.is_empty()) {
                                    send_for(self.io, self.share, Message::AuditPreempt { shares })
                                        .map_err(std::io::Error::other)?;
                                    crate::trace_event!(
                                        crate::trace::Level::Info,
                                        crate::trace::Component::Scanner,
                                        "STREAM_PREEMPT_REQUESTED",
                                        serde_json::json!({"phase":"hash_wait","frame_boundary":true})
                                    );
                                    self.preempted = true;
                                }
                            }
                        }
                        result => return result,
                    }
                }
            }
        }
        ensure!(!self.ended, "hash stream already ended");
        send_for(
            io,
            share,
            Message::HashNext {
                scan_id: self.scan_id.clone(),
                sequence: self.sequence,
            },
        )?;
        let mut waiting = Waiting {
            io,
            share,
            control,
            preempted: false,
            alive: std::time::Instant::now(),
        };
        loop {
            match receive_for(&mut waiting, share)? {
                Message::HashChunk {
                    scan_id,
                    sequence,
                    files,
                } => {
                    if waiting.preempted {
                        continue;
                    }
                    ensure!(
                        scan_id == self.scan_id && sequence == self.sequence,
                        "misaligned hash chunk"
                    );
                    ensure!(
                        !files.is_empty() && files.len() <= SCAN_STREAM_CHUNK_FILES,
                        "invalid hash chunk size"
                    );
                    crate::model::validate_manifest(&files)?;
                    for (path, entry) in &files {
                        let physical = self
                            .namespace
                            .get(path)
                            .context("hash path absent from namespace")?;
                        ensure!(
                            !physical.directory
                                && physical.size == entry.size
                                && self.seen.insert(path.clone()),
                            "hash differs from namespace or is duplicated"
                        );
                    }
                    self.sequence += 1;
                    return Ok((Some(files), Default::default(), Default::default()));
                }
                Message::HashStreamEnd {
                    scan_id,
                    last_sequence,
                    total_entries,
                    metrics,
                    pipeline,
                } => {
                    if waiting.preempted {
                        continue;
                    }
                    ensure!(
                        scan_id == self.scan_id
                            && last_sequence == self.sequence
                            && total_entries == self.seen.len(),
                        "misaligned hash stream end"
                    );
                    ensure!(
                        self.seen.len() == self.namespace.values().filter(|e| !e.directory).count(),
                        "incomplete hash stream"
                    );
                    self.ended = true;
                    return Ok((None, metrics, pipeline));
                }
                Message::ScanAlive => {
                    waiting.alive = std::time::Instant::now();
                }
                Message::ScanDeferred => return Err(crate::sync::ScanDeferred.into()),
                _ => anyhow::bail!("unexpected hash frame"),
            }
        }
    }
}

pub fn send_manifest(io: &mut impl Write, share_id: &str, files: &Manifest) -> Result<()> {
    send_manifest_with_metrics(io, share_id, files, Default::default())
}

pub fn send_manifest_with_metrics(
    io: &mut impl Write,
    share_id: &str,
    files: &Manifest,
    metrics: crate::storage::StoreMetrics,
) -> Result<()> {
    crate::model::validate_manifest(files)?;
    send_for(io, share_id, Message::ManifestBegin { count: files.len() })?;
    let mut chunk = Manifest::new();
    for (path, entry) in files {
        chunk.insert(path.clone(), entry.clone());
        if chunk.len() == MANIFEST_CHUNK_FILES {
            send_for(
                io,
                share_id,
                Message::ManifestChunk {
                    files: std::mem::take(&mut chunk),
                },
            )?;
        }
    }
    if !chunk.is_empty() {
        send_for(io, share_id, Message::ManifestChunk { files: chunk })?;
    }
    send_for(io, share_id, Message::ManifestEnd { metrics })
}

pub fn receive_manifest(io: &mut impl Read, share_id: &str) -> Result<Manifest> {
    Ok(receive_manifest_with_metrics(io, share_id)?.0)
}

pub fn receive_manifest_with_metrics(
    io: &mut impl Read,
    share_id: &str,
) -> Result<(Manifest, crate::storage::StoreMetrics, u64)> {
    struct Counted<'a, R>(&'a mut R, u64);
    impl<R: Read> Read for Counted<'_, R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let size = self.0.read(buf)?;
            self.1 += size as u64;
            Ok(size)
        }
    }
    let mut io = Counted(io, 0);
    let count = match receive_for(&mut io, share_id)? {
        Message::ManifestBegin { count } => count,
        Message::ScanDeferred => return Err(crate::sync::ScanDeferred.into()),
        _ => anyhow::bail!("expected manifest begin"),
    };
    ensure!(count <= crate::model::MAX_FILES, "too many files");
    let mut files = Manifest::new();
    while files.len() < count {
        let Message::ManifestChunk { files: chunk } = receive_for(&mut io, share_id)? else {
            anyhow::bail!("expected manifest chunk")
        };
        ensure!(
            !chunk.is_empty() && chunk.len() <= MANIFEST_CHUNK_FILES,
            "invalid manifest chunk"
        );
        for (path, entry) in chunk {
            ensure!(
                files.insert(path.clone(), entry).is_none(),
                "duplicate manifest path: {path}"
            );
        }
        ensure!(files.len() <= count, "manifest exceeds declared count");
    }
    let Message::ManifestEnd { metrics } = receive_for(&mut io, share_id)? else {
        anyhow::bail!("expected manifest end")
    };
    crate::model::validate_manifest(&files)?;
    if crate::trace::enabled() {
        for path in files.keys() {
            let _file = crate::trace::current_context().file(share_id, path).enter();
            crate::trace_event!(
                crate::trace::Level::Trace,
                crate::trace::Component::Protocol,
                "REMOTE_FILE_ADVERTISED",
                serde_json::json!({"relative_path":path})
            );
        }
    }
    Ok((files, metrics, io.1))
}

pub fn copy_exact(reader: &mut impl Read, writer: &mut impl Write, size: u64) -> Result<()> {
    (|| -> Result<()> {
        ensure!(size <= MAX_FILE, "file too large");
        let n = std::io::copy(&mut reader.take(size), writer)?;
        ensure!(n == size, "truncated transfer: {n}/{size}");
        writer.flush()?;
        Ok(())
    })()
    .context(IncompleteFrame)
}

fn auth_mac(secret: &str, nonce: &str, pair_id: &str, device_id: &str) -> Result<Hmac<Sha256>> {
    let key = hex::decode(secret)?;
    ensure!(key.len() == 32, "invalid pairing secret");
    let mut mac = Hmac::<Sha256>::new_from_slice(&key)?;
    // Fixed-length identities and a domain label avoid ambiguous concatenations.
    mac.update(b"rowd-auth-v2\0");
    for value in [nonce, pair_id, device_id] {
        crate::model::validate_hash(value)?;
        mac.update(&hex::decode(value)?);
    }
    Ok(mac)
}

pub fn server_auth(io: &mut (impl Read + Write), pair_id: &str, secret: &str) -> Result<String> {
    let first = receive(io)?;
    server_auth_with_first(io, pair_id, secret, first)
}

fn server_auth_with_first_inner(
    io: &mut (impl Read + Write),
    pair_id: &str,
    secret: &str,
    first: Message,
) -> Result<String> {
    let Message::Hello {
        version,
        pair_id: peer,
        device_id,
    } = first
    else {
        anyhow::bail!("expected hello")
    };
    if version != PROTOCOL_VERSION {
        let message =
            format!("incompatible protocol: expected {PROTOCOL_VERSION}, received {version}");
        send(
            io,
            &Message::Error {
                message: message.clone(),
            },
        )?;
        anyhow::bail!(message);
    }
    ensure!(peer == pair_id, "wrong pairing identity");
    let nonce = random_id()?;
    send(
        io,
        &Message::Challenge {
            nonce: nonce.clone(),
        },
    )?;
    let Message::Proof { mac } = receive(io)? else {
        anyhow::bail!("expected auth proof")
    };
    auth_mac(secret, &nonce, pair_id, &device_id)?
        .verify_slice(&hex::decode(mac)?)
        .context("authentication failed")?;
    send(io, &Message::Ready)?;
    Ok(device_id)
}

fn client_auth_inner(
    io: &mut (impl Read + Write),
    pair_id: &str,
    secret: &str,
    device_id: &str,
) -> Result<()> {
    send(
        io,
        &Message::Hello {
            version: PROTOCOL_VERSION,
            pair_id: pair_id.into(),
            device_id: device_id.into(),
        },
    )?;
    let Message::Challenge { nonce } = receive(io)? else {
        anyhow::bail!("expected challenge")
    };
    let mac = auth_mac(secret, &nonce, pair_id, device_id)?
        .finalize()
        .into_bytes();
    send(
        io,
        &Message::Proof {
            mac: hex::encode(mac),
        },
    )?;
    ensure!(
        matches!(receive(io)?, Message::Ready),
        "authentication rejected"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_and_truncation() {
        assert!(receive(&mut std::io::Cursor::new(u32::MAX.to_be_bytes())).is_err());
        assert!(copy_exact(&mut &b"abc"[..], &mut Vec::new(), 4).is_err());
    }
    #[test]
    fn proof_is_bound_to_nonce_and_device() {
        let h = "a".repeat(64);
        let other = "b".repeat(64);
        let proof = auth_mac(&h, &h, &h, &h).unwrap().finalize().into_bytes();
        assert!(auth_mac(&h, &other, &h, &h)
            .unwrap()
            .verify_slice(&proof)
            .is_err());
        assert!(auth_mac(&h, &h, &h, &other)
            .unwrap()
            .verify_slice(&proof)
            .is_err());
    }
    #[test]
    fn chunked_manifest_round_trips_and_rejects_invalid_sequences() {
        let entry = Entry {
            hash: "a".repeat(64),
            size: 1,
        };
        let files: Manifest = (0..2050)
            .map(|index| (format!("file-{index:05}"), entry.clone()))
            .collect();
        let mut bytes = Vec::new();
        send_manifest(&mut bytes, "share", &files).unwrap();
        assert_eq!(
            receive_manifest(&mut std::io::Cursor::new(bytes), "share").unwrap(),
            files
        );

        let mut missing = Vec::new();
        send_for(&mut missing, "share", Message::ManifestBegin { count: 1 }).unwrap();
        send_for(
            &mut missing,
            "share",
            Message::ManifestEnd {
                metrics: Default::default(),
            },
        )
        .unwrap();
        assert!(receive_manifest(&mut std::io::Cursor::new(missing), "share").is_err());

        let mut duplicate = Vec::new();
        send_for(&mut duplicate, "share", Message::ManifestBegin { count: 2 }).unwrap();
        for _ in 0..2 {
            send_for(
                &mut duplicate,
                "share",
                Message::ManifestChunk {
                    files: Manifest::from([("a".into(), entry.clone())]),
                },
            )
            .unwrap();
        }
        assert!(receive_manifest(&mut std::io::Cursor::new(duplicate), "share").is_err());
    }
}

#[cfg(test)]
mod v2_tests {
    use super::*;
    #[test]
    fn rejects_incompatible_protocol_and_cross_share_messages() {
        struct Buffer {
            input: std::io::Cursor<Vec<u8>>,
            output: Vec<u8>,
        }
        impl Read for Buffer {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.input.read(b)
            }
        }
        impl Write for Buffer {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.output.write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let h = "a".repeat(64);
        let mut input = Vec::new();
        send(
            &mut input,
            &Message::Hello {
                version: 4,
                pair_id: h.clone(),
                device_id: h.clone(),
            },
        )
        .unwrap();
        let mut io = Buffer {
            input: std::io::Cursor::new(input),
            output: vec![],
        };
        assert!(server_auth(&mut io, &h, &h)
            .unwrap_err()
            .to_string()
            .contains("incompatible protocol"));
        assert!(receive(&mut std::io::Cursor::new(io.output))
            .unwrap_err()
            .to_string()
            .contains("incompatible protocol"));
        let mut data = Vec::new();
        send_for(&mut data, "first", Message::Scan).unwrap();
        assert!(receive_for(&mut std::io::Cursor::new(data), "second").is_err());
    }
}

pub fn client_auth(
    io: &mut (impl Read + Write),
    pair_id: &str,
    secret: &str,
    device_id: &str,
) -> Result<()> {
    crate::trace::register_secret(secret);
    crate::trace_event!(
        crate::trace::Level::Debug,
        crate::trace::Component::Connection,
        "AUTH_START",
        serde_json::json!({})
    );
    let result = client_auth_inner(io, pair_id, secret, device_id);
    match &result {
        Ok(_) => {
            crate::trace_event!(
                crate::trace::Level::Debug,
                crate::trace::Component::Connection,
                "TLS_HANDSHAKE_END",
                serde_json::json!({"result":"authenticated","mode":"lazy"})
            );
            crate::trace_event!(
                crate::trace::Level::Info,
                crate::trace::Component::Connection,
                "AUTH_SUCCESS",
                serde_json::json!({})
            );
        }
        Err(error) => crate::trace_event!(
            crate::trace::Level::Error,
            crate::trace::Component::Connection,
            "AUTH_FAILED",
            serde_json::json!({"error":crate::trace::TraceError::new("handshake","authenticate",error)})
        ),
    }
    result
}

pub fn server_auth_with_first(
    io: &mut (impl Read + Write),
    pair_id: &str,
    secret: &str,
    first: Message,
) -> Result<String> {
    crate::trace::register_secret(secret);
    crate::trace_event!(
        crate::trace::Level::Debug,
        crate::trace::Component::Connection,
        "AUTH_START",
        serde_json::json!({})
    );
    let result = server_auth_with_first_inner(io, pair_id, secret, first);
    match &result {
        Ok(_) => {
            crate::trace_event!(
                crate::trace::Level::Debug,
                crate::trace::Component::Connection,
                "TLS_HANDSHAKE_END",
                serde_json::json!({"result":"authenticated","mode":"lazy"})
            );
            crate::trace_event!(
                crate::trace::Level::Info,
                crate::trace::Component::Connection,
                "AUTH_SUCCESS",
                serde_json::json!({})
            );
        }
        Err(error) => crate::trace_event!(
            crate::trace::Level::Error,
            crate::trace::Component::Connection,
            "AUTH_FAILED",
            serde_json::json!({"error":crate::trace::TraceError::new("handshake","authenticate",error)})
        ),
    }
    result
}

#[cfg(test)]
mod error_regressions {
    use super::*;
    #[test]
    fn local_pc_cause_survives_context_and_remote_errors_never_echo() {
        let local = LocalOperation::attach(
            anyhow::anyhow!("storage permission denied"),
            "snapshot",
            Some("screenshots/a.png"),
        );
        let mut bytes = vec![];
        send_share_error(
            &mut bytes,
            &local,
            "pc",
            Some("share"),
            "round",
            None,
            "protocol",
        );
        let remote = receive(&mut std::io::Cursor::new(bytes))
            .unwrap_err()
            .context("round failed");
        let cause = remote.downcast_ref::<ShareError>().unwrap();
        assert_eq!(cause.side, "pc");
        assert_eq!(cause.operation, "snapshot");
        assert_eq!(cause.kind, "filesystem");
        assert_eq!(cause.share_id.as_deref(), Some("share"));
        assert_eq!(cause.relative_path.as_deref(), Some("screenshots/a.png"));
        assert!(cause.message.contains("storage permission denied"));
        assert!(!cause.stream_reusable);
        let mut reflected = vec![];
        send_share_error(
            &mut reflected,
            &remote,
            "responder",
            None,
            "round",
            None,
            "protocol",
        );
        assert!(reflected.is_empty());
    }
    #[test]
    fn disconnect_and_incomplete_blob_do_not_invent_logical_errors() {
        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
        ] {
            let error = anyhow::Error::from(std::io::Error::from(kind));
            let mut bytes = vec![];
            send_share_error(&mut bytes, &error, "pc", None, "blob", None, "transport");
            assert!(bytes.is_empty());
            assert!(transport_dead(&error));
        }
        let error = copy_exact(&mut &b"partial"[..], &mut Vec::new(), 100).unwrap_err();
        let mut bytes = vec![];
        send_share_error(&mut bytes, &error, "pc", None, "blob", None, "filesystem");
        assert!(bytes.is_empty());
        assert!(error.is::<IncompleteFrame>());
    }
}
