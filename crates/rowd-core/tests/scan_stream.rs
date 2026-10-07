#![cfg(unix)]
use anyhow::{ensure, Result};
use rowd_core::{
    hash_reader,
    model::{Entry, Manifest, Namespace},
    protocol,
    storage::{LocalStore, Store, VerifiedStaged},
    sync::{self, State},
};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{sync_channel, Receiver},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

/// SAF-shaped producer: separate hashing thread, two chunks, one socket owner.
struct StreamStore {
    inner: LocalStore,
    root: PathBuf,
    namespace: Namespace,
    receiver: Option<Receiver<Manifest>>,
    worker: Option<JoinHandle<Result<()>>>,
    hashed: Arc<AtomicUsize>,
    first_snapshot_hashes: Arc<AtomicUsize>,
}
impl Store for StreamStore {
    fn scan_binding(&mut self) -> Result<String> {
        self.inner.scan_binding()
    }
    fn stream_namespace(&mut self, io: &mut (impl Read + Write)) -> Result<Option<Namespace>> {
        self.namespace = self.inner.stream_namespace(io)?.unwrap();
        Ok(Some(self.namespace.clone()))
    }
    fn start_hash_stream(&mut self, _: &BTreeSet<String>) -> Result<()> {
        let (tx, rx) = sync_channel(2);
        let root = self.root.clone();
        let namespace = self.namespace.clone();
        let hashed = self.hashed.clone();
        self.receiver = Some(rx);
        self.worker = Some(std::thread::spawn(move || {
            let mut files = Manifest::new();
            for (path, physical) in namespace.iter().filter(|(_, e)| !e.directory) {
                let source = root.join(path);
                let before = source.metadata()?;
                std::thread::sleep(Duration::from_millis(1));
                let (hash, size) = hash_reader(File::open(&source)?)?;
                ensure!(
                    size == physical.size && before.modified()? == source.metadata()?.modified()?,
                    "STALE_SOURCE: {path}"
                );
                files.insert(path.clone(), Entry { hash, size });
                hashed.fetch_add(1, Ordering::SeqCst);
                if files.len() == protocol::SCAN_STREAM_CHUNK_FILES {
                    if tx.send(std::mem::take(&mut files)).is_err() {
                        return Ok(());
                    }
                }
            }
            if !files.is_empty() {
                let _ = tx.send(files);
            }
            Ok(())
        }));
        Ok(())
    }
    fn next_hash_chunk(&mut self, _: &mut (impl Read + Write)) -> Result<Option<Manifest>> {
        match self.receiver.as_ref().unwrap().recv() {
            Ok(files) => Ok(Some(files)),
            Err(_) => {
                if let Some(worker) = self.worker.take() {
                    worker.join().unwrap()?;
                }
                Ok(None)
            }
        }
    }
    fn scan(&mut self) -> Result<Manifest> {
        self.inner.scan()
    }
    fn snapshot(&mut self, path: &str, e: &Entry) -> Result<VerifiedStaged> {
        let _ = self.first_snapshot_hashes.compare_exchange(
            usize::MAX,
            self.hashed.load(Ordering::SeqCst),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        self.inner.snapshot(path, e)
    }
    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        e: &Entry,
        staged: &VerifiedStaged,
    ) -> Result<()> {
        self.inner.install(path, expected, e, staged)
    }
    fn commit_scan(&mut self) -> Result<()> {
        self.inner.scan()?;
        self.inner.commit_scan()
    }
    fn discard_scan(&mut self) -> Result<()> {
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap()?;
        }
        self.inner.discard_scan()
    }
}
struct CheckedTarget {
    inner: LocalStore,
    state_path: PathBuf,
    hashed: Arc<AtomicUsize>,
    overlap: usize,
    installed: usize,
    fail_at: Option<usize>,
}
impl Store for CheckedTarget {
    fn staging_directory(&self) -> Option<PathBuf> {
        self.inner.staging_directory()
    }
    fn validate_scan_snapshot(&mut self) -> Result<()> {
        self.inner.validate_scan_snapshot()
    }
    fn stream_namespace(&mut self, io: &mut (impl Read + Write)) -> Result<Option<Namespace>> {
        self.inner.stream_namespace(io)
    }
    fn require_full_scan(&mut self) -> Result<()> {
        self.inner.require_full_scan()
    }
    fn scan(&mut self) -> Result<Manifest> {
        self.inner.scan()
    }
    fn snapshot(&mut self, p: &str, e: &Entry) -> Result<VerifiedStaged> {
        self.inner.snapshot(p, e)
    }
    fn install(
        &mut self,
        p: &str,
        expected: Option<&str>,
        e: &Entry,
        staged: &VerifiedStaged,
    ) -> Result<()> {
        self.inner.install(p, expected, e, staged)
    }
    fn install_received(
        &mut self,
        p: &str,
        expected: Option<&str>,
        e: &Entry,
        staged: VerifiedStaged,
    ) -> Result<()> {
        assert!(
            State::load(&self.state_path, "pair", "share")?
                .files
                .is_empty(),
            "partial committed base"
        );
        assert!(!self.state_path.with_extension("next").exists());
        if self.fail_at == Some(self.installed) {
            anyhow::bail!("simulated connection loss");
        }
        let before = self.hashed.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(3));
        if self.hashed.load(Ordering::SeqCst) > before {
            self.overlap += 1;
        }
        self.inner.install_received(p, expected, e, staged)?;
        self.installed += 1;
        Ok(())
    }
}
#[test]
fn phone_to_pc_reuses_received_payload_without_copy_or_final_rehash() {
    let pc = tempfile::tempdir().unwrap();
    let phone = tempfile::tempdir().unwrap();
    for n in 0..5 {
        std::fs::write(
            phone.path().join(format!("file-{n}")),
            vec![n as u8; 2 * 1024 * 1024],
        )
        .unwrap();
    }
    // Exercise both the bounded GET window and the single large-file path.
    std::fs::write(phone.path().join("large"), vec![7; 9 * 1024 * 1024]).unwrap();
    let mut local = LocalStore::open(pc.path()).unwrap();
    let mut remote = LocalStore::open(phone.path()).unwrap();
    let state_path = pc.path().join(".rowd/state.json");
    let mut state = State::load(&state_path, "pair", "share").unwrap();
    let (mut sender, mut receiver) = std::os::unix::net::UnixStream::pair().unwrap();
    sender
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| sync::respond_share(&mut receiver, &mut remote, Some("share")));
        // The managed coordinator also reaches LocalStore through &mut Store.
        let mut borrowed = &mut local;
        let report = sync::coordinate_mode(
            &mut sender,
            &mut borrowed,
            &mut state,
            &state_path,
            rowd_core::config::SyncMode::ToPc,
        )
        .unwrap();
        assert_eq!(report.transferred, 6);
        worker.join().unwrap().unwrap();
    });
    assert_eq!(state.files.len(), 6);
    assert_eq!(local.metrics().staging_copies, 0);
    assert_eq!(local.metrics().files_hashed, 0);
    assert_eq!(local.metrics().bytes_hashed, 0);
    for n in 0..5 {
        assert_eq!(
            std::fs::read(pc.path().join(format!("file-{n}"))).unwrap(),
            vec![n as u8; 2 * 1024 * 1024]
        );
    }
    assert_eq!(
        std::fs::read(pc.path().join("large")).unwrap(),
        vec![7; 9 * 1024 * 1024]
    );
}

