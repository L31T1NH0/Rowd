#![cfg(unix)]
//! BUG-001 reproductions: inject the typed result returned by the Android JNI
//! bridge at exact Store boundaries. This does not emulate a real SAF provider.
use anyhow::Result;
use rowd_core::{
    config::SyncMode,
    hash_reader,
    managed::{self, ClientState, ManagedClient},
    model::{Entry, Manifest, Namespace},
    protocol::{self, Message},
    storage::{LocalStore, Store, StoreMetrics, VerifiedStaged},
    sync::{self, Report, ScanDeferred, State},
};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Cursor, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Fault {
    None,
    HashBegin,
    HashNext,
    Snapshot(usize),
    HashBeginIo,
    SnapshotIo(usize),
}

struct DeferredStore {
    inner: LocalStore,
    fault: Fault,
    fired: bool,
    snapshots: usize,
    legacy: bool,
}
impl DeferredStore {
    fn open(root: &Path) -> Self {
        Self {
            inner: LocalStore::open(root).unwrap(),
            fault: Fault::None,
            fired: false,
            snapshots: 0,
            legacy: false,
        }
    }
    fn defer(&mut self, point: Fault) -> Result<()> {
        let io_failure = matches!((self.fault, point), (Fault::HashBeginIo, Fault::HashBegin))
            || matches!((self.fault, point), (Fault::SnapshotIo(a), Fault::Snapshot(b)) if a == b);
        if !self.fired && io_failure {
            self.fired = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected real filesystem failure",
            )
            .into());
        }
        if !self.fired && self.fault == point {
            self.fired = true;
            return Err(ScanDeferred.into());
        }
        Ok(())
    }
}
impl Store for DeferredStore {
    fn staging_directory(&self) -> Option<PathBuf> {
        self.inner.staging_directory()
    }
    fn scan_binding(&mut self) -> Result<String> {
        self.inner.scan_binding()
    }
    fn stream_namespace(&mut self, io: &mut (impl Read + Write)) -> Result<Option<Namespace>> {
        if self.legacy {
            return Ok(None);
        }
        self.inner.stream_namespace(io)
    }
    fn start_hash_stream(&mut self, paths: &BTreeSet<String>) -> Result<()> {
        self.defer(Fault::HashBegin)?;
        self.inner.start_hash_stream(paths)
    }
    fn next_hash_chunk(&mut self, io: &mut (impl Read + Write)) -> Result<Option<Manifest>> {
        self.defer(Fault::HashNext)?;
        self.inner.next_hash_chunk(io)
    }
    fn release_hash_staging(&mut self, paths: &BTreeSet<String>) -> Result<()> {
        self.inner.release_hash_staging(paths)
    }
    fn commit_scan(&mut self) -> Result<()> {
        self.inner.commit_scan()
    }
    fn discard_scan(&mut self) -> Result<()> {
        self.inner.discard_scan()
    }
    fn metrics(&self) -> StoreMetrics {
        self.inner.metrics()
    }
    fn scan(&mut self) -> Result<Manifest> {
        self.inner.scan()
    }
    fn snapshot(&mut self, path: &str, entry: &Entry) -> Result<VerifiedStaged> {
        self.snapshots += 1;
        self.defer(Fault::Snapshot(self.snapshots))?;
        self.inner.snapshot(path, entry)
    }
    fn acknowledge(&mut self, path: &str, entry: &Entry) -> Result<()> {
        self.inner.acknowledge(path, entry)
    }
    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: &VerifiedStaged,
    ) -> Result<()> {
        self.inner.install(path, expected, entry, staged)
    }
}

// A fixed two-share fixture exercises the managed dispatcher on one transport.
impl ManagedClient for DeferredStore {
    fn configure(&mut self, _: &[rowd_core::config::ShareDefinition]) -> Result<()> {
        Ok(())
    }
    fn session_state(&mut self) -> Result<ClientState> {
        Ok(ClientState {
            focus_shares: None,
            audit: false,
            available_shares: vec!["a".repeat(64), "b".repeat(64)],
            share_requests: vec![],
            cancel_intents: vec![],
            unlink_requested: false,
        })
    }
    fn select(&mut self, share: &str) -> Result<()> {
        if share == "b".repeat(64) {
            self.fault = Fault::None;
        }
        Ok(())
    }
    fn acknowledge_share_requests(
        &mut self,
        _: &[String],
        _: &[String],
        _: &[String],
    ) -> Result<()> {
        Ok(())
    }
    fn finish_unlink(&mut self) -> Result<()> {
        Ok(())
    }
}

