use crate::{
    hash_reader,
    model::{conflict_path, reconcile, validate_manifest, Action, Entry, Invitation, VERSION},
    protocol::{self, Message},
    storage::{atomic_json, Store},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::Path,
};
use tempfile::NamedTempFile;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub transferred: usize,
    pub conflicts: usize,
}

#[derive(Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub conflicts: BTreeSet<String>,
    #[serde(default)]
    pub last_sync: Option<u64>,
    pub version: u32,
    pub pair_id: String,
    pub folder_id: String,
    pub peer_root: Option<String>,
    pub files: BTreeMap<String, String>,
}
impl State {
    pub fn load(path: &Path, pair_id: &str, folder_id: &str) -> Result<Self> {
        let state: Self = if path.try_exists()? {
            serde_json::from_reader(File::open(path)?)?
        } else {
            Self {
                conflicts: BTreeSet::new(),
                last_sync: None,
                version: VERSION,
                pair_id: pair_id.into(),
                folder_id: folder_id.into(),
                peer_root: None,
                files: BTreeMap::new(),
            }
        };
        ensure!(
            state.version == VERSION && state.pair_id == pair_id && state.folder_id == folder_id,
            "state belongs to another folder/pair"
        );
        Ok(state)
    }
}

fn receive_blob(io: &mut impl Read, entry: &Entry) -> Result<NamedTempFile> {
    let mut temp = NamedTempFile::new()?;
    protocol::copy_exact(io, &mut temp, entry.size)?;
    let (hash, size) = hash_reader(File::open(temp.path())?)?;
    ensure!(hash == entry.hash && size == entry.size, "HASH_MISMATCH");
    Ok(temp)
}
fn remote_snapshot(
    share_id: &str,
    io: &mut (impl Read + Write),
    path: &str,
    entry: &Entry,
) -> Result<NamedTempFile> {
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
    receive_blob(io, entry)
}
fn remote_install(
    share_id: &str,
    io: &mut (impl Read + Write),
    path: &str,
    expected: Option<&str>,
    entry: &Entry,
    staged: &Path,
) -> Result<()> {
    protocol::send_for(
        io,
        share_id,
        Message::Put {
            path: path.into(),
            expected: expected.map(String::from),
            entry: entry.clone(),
        },
    )?;
    protocol::copy_exact(&mut File::open(staged)?, io, entry.size)?;
    ensure!(
        matches!(protocol::receive_for(io, share_id)?, Message::Accept),
        "expected file confirmation"
    );
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
    coordinate_with_progress(io, store, state, state_path, mode, |_, _, _| {})
}