#[test]
fn thousand_files_overlap_hash_and_transfer_and_commit_once() {
    run_large(None);
}
#[test]
fn connection_loss_after_five_hundred_keeps_physical_files_and_old_base() {
    run_large(Some(500));
}
fn run_large(fail_at: Option<usize>) {
    let pc = tempfile::tempdir().unwrap();
    let phone = tempfile::tempdir().unwrap();
    for n in 0..1040 {
        std::fs::write(
            phone.path().join(format!("shot-{n:04}.png")),
            format!("screenshot {n}"),
        )
        .unwrap();
    }
    let hashed = Arc::new(AtomicUsize::new(0));
    let first = Arc::new(AtomicUsize::new(usize::MAX));
    let mut remote = StreamStore {
        inner: LocalStore::open(phone.path()).unwrap(),
        root: phone.path().into(),
        namespace: Default::default(),
        receiver: None,
        worker: None,
        hashed: hashed.clone(),
        first_snapshot_hashes: first.clone(),
    };
    let state_path = pc.path().join(".rowd/state.json");
    let mut local = CheckedTarget {
        inner: LocalStore::open(pc.path()).unwrap(),
        state_path: state_path.clone(),
        hashed,
        overlap: 0,
        installed: 0,
        fail_at,
    };
    let mut state = State::load(&state_path, "pair", "share").unwrap();
    let (mut sender, mut receiver) = std::os::unix::net::UnixStream::pair().unwrap();
    sender
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let result = std::thread::scope(|scope| {
        let worker = scope.spawn(|| sync::respond_share(&mut receiver, &mut remote, Some("share")));
        let result = sync::coordinate_mode(
            &mut sender,
            &mut local,
            &mut state,
            &state_path,
            rowd_core::config::SyncMode::ToPc,
        );
        if result.is_err() {
            sender.shutdown(std::net::Shutdown::Both).unwrap();
        }
        let response = worker.join().unwrap();
        if result.is_ok() {
            response.unwrap();
        }
        result
    });
    assert!(first.load(Ordering::SeqCst) < 1040);
    assert!(
        local.overlap > 0,
        "hashes never advanced during installation"
    );
    assert_eq!(local.inner.metrics().staging_copies, 0);
    if fail_at.is_none() {
        let report = result.unwrap();
        assert_eq!(report.transferred, 1040);
        assert!(report.metrics.transfers_completed_before_hash_end > 0);
        assert!(report.metrics.first_transfer_ms.unwrap() < report.metrics.hash_stream_ms);
        assert_eq!(state.files.len(), 1040);
        assert_eq!(local.inner.scan().unwrap(), remote.inner.scan().unwrap());
    } else {
        assert!(result.is_err());
        assert_eq!(local.installed, 500);
        assert!(state.files.is_empty());
        assert!(State::load(&state_path, "pair", "share")
            .unwrap()
            .files
            .is_empty());
        assert!(state_path.with_extension("pending").exists());
        // A fresh session observes the actual 500 files; equal content is never resent.
        local.fail_at = None;
        let (mut x, mut y) = std::os::unix::net::UnixStream::pair().unwrap();
        // Drop the assertion wrapper once this new round may legitimately commit.
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| sync::respond_share(&mut y, &mut remote, Some("share")));
            let report = sync::coordinate_mode(
                &mut x,
                &mut local.inner,
                &mut state,
                &state_path,
                rowd_core::config::SyncMode::ToPc,
            )
            .unwrap();
            assert_eq!(report.transferred, 540);
            worker.join().unwrap().unwrap();
        });
        assert_eq!(state.files.len(), 1040);
    }
}