struct RecordedSocket {
    stream: UnixStream,
    sent: Vec<u8>,
    received: Vec<u8>,
}
impl Read for RecordedSocket {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.stream.read(bytes)?;
        self.received.extend_from_slice(&bytes[..n]);
        Ok(n)
    }
}
impl Write for RecordedSocket {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.stream.write(bytes)?;
        self.sent.extend_from_slice(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}
fn sockets() -> (RecordedSocket, RecordedSocket) {
    let (left, right) = UnixStream::pair().unwrap();
    for socket in [&left, &right] {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
    }
    (
        RecordedSocket {
            stream: left,
            sent: vec![],
            received: vec![],
        },
        RecordedSocket {
            stream: right,
            sent: vec![],
            received: vec![],
        },
    )
}
fn round(
    pc_io: &mut RecordedSocket,
    phone_io: &mut RecordedSocket,
    pc: &mut LocalStore,
    phone: &mut DeferredStore,
    state: &mut State,
    state_path: &Path,
    mode: SyncMode,
) -> (Result<Report>, Result<Report>) {
    std::thread::scope(|scope| {
        let remote = scope.spawn(|| sync::respond_share(phone_io, phone, Some("share")));
        let local = sync::coordinate_mode(pc_io, pc, state, state_path, mode);
        if local.is_err() {
            // Bound cleanup when an unexpected frame leaves the responder waiting.
            let _ = pc_io.stream.shutdown(Shutdown::Both);
        }
        (local, remote.join().unwrap())
    })
}
fn describe(result: &Result<Report>) -> String {
    match result {
        Ok(report) => serde_json::to_string(report).unwrap(),
        Err(error) => format!("{error:#}"),
    }
}
fn sent_types(bytes: &[u8]) -> Vec<String> {
    let mut input = Cursor::new(bytes);
    let mut types = vec![];
    while input.position() < bytes.len() as u64 {
        let message = protocol::receive(&mut input).unwrap();
        types.push(match message {
            Message::Scoped { message, .. } => message.trace_type().to_string(),
            other => other.trace_type().to_string(),
        });
    }
    types
}

fn deferred_contract(fault: Fault, sizes: &[usize]) {
    deferred_contract_with_namespace(fault, sizes, false);
}

fn deferred_contract_with_namespace(fault: Fault, sizes: &[usize], legacy: bool) {
    let pc_root = tempfile::tempdir().unwrap();
    let phone_root = tempfile::tempdir().unwrap();
    for root in [pc_root.path(), phone_root.path()] {
        fs::write(root.join("a-base"), b"committed base").unwrap();
    }
    let mut pc = LocalStore::open(pc_root.path()).unwrap();
    let mut phone = DeferredStore::open(phone_root.path());
    phone.legacy = legacy;
    let state_path = pc.private().join("state.json");
    let mut state = State::load(&state_path, "pair", "share").unwrap();
    let (mut left, mut right) = sockets();
    let (local, remote) = round(
        &mut left,
        &mut right,
        &mut pc,
        &mut phone,
        &mut state,
        &state_path,
        SyncMode::ToPc,
    );
    assert!(!local.unwrap().round_deferred);
    remote.unwrap();
    let prior_memory = serde_json::to_value(&state).unwrap();
    let prior_disk = fs::read(&state_path).unwrap();
    for (n, size) in sizes.iter().enumerate() {
        fs::write(
            phone_root.path().join(format!("b-new-{n:02}")),
            vec![n as u8 + 1; *size],
        )
        .unwrap();
    }
    phone.fault = fault;
    phone.snapshots = 0;
    left.sent.clear();
    right.received.clear();
    let (local, remote) = round(
        &mut left,
        &mut right,
        &mut pc,
        &mut phone,
        &mut state,
        &state_path,
        SyncMode::ToPc,
    );
    eprintln!(
        "fault={fault:?}; PC={}; responder={}; requests={:?}; consumed={:?}",
        describe(&local),
        describe(&remote),
        sent_types(&left.sent),
        sent_types(&right.received)
    );
    assert!(phone.fired, "fault boundary was not reached");
    if sizes.len() == 4 {
        assert_eq!(
            sent_types(&left.sent)
                .iter()
                .filter(|name| name.as_str() == "Get")
                .count(),
            4,
            "do not hide BUG-001 by removing the existing Get window"
        );
    }
    assert_eq!(serde_json::to_value(&state).unwrap(), prior_memory);
    assert_eq!(fs::read(&state_path).unwrap(), prior_disk);
    assert!(!state_path.with_extension("next").exists());
    assert_eq!(
        fs::read(pc_root.path().join("a-base")).unwrap(),
        b"committed base"
    );
    let mut published = 0;
    for (n, size) in sizes.iter().enumerate() {
        let file = pc_root.path().join(format!("b-new-{n:02}"));
        if file.exists() {
            published += 1;
            assert_eq!(
                fs::read(file).unwrap(),
                vec![n as u8 + 1; *size],
                "partial payload published"
            );
        }
    }
    if sizes.len() == 4 && matches!(fault, Fault::Snapshot(_)) {
        assert_eq!(
            published, 0,
            "the deferred batch must discard its private staging"
        );
    }
    if fault == Fault::Snapshot(6) && sizes.len() == 8 {
        assert_eq!(
            published, 4,
            "earlier verified batches must remain installed"
        );
    }
    assert!(
        local.as_ref().is_ok_and(|report| report.round_deferred),
        "BUG-001: PC must defer, not fail: {}",
        describe(&local)
    );
    assert!(
        remote.as_ref().is_ok_and(|report| report.round_deferred),
        "BUG-001: responder must defer, not fail: {}",
        describe(&remote)
    );

    // Do not replace either socket: this catches unread queued requests/responses.
    phone.fault = Fault::None;
    let (local, remote) = round(
        &mut left,
        &mut right,
        &mut pc,
        &mut phone,
        &mut state,
        &state_path,
        SyncMode::ToPc,
    );
    let resumed = local.unwrap();
    assert!(!resumed.round_deferred);
    assert_eq!(resumed.transferred, sizes.len() - published);
    remote.unwrap();
    for (n, size) in sizes.iter().enumerate() {
        let name = format!("b-new-{n:02}");
        let bytes = fs::read(pc_root.path().join(&name)).unwrap();
        assert_eq!(bytes, vec![n as u8 + 1; *size]);
        assert_eq!(
            hash_reader(File::open(phone_root.path().join(&name)).unwrap())
                .unwrap()
                .0,
            state.files[&name]
        );
    }
    let (local, remote) = round(
        &mut left,
        &mut right,
        &mut pc,
        &mut phone,
        &mut state,
        &state_path,
        SyncMode::ToPc,
    );
    assert_eq!(local.unwrap().transferred, 0);
    remote.unwrap();
}

#[test]
fn hash_next_deferral_keeps_base_and_reuses_connection() {
    deferred_contract(Fault::HashNext, &[4096]);
}

#[test]
fn hash_begin_deferral_keeps_base_and_reuses_connection() {
    deferred_contract(Fault::HashBegin, &[4096]);
}
#[test]
fn first_get_deferral_drains_four_requests_and_reuses_connection() {
    deferred_contract(Fault::Snapshot(1), &[4096; 4]);
}
#[test]
fn second_get_deferral_drains_valid_blob_and_later_requests() {
    deferred_contract(Fault::Snapshot(2), &[4096; 4]);
}
#[test]
fn large_get_deferral_keeps_base_and_reuses_connection() {
    deferred_contract(Fault::Snapshot(1), &[9 * 1024 * 1024]);
}

#[test]
fn create_modify_restart_and_direction_smoke() {
    for mode in [SyncMode::ToPc, SyncMode::ToAndroid, SyncMode::Bidirectional] {
        let pc_root = tempfile::tempdir().unwrap();
        let phone_root = tempfile::tempdir().unwrap();
        let (source, target) = if mode == SyncMode::ToAndroid {
            (pc_root.path(), phone_root.path())
        } else {
            (phone_root.path(), pc_root.path())
        };
        let state_path = pc_root.path().join(".rowd/state.json");
        for content in [b"created".as_slice(), b"modified contents".as_slice()] {
            fs::write(source.join("file"), content).unwrap();
            // Fresh stores, State reload and socket on each round simulate restart.
            let mut pc = LocalStore::open(pc_root.path()).unwrap();
            let mut phone = DeferredStore::open(phone_root.path());
            let mut state = State::load(&state_path, "pair", "share").unwrap();
            let (mut left, mut right) = sockets();
            let (local, remote) = round(
                &mut left,
                &mut right,
                &mut pc,
                &mut phone,
                &mut state,
                &state_path,
                mode,
            );
            assert_eq!(local.unwrap().transferred, 1);
            remote.unwrap();
            assert_eq!(fs::read(target.join("file")).unwrap(), content);
            assert_eq!(state.files["file"], hash_reader(content).unwrap().0);
        }
        fs::write(source.join("after-restart"), b"new file").unwrap();
        if mode != SyncMode::Bidirectional {
            fs::write(target.join("target-only"), b"forbidden reverse write").unwrap();
        }
        let mut pc = LocalStore::open(pc_root.path()).unwrap();
        let mut phone = DeferredStore::open(phone_root.path());
        let mut state = State::load(&state_path, "pair", "share").unwrap();
        let (mut left, mut right) = sockets();
        let (local, remote) = round(
            &mut left,
            &mut right,
            &mut pc,
            &mut phone,
            &mut state,
            &state_path,
            mode,
        );
        assert_eq!(local.unwrap().transferred, 1);
        remote.unwrap();
        assert_eq!(fs::read(target.join("after-restart")).unwrap(), b"new file");
        if mode != SyncMode::Bidirectional {
            assert!(!source.join("target-only").exists());
            assert_eq!(
                fs::read(target.join("target-only")).unwrap(),
                b"forbidden reverse write"
            );
        }
    }
}

#[test]
fn last_get_deferral_discards_valid_staging_and_reuses_connection() {
    deferred_contract(Fault::Snapshot(4), &[4096; 4]);
}

#[test]
fn third_get_deferral_keeps_window_aligned() {
    deferred_contract(Fault::Snapshot(3), &[4096; 4]);
}

#[test]
fn later_get_deferral_preserves_earlier_verified_batch_and_resumes_remaining_files() {
    deferred_contract(Fault::Snapshot(6), &[4096; 8]);
}

#[test]
fn legacy_get_deferral_without_active_hash_stream_reuses_connection() {
    deferred_contract_with_namespace(Fault::Snapshot(2), &[4096; 4], true);
}

#[test]
fn genuine_io_failures_remain_fatal_and_preserve_base() {
    for fault in [Fault::HashBeginIo, Fault::SnapshotIo(2)] {
        let pc_root = tempfile::tempdir().unwrap();
        let phone_root = tempfile::tempdir().unwrap();
        let mut pc = LocalStore::open(pc_root.path()).unwrap();
        let mut phone = DeferredStore::open(phone_root.path());
        let state_path = pc.private().join("state.json");
        let mut state = State::load(&state_path, "pair", "share").unwrap();
        let (mut left, mut right) = sockets();
        let (local, remote) = round(
            &mut left,
            &mut right,
            &mut pc,
            &mut phone,
            &mut state,
            &state_path,
            SyncMode::ToPc,
        );
        local.unwrap();
        remote.unwrap();
        let prior_memory = serde_json::to_value(&state).unwrap();
        let prior_disk = fs::read(&state_path).unwrap();
        for n in 0..4 {
            fs::write(phone_root.path().join(format!("new-{n}")), [n as u8; 4096]).unwrap();
        }
        phone.fault = fault;
        phone.snapshots = 0;
        let (local, remote) = round(
            &mut left,
            &mut right,
            &mut pc,
            &mut phone,
            &mut state,
            &state_path,
            SyncMode::ToPc,
        );
        assert!(phone.fired);
        for result in [&local, &remote] {
            let error = result.as_ref().unwrap_err();
            assert!(format!("{error:#}").contains("injected real filesystem failure"));
            assert!(!error.is::<ScanDeferred>());
        }
        assert_eq!(serde_json::to_value(&state).unwrap(), prior_memory);
        assert_eq!(fs::read(&state_path).unwrap(), prior_disk);
        assert!(!state_path.with_extension("next").exists());
        for n in 0..4 {
            assert!(!pc_root.path().join(format!("new-{n}")).exists());
        }
    }
}

#[test]
fn hash_begin_deferral_with_pc_only_puts_preserves_both_directions() {
    for mode in [SyncMode::ToAndroid, SyncMode::Bidirectional] {
        let pc_root = tempfile::tempdir().unwrap();
        let phone_root = tempfile::tempdir().unwrap();
        let mut pc = LocalStore::open(pc_root.path()).unwrap();
        let mut phone = DeferredStore::open(phone_root.path());
        let state_path = pc.private().join("state.json");
        let mut state = State::load(&state_path, "pair", "share").unwrap();
        let (mut left, mut right) = sockets();
        let (local, remote) = round(
            &mut left,
            &mut right,
            &mut pc,
            &mut phone,
            &mut state,
            &state_path,
            mode,
        );
        local.unwrap();
        remote.unwrap();
        let prior_memory = serde_json::to_value(&state).unwrap();
        let prior_disk = fs::read(&state_path).unwrap();
        fs::write(pc_root.path().join("pc-only"), b"outbound staged bytes").unwrap();
        phone.fault = Fault::HashBegin;
        let (local, remote) = round(
            &mut left,
            &mut right,
            &mut pc,
            &mut phone,
            &mut state,
            &state_path,
            mode,
        );
        assert!(phone.fired);
        assert!(local.unwrap().round_deferred);
        assert!(remote.unwrap().round_deferred);
        assert_eq!(serde_json::to_value(&state).unwrap(), prior_memory);
        assert_eq!(fs::read(&state_path).unwrap(), prior_disk);
        assert!(!state_path.with_extension("next").exists());
        assert!(!phone_root.path().join("pc-only").exists());
        phone.fault = Fault::None;
        let (local, remote) = round(
            &mut left,
            &mut right,
            &mut pc,
            &mut phone,
            &mut state,
            &state_path,
            mode,
        );
        assert_eq!(local.unwrap().transferred, 1);
        remote.unwrap();
        assert_eq!(
            fs::read(phone_root.path().join("pc-only")).unwrap(),
            b"outbound staged bytes"
        );
        let (local, remote) = round(
            &mut left,
            &mut right,
            &mut pc,
            &mut phone,
            &mut state,
            &state_path,
            mode,
        );
        assert_eq!(local.unwrap().transferred, 0);
        remote.unwrap();
    }
}

#[test]
fn managed_dispatcher_accepts_next_share_and_late_wake_after_deferral() {
    for fault in [Fault::HashBegin, Fault::Snapshot(2)] {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let phone_root = tempfile::tempdir().unwrap();
        for n in 0..4 {
            fs::write(phone_root.path().join(format!("file-{n}")), [n as u8; 4096]).unwrap();
        }
        let mut phone = DeferredStore::open(phone_root.path());
        phone.fault = fault;
        let (mut left, mut right) = sockets();
        std::thread::scope(|scope| {
            let responder =
                scope.spawn(|| managed::client_round_on(&mut right, "device", &mut phone));
            assert!(matches!(
                protocol::receive(&mut left).unwrap(),
                Message::StartRound
            ));
            let shares = ["a", "b"].map(|name| rowd_core::config::ShareDefinition {
                share_id: name.repeat(64),
                name: name.into(),
                mode: SyncMode::ToPc,
                enabled: true,
                binding_revision: 0,
                ignore_rules: String::new(),
                request_id: None,
                remap_policy: None,
            });
            protocol::send(
                &mut left,
                &Message::Shares {
                    shares: shares.to_vec(),
                },
            )
            .unwrap();
            assert!(matches!(
                protocol::receive(&mut left).unwrap(),
                Message::Capabilities { .. }
            ));
            protocol::send(
                &mut left,
                &Message::ShareRequestStatus {
                    accepted: vec![],
                    rejected: vec![],
                    cancelled: vec![],
                },
            )
            .unwrap();
            for (n, root) in [first_root.path(), second_root.path()].iter().enumerate() {
                let id = shares[n].share_id.clone();
                protocol::send(
                    &mut left,
                    &Message::SelectShare {
                        share_id: id.clone(),
                    },
                )
                .unwrap();
                assert!(matches!(
                    protocol::receive(&mut left).unwrap(),
                    Message::Ready
                ));
                let mut pc = LocalStore::open(root).unwrap();
                let state_path = pc.private().join("state.json");
                let mut state = State::load(&state_path, "pair", &id).unwrap();
                let report = sync::coordinate_mode(
                    &mut left,
                    &mut pc,
                    &mut state,
                    &state_path,
                    SyncMode::ToPc,
                )
                .unwrap();
                if n == 0 {
                    assert!(report.round_deferred);
                    assert!(!state_path.exists());
                    // A crossed control remains scoped to the completed deferred share.
                    protocol::send_for(
                        &mut left,
                        &id,
                        Message::AuditPreempt {
                            shares: vec!["c".repeat(64)],
                        },
                    )
                    .unwrap();
                } else {
                    assert!(!report.round_deferred);
                    assert_eq!(report.transferred, 4);
                    for file in 0..4 {
                        assert_eq!(
                            fs::read(root.join(format!("file-{file}"))).unwrap(),
                            [file as u8; 4096]
                        );
                    }
                }
            }
            protocol::send(&mut left, &Message::SessionDone).unwrap();
            let report = responder.join().unwrap().unwrap();
            assert_eq!(report.pending_wakes, vec!["c".repeat(64)]);
        });
        assert!(phone.fired);
    }
}

#[test]
fn coordinator_preemption_crossing_get_deferral_preserves_other_share_wake() {
    struct WakeOnGet<'a> {
        io: &'a mut RecordedSocket,
        count: Arc<AtomicUsize>,
    }
    impl Read for WakeOnGet<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.io.read(bytes)
        }
    }
    impl Write for WakeOnGet<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let n = self.io.write(bytes)?;
            if matches!(serde_json::from_slice::<Message>(&bytes[..n]), Ok(Message::Scoped { message, .. }) if matches!(*message, Message::Get { .. }))
            {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.io.flush()
        }
    }
    let pc_root = tempfile::tempdir().unwrap();
    let phone_root = tempfile::tempdir().unwrap();
    let share_id = "a".repeat(64);
    let mut pc = LocalStore::open(pc_root.path()).unwrap();
    let mut phone = DeferredStore::open(phone_root.path());
    for n in 0..4 {
        fs::write(phone_root.path().join(format!("file-{n}")), [n as u8; 4096]).unwrap();
    }
    phone.fault = Fault::Snapshot(2);
    let state_path = pc.private().join("state.json");
    let mut state = State::load(&state_path, "pair", &share_id).unwrap();
    let (mut left, mut right) = sockets();
    let count = Arc::new(AtomicUsize::new(0));
    let other_share = "c".repeat(64);
    std::thread::scope(|scope| {
        let remote = scope.spawn(|| sync::respond_share(&mut right, &mut phone, Some(&share_id)));
        let mut observed = WakeOnGet {
            io: &mut left,
            count: count.clone(),
        };
        let local = sync::coordinate_with_progress_and_audit_control(
            &mut observed,
            &mut pc,
            &mut state,
            &state_path,
            SyncMode::ToPc,
            None,
            |_, _, _| {},
            |_| (count.load(Ordering::SeqCst) == 4).then(|| vec![other_share.clone()]),
        )
        .unwrap();
        assert!(local.round_deferred);
        assert_eq!(local.pending_wakes, vec![other_share]);
        assert!(remote.join().unwrap().unwrap().round_deferred);
        assert!(!state_path.exists());
    });
    phone.fault = Fault::None;
    std::thread::scope(|scope| {
        let remote = scope.spawn(|| sync::respond_share(&mut right, &mut phone, Some(&share_id)));
        assert_eq!(
            sync::coordinate_mode(&mut left, &mut pc, &mut state, &state_path, SyncMode::ToPc)
                .unwrap()
                .transferred,
            4
        );
        remote.join().unwrap().unwrap();
    });
}