pub fn coordinate_with_progress(
    io: &mut (impl Read + Write),
    store: &mut impl Store,
    state: &mut State,
    state_path: &Path,
    mode: crate::config::SyncMode,
    mut progress: impl FnMut(&str, usize, usize),
) -> Result<Report> {
    let share_id = state.folder_id.clone();
    let pc = store.scan()?;
    protocol::send_for(io, &share_id, Message::Scan)?;
    let Message::Files { files: android } = protocol::receive_for(io, &share_id)? else {
        anyhow::bail!("expected manifest")
    };
    validate_manifest(&android)?;
    let android: crate::model::Manifest = android
        .into_iter()
        .filter(|(p, _)| !store.excluded(p))
        .collect();
    // Cross-device case collisions would alias on some SAF providers. Stop before any write.
    let paths: BTreeSet<_> = pc
        .keys()
        .chain(android.keys())
        .chain(state.files.keys())
        .cloned()
        .collect();
    let mut folded = BTreeSet::new();
    for path in &paths {
        ensure!(folded.insert(path.to_lowercase()), "case collision: {path}");
    }
    let mut report = Report::default();
    let total = paths.len();
    for (index, path) in paths.into_iter().enumerate() {
        progress(&path, index, total);
        if store.excluded(&path) {
            continue;
        }
        let p = pc.get(&path);
        let a = android.get(&path);
        let ph = p.map(|e| e.hash.as_str());
        let ah = a.map(|e| e.hash.as_str());
        let previous_base = state.files.get(&path).cloned();
        let action = reconcile(state.files.get(&path).map(String::as_str), ph, ah);
        let prohibited = match mode {
            crate::config::SyncMode::Bidirectional => false,
            crate::config::SyncMode::ToAndroid => matches!(action, Action::ToPc | Action::Conflict),
            crate::config::SyncMode::ToPc => matches!(action, Action::ToAndroid | Action::Conflict),
        };
        if prohibited {
            state.conflicts.insert(path.clone());
            report.conflicts += 1;
            atomic_json(state_path, state)?;
            continue;
        }
        if action != Action::Conflict && !path.starts_with("Rowd Conflicts/") {
            state.conflicts.remove(&path);
        }
        match action {
            Action::None => {
                if let Some(entry) = p {
                    store.acknowledge(&path, entry)?;
                    protocol::send_for(
                        io,
                        &share_id,
                        Message::Ack {
                            path: path.clone(),
                            entry: entry.clone(),
                        },
                    )?;
                }
                if let Some(hash) = ph {
                    state.files.insert(path.clone(), hash.into());
                } else {
                    state.files.remove(&path);
                }
            }
            Action::ToAndroid => {
                let entry = p.unwrap();
                let temp = store.snapshot(&path, entry)?;
                remote_install(&share_id, io, &path, ah, entry, temp.path())?;
                state.files.insert(path.clone(), entry.hash.clone());
                store.acknowledge(&path, entry)?;
                report.transferred += 1;
            }
            Action::ToPc => {
                let entry = a.unwrap();
                let temp = remote_snapshot(&share_id, io, &path, entry)?;
                store.install(&path, ph, entry, temp.path())?;
                protocol::send_for(
                    io,
                    &share_id,
                    Message::Ack {
                        path: path.clone(),
                        entry: entry.clone(),
                    },
                )?;
                state.files.insert(path.clone(), entry.hash.clone());
                report.transferred += 1;
            }
            Action::Conflict => {
                let pe = p.unwrap();
                let ae = a.unwrap();
                let pc_snapshot = store.snapshot(&path, pe)?;
                let android_snapshot = remote_snapshot(&share_id, io, &path, ae)?;
                let conflict = conflict_path(&path, &ae.hash);
                // Preserve Android on BOTH sides before changing its original path.
                // Missing precondition prevents overwriting a manually edited conflict copy.
                store.install(&conflict, None, ae, android_snapshot.path())?;
                remote_install(&share_id, io, &conflict, None, ae, android_snapshot.path())?;
                remote_install(&share_id, io, &path, ah, pe, pc_snapshot.path())?;
                store.acknowledge(&path, pe)?;
                state.conflicts.insert(conflict.clone());
                state.files.insert(conflict, ae.hash.clone());
                state.files.insert(path.clone(), pe.hash.clone());
                report.conflicts += 1;
                report.transferred += 3;
            }
        }
        // An unchanged scan should not rewrite the entire state once per file.
        if state.files.get(&path) != previous_base.as_ref() {
            atomic_json(state_path, state)?;
        }
    }
    progress("", total, total);
    state.last_sync = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    );
    atomic_json(state_path, state)?;
    protocol::send_for(
        io,
        &share_id,
        Message::Done {
            transferred: report.transferred,
            conflicts: report.conflicts,
        },
    )?;
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
                Message::Ack { path, entry } => store.acknowledge(&path, &entry)?,
                Message::Scan => {
                    let files = store.scan()?;
                    validate_manifest(&files)?;
                    protocol::send_for(io, &share_id, Message::Files { files })?;
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
                    store.install(&path, expected.as_deref(), &entry, temp.path())?;
                    protocol::send_for(io, &share_id, Message::Accept)?;
                }
                Message::Done {
                    transferred,
                    conflicts,
                } => {
                    return Ok(Report {
                        transferred,
                        conflicts,
                    })
                }
                _ => anyhow::bail!("unexpected protocol message"),
            }
        }
    })();
    if let Err(ref e) = result {
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
    protocol::client_auth(
        &mut io,
        &invitation.pair_id,
        &invitation.folder_id,
        &invitation.secret,
        root_id,
    )
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
            fn snapshot(&mut self, path: &str, entry: &Entry) -> Result<NamedTempFile> {
                self.inner.snapshot(path, entry)
            }
            fn install(
                &mut self,
                path: &str,
                expected: Option<&str>,
                entry: &Entry,
                staged: &Path,
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
            } else {
                assert_eq!(report.transferred, 0);
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
}

#[cfg(all(test, unix))]
mod v2_tests {
    use super::*;
    use crate::{config::SyncMode, journal::TrackedStore, storage::LocalStore};
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
        fn snapshot(&mut self, p: &str, e: &Entry) -> Result<NamedTempFile> {
            self.inner.snapshot(p, e)
        }
        fn install(&mut self, p: &str, x: Option<&str>, e: &Entry, s: &Path) -> Result<()> {
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
    fn lost_ack_keeps_queue_and_replay_is_idempotent() {
        let pc = tempfile::tempdir().unwrap();
        let phone = tempfile::tempdir().unwrap();
        std::fs::write(pc.path().join("a"), b"latest").unwrap();
        let base = pc.path().join(".rowd/state.json");
        let queue = pc.path().join(".rowd/queue.json");
        let mut p = TrackedStore::new(LocalStore::open(pc.path()).unwrap(), queue.clone(), "share")
            .unwrap();
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
        let mut p =
            TrackedStore::new(LocalStore::open(pc.path()).unwrap(), queue, "share").unwrap();
        assert_eq!(p.state.pending.len(), 1);
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
        assert!(p.state.pending.is_empty());
        assert_eq!(p.scan().unwrap(), a.scan().unwrap());
    }
}