#[test]
fn mutation_between_namespace_and_hash_is_stale_and_not_published() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"old").unwrap();
    let mut store = LocalStore::open(root.path()).unwrap();
    let mut io = std::io::Cursor::new(vec![]);
    store.stream_namespace(&mut io).unwrap();
    store.start_hash_stream(&BTreeSet::new()).unwrap();
    std::fs::write(root.path().join("file"), b"changed length").unwrap();
    assert!(store
        .next_hash_chunk(&mut io)
        .unwrap_err()
        .to_string()
        .contains("STALE_SOURCE"));
}

#[test]
fn functional_internal_write_suppression_does_not_hide_external_edit() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("file");
    let (hash, size) = hash_reader(&b"verified"[..]).unwrap();
    let expected = rowd_core::internal_writes::register(&path, root.path(), &Entry { hash, size });
    std::fs::write(&path, b"verified").unwrap();
    expected.complete();
    assert!(rowd_core::internal_writes::matches(&path, false));
    std::fs::write(&path, b"user edit").unwrap();
    assert!(!rowd_core::internal_writes::matches(&path, false));
}

struct Peer {
    input: std::io::Cursor<Vec<u8>>,
    output: Vec<u8>,
}
impl Read for Peer {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(b)
    }
}
impl Write for Peer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.output.extend(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn namespace_peer(namespace: &Namespace) -> Peer {
    let id = "a".repeat(64);
    let mut input = vec![];
    protocol::send_for(&mut input, "share", protocol::Message::ScanReady).unwrap();
    protocol::send_for(
        &mut input,
        "share",
        protocol::Message::ScanStreamBegin {
            scan_id: id.clone(),
            binding: "tree|epoch".into(),
        },
    )
    .unwrap();
    let files: Vec<_> = namespace.iter().collect();
    for (sequence, chunk) in files.chunks(protocol::SCAN_STREAM_CHUNK_FILES).enumerate() {
        protocol::send_for(
            &mut input,
            "share",
            protocol::Message::NamespaceChunk {
                scan_id: id.clone(),
                sequence: sequence as u64,
                entries: chunk
                    .iter()
                    .map(|(p, e)| ((*p).clone(), (*e).clone()))
                    .collect(),
            },
        )
        .unwrap();
    }
    protocol::send_for(
        &mut input,
        "share",
        protocol::Message::NamespaceEnd {
            scan_id: id,
            last_sequence: files.len().div_ceil(protocol::SCAN_STREAM_CHUNK_FILES) as u64,
            total_entries: files.len(),
            namespace_digest: protocol::namespace_digest(namespace).unwrap(),
        },
    )
    .unwrap();
    Peer {
        input: std::io::Cursor::new(input),
        output: vec![],
    }
}
#[test]
fn late_collision_and_file_limit_reject_before_hash_or_transfer() {
    let entry = rowd_core::model::NamespaceEntry {
        size: 0,
        modified: 1,
        directory: false,
    };
    let collision = Namespace::from([
        ("Foo/a".into(), entry.clone()),
        ("foo/B".into(), entry.clone()),
    ]);
    let excessive: Namespace = (0..100001)
        .map(|i| (format!("file-{i:06}"), entry.clone()))
        .collect();
    for namespace in [collision, excessive] {
        let pc = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(pc.path()).unwrap();
        let state_path = pc.path().join(".rowd/state.json");
        let mut state = State::load(&state_path, "pair", "share").unwrap();
        let mut peer = namespace_peer(&namespace);
        assert!(sync::coordinate(&mut peer, &mut store, &mut state, &state_path).is_err());
        let mut frames = std::io::Cursor::new(peer.output);
        while frames.position() < frames.get_ref().len() as u64 {
            let protocol::Message::Scoped { message, .. } = protocol::receive(&mut frames).unwrap()
            else {
                panic!()
            };
            assert!(matches!(
                *message,
                protocol::Message::Scan | protocol::Message::ScanContinue
            ));
        }
        assert!(!state_path.exists());
        assert!(store.scan().unwrap().is_empty());
    }
}
#[test]
fn old_scan_identity_skipped_and_repeated_sequences_are_rejected() {
    let id = "a".repeat(64);
    let entry = rowd_core::model::NamespaceEntry {
        size: 0,
        modified: 1,
        directory: false,
    };
    for (scan_id, sequence) in [("b".repeat(64), 0), (id.clone(), 1), (id.clone(), 0)] {
        let mut bytes = vec![];
        protocol::send_for(
            &mut bytes,
            "share",
            protocol::Message::ScanStreamBegin {
                scan_id: id.clone(),
                binding: "epoch".into(),
            },
        )
        .unwrap();
        if scan_id == id && sequence == 0 {
            protocol::send_for(
                &mut bytes,
                "share",
                protocol::Message::NamespaceChunk {
                    scan_id: id.clone(),
                    sequence: 0,
                    entries: Namespace::from([("one".into(), entry.clone())]),
                },
            )
            .unwrap();
        }
        protocol::send_for(
            &mut bytes,
            "share",
            protocol::Message::NamespaceChunk {
                scan_id,
                sequence,
                entries: Namespace::from([("two".into(), entry.clone())]),
            },
        )
        .unwrap();
        assert!(protocol::ScanStream::receive_namespace(
            &mut Peer {
                input: std::io::Cursor::new(bytes),
                output: vec![]
            },
            "share"
        )
        .is_err());
    }
    // A new receiver always starts at sequence zero, regardless of a previous connection.
    let mut stream = protocol::ScanStream {
        scan_id: "c".repeat(64),
        binding: "epoch".into(),
        namespace: Namespace::from([("file".into(), entry)]),
        sequence: 0,
        seen: Default::default(),
        ended: false,
    };
    let mut bytes = vec![];
    protocol::send_for(
        &mut bytes,
        "share",
        protocol::Message::HashChunk {
            scan_id: id,
            sequence: 0,
            files: Manifest::from([(
                "file".into(),
                Entry {
                    hash: "d".repeat(64),
                    size: 0,
                },
            )]),
        },
    )
    .unwrap();
    assert!(stream
        .next(
            &mut Peer {
                input: std::io::Cursor::new(bytes),
                output: vec![]
            },
            "share"
        )
        .is_err());
}
