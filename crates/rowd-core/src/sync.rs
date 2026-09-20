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
    io: &mut (impl Read + Write),
    path: &str,
    entry: &Entry,
) -> Result<NamedTempFile> {
    protocol::send(
        io,
        &Message::Get {
            path: path.into(),
            entry: entry.clone(),
        },
    )?;
    let Message::Blob { entry: actual } = protocol::receive(io)? else {
        anyhow::bail!("expected blob")
    };
    ensure!(actual == *entry, "STALE_SOURCE");
    receive_blob(io, entry)
}
fn remote_install(
    io: &mut (impl Read + Write),
    path: &str,
    expected: Option<&str>,
    entry: &Entry,
    staged: &Path,
) -> Result<()> {
    protocol::send(
        io,
        &Message::Put {
            path: path.into(),
            expected: expected.map(String::from),
            entry: entry.clone(),
        },
    )?;
    protocol::copy_exact(&mut File::open(staged)?, io, entry.size)?;
    ensure!(
        matches!(protocol::receive(io)?, Message::Accept),
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
    let pc = store.scan()?;
    protocol::send(io, &Message::Scan)?;
    let Message::Files { files: android } = protocol::receive(io)? else {
        anyhow::bail!("expected manifest")
    };
    validate_manifest(&android)?;
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
    for path in paths {
        let p = pc.get(&path);
        let a = android.get(&path);
        let ph = p.map(|e| e.hash.as_str());
        let ah = a.map(|e| e.hash.as_str());
        let previous_base = state.files.get(&path).cloned();
        match reconcile(state.files.get(&path).map(String::as_str), ph, ah) {
            Action::None => {
                if let Some(hash) = ph {
                    state.files.insert(path.clone(), hash.into());
                } else {
                    state.files.remove(&path);
                }
            }
            Action::ToAndroid => {
                let entry = p.unwrap();
                let temp = store.snapshot(&path, entry)?;
                remote_install(io, &path, ah, entry, temp.path())?;
                state.files.insert(path.clone(), entry.hash.clone());
                report.transferred += 1;
            }
            Action::ToPc => {
                let entry = a.unwrap();
                let temp = remote_snapshot(io, &path, entry)?;
                store.install(&path, ph, entry, temp.path())?;
                state.files.insert(path.clone(), entry.hash.clone());
                report.transferred += 1;
            }
            Action::Conflict => {
                let pe = p.unwrap();
                let ae = a.unwrap();
                let pc_snapshot = store.snapshot(&path, pe)?;
                let android_snapshot = remote_snapshot(io, &path, ae)?;
                let conflict = conflict_path(&path, &ae.hash);
                // Preserve Android on BOTH sides before changing its original path.
                // Missing precondition prevents overwriting a manually edited conflict copy.
                store.install(&conflict, None, ae, android_snapshot.path())?;
                remote_install(io, &conflict, None, ae, android_snapshot.path())?;
                remote_install(io, &path, ah, pe, pc_snapshot.path())?;
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
    protocol::send(
        io,
        &Message::Done {
            transferred: report.transferred,
            conflicts: report.conflicts,
        },
    )?;
    Ok(report)
}

pub fn respond(io: &mut (impl Read + Write), store: &mut impl Store) -> Result<Report> {
    let result = (|| -> Result<Report> {
        loop {
            match protocol::receive(io)? {
                Message::Scan => {
                    let files = store.scan()?;
                    validate_manifest(&files)?;
                    protocol::send(io, &Message::Files { files })?;
                }
                Message::Get { path, entry } => {
                    crate::model::validate_path(&path)?;
                    let temp = store.snapshot(&path, &entry)?;
                    protocol::send(
                        io,
                        &Message::Blob {
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
                    protocol::send(io, &Message::Accept)?;
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
                protocol::send(
                    &mut sender,
                    &Message::Put {
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