fn start_scripted_hash_stream(io: &mut RecordedSocket) -> protocol::ScanStream {
    protocol::send_for(io, "share", Message::Scan).unwrap();
    assert!(matches!(
        protocol::receive_for(io, "share").unwrap(),
        Message::ScanReady
    ));
    protocol::send_for(io, "share", Message::ScanContinue).unwrap();
    let stream = protocol::ScanStream::receive_namespace(io, "share").unwrap();
    protocol::send_for(
        io,
        "share",
        Message::HashStreamBegin {
            scan_id: stream.scan_id.clone(),
            binding: stream.binding.clone(),
        },
    )
    .unwrap();
    stream
}

#[test]
fn deferred_hash_begin_still_rejects_wrong_scan_sequence_and_share() {
    for bad_request in ["scan", "sequence", "share"] {
        let phone_root = tempfile::tempdir().unwrap();
        let mut phone = DeferredStore::open(phone_root.path());
        phone.fault = Fault::HashBegin;
        let (mut left, mut right) = sockets();
        std::thread::scope(|scope| {
            let remote = scope.spawn(|| sync::respond_share(&mut right, &mut phone, Some("share")));
            let stream = start_scripted_hash_stream(&mut left);
            protocol::send_for(
                &mut left,
                if bad_request == "share" {
                    "other"
                } else {
                    "share"
                },
                Message::HashNext {
                    scan_id: if bad_request == "scan" {
                        "f".repeat(64)
                    } else {
                        stream.scan_id
                    },
                    sequence: if bad_request == "sequence" { 1 } else { 0 },
                },
            )
            .unwrap();
            let peer_error = protocol::receive(&mut left).unwrap_err();
            let error = peer_error.downcast_ref::<protocol::ShareError>().unwrap();
            assert!(!error.stream_reusable);
            assert!(remote.join().unwrap().is_err());
        });
        assert!(phone.fired);
    }
}

#[test]
fn snapshot_deferral_after_hash_end_requires_barrier_and_rejects_eof() {
    for eof in [false, true] {
        let phone_root = tempfile::tempdir().unwrap();
        fs::write(phone_root.path().join("file"), b"complete payload").unwrap();
        let mut phone = DeferredStore::open(phone_root.path());
        phone.fault = Fault::Snapshot(1);
        let (mut left, mut right) = sockets();
        std::thread::scope(|scope| {
            let remote = scope.spawn(|| sync::respond_share(&mut right, &mut phone, Some("share")));
            let mut stream = start_scripted_hash_stream(&mut left);
            let files = stream.next(&mut left, "share").unwrap().0.unwrap();
            assert!(stream.next(&mut left, "share").unwrap().0.is_none());
            protocol::send_for(
                &mut left,
                "share",
                Message::Get {
                    path: "file".into(),
                    entry: files["file"].clone(),
                },
            )
            .unwrap();
            assert!(matches!(
                protocol::receive_for(&mut left, "share").unwrap(),
                Message::ScanDeferred
            ));
            if eof {
                left.stream.shutdown(Shutdown::Write).unwrap();
                let error = remote.join().unwrap().unwrap_err();
                assert!(!error.is::<ScanDeferred>());
                assert_eq!(
                    error.downcast_ref::<std::io::Error>().unwrap().kind(),
                    std::io::ErrorKind::UnexpectedEof
                );
            } else {
                protocol::send_for(&mut left, "share", Message::AuditPreempt { shares: vec![] })
                    .unwrap();
                assert!(matches!(
                    protocol::receive_for(&mut left, "share").unwrap(),
                    Message::ScanDeferred
                ));
                assert!(remote.join().unwrap().unwrap().round_deferred);
            }
        });
        assert!(phone.fired);
    }
}

#[test]
fn raw_payload_after_get_deferral_closes_without_injecting_control_frame() {
    for raw_kind in ["put", "stage", "blob"] {
        let phone_root = tempfile::tempdir().unwrap();
        fs::write(phone_root.path().join("file"), b"complete payload").unwrap();
        let mut phone = DeferredStore::open(phone_root.path());
        phone.fault = Fault::Snapshot(1);
        let (mut left, mut right) = sockets();
        std::thread::scope(|scope| {
            let remote = scope.spawn(|| sync::respond_share(&mut right, &mut phone, Some("share")));
            let mut stream = start_scripted_hash_stream(&mut left);
            let files = stream.next(&mut left, "share").unwrap().0.unwrap();
            let entry = files["file"].clone();
            protocol::send_for(
                &mut left,
                "share",
                Message::Get {
                    path: "file".into(),
                    entry: entry.clone(),
                },
            )
            .unwrap();
            assert!(matches!(
                protocol::receive_for(&mut left, "share").unwrap(),
                Message::ScanDeferred
            ));
            let message = match raw_kind {
                "put" => Message::Put {
                    path: "unexpected".into(),
                    entry,
                    expected: None,
                },
                "stage" => Message::StagePut {
                    path: "unexpected".into(),
                    entry,
                    expected: None,
                    scan_id: stream.scan_id,
                    sequence: 0,
                },
                _ => Message::Blob { entry },
            };
            protocol::send_for(&mut left, "share", message).unwrap();
            left.write_all(b"complete payload").unwrap();
            let error = remote.join().unwrap().unwrap_err();
            assert!(error.is::<protocol::IncompleteFrame>());
        });
        assert!(!sent_types(&right.sent)
            .iter()
            .any(|name| name == "ShareError"));
        assert!(!phone_root.path().join("unexpected").exists());
    }
}
