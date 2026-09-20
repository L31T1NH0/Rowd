use crate::{
    hash_reader,
    model::{validate_manifest, validate_path, Entry, Manifest},
    random_id,
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

pub type Snapshot = NamedTempFile;
pub fn snapshot_file(path: &Path) -> Result<Snapshot> {
    let mut temp = NamedTempFile::new()?;
    std::io::copy(&mut File::open(path)?, &mut temp)?;
    Ok(temp)
}

pub trait Store {
    fn scan(&mut self) -> Result<Manifest>;
    fn snapshot(&mut self, path: &str, expected: &Entry) -> Result<NamedTempFile>;
    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: &Path,
    ) -> Result<()>;
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("state has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temp, value)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    sync_dir(parent)?;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Journal {
    path: String,
    backup: String,
    finished: bool,
}

pub struct LocalStore {
    root: PathBuf,
    private: PathBuf,
    _lock: File,
}

impl LocalStore {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let private = root.join(".rowd");
        if private.try_exists()? {
            ensure!(
                !fs::symlink_metadata(&private)?.file_type().is_symlink(),
                "symlink metadata directory"
            );
        }
        fs::create_dir_all(private.join("recovery"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(private.join("lock"))?;
        lock.try_lock_exclusive()
            .context("another Rowd process is using this folder")?;
        let mut store = Self {
            root,
            private,
            _lock: lock,
        };
        store.recover()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn private(&self) -> &Path {
        &self.private
    }

    fn checked_path(&self, relative: &str, create_parents: bool) -> Result<PathBuf> {
        validate_path(relative)?;
        let parts: Vec<_> = relative.split('/').collect();
        let mut out = self.root.clone();
        for (i, part) in parts.iter().enumerate() {
            out.push(part);
            match fs::symlink_metadata(&out) {
                Ok(meta) => {
                    ensure!(
                        !meta.file_type().is_symlink(),
                        "symlink rejected: {relative}"
                    );
                    if i + 1 < parts.len() {
                        ensure!(meta.is_dir(), "not a directory: {relative}");
                    } else {
                        ensure!(meta.is_file(), "not a regular file: {relative}");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if i + 1 < parts.len() && create_parents {
                        fs::create_dir(&out)?;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(out)
    }

    fn current(&self, path: &str) -> Result<Option<Entry>> {
        let target = self.checked_path(path, false)?;
        match File::open(target) {
            Ok(f) => {
                let (hash, size) = hash_reader(f)?;
                Ok(Some(Entry { hash, size }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn recover(&mut self) -> Result<()> {
        for item in fs::read_dir(self.private.join("recovery"))? {
            let path = item?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let mut journal: Journal = serde_json::from_reader(File::open(&path)?)?;
            if journal.finished {
                continue;
            }
            let target = self.checked_path(&journal.path, true)?;
            crate::model::validate_hash(&journal.backup)?;
            let backup = self.private.join("recovery").join(&journal.backup);
            if !target.try_exists()? && backup.is_file() {
                let mut temp = NamedTempFile::new_in(target.parent().unwrap())?;
                std::io::copy(&mut File::open(&backup)?, &mut temp)?;
                temp.as_file().sync_all()?;
                temp.persist_noclobber(&target)?;
                sync_dir(target.parent().unwrap())?;
            }
            journal.finished = true;
            atomic_json(&path, &journal)?;
        }
        Ok(())
    }
}

impl Store for LocalStore {
    fn scan(&mut self) -> Result<Manifest> {
        fn walk(base: &Path, dir: &Path, result: &mut Manifest) -> Result<()> {
            for item in fs::read_dir(dir)? {
                let item = item?;
                if dir == base && item.file_name() == ".rowd" {
                    continue;
                }
                let path = item.path();
                let rel = path
                    .strip_prefix(base)?
                    .to_str()
                    .context("non UTF-8 filename")?
                    .replace('\\', "/");
                validate_path(&rel)?;
                let kind = item.file_type()?;
                ensure!(!kind.is_symlink(), "symlink rejected: {rel}");
                if kind.is_dir() {
                    walk(base, &path, result)?;
                } else if kind.is_file() {
                    let (hash, size) = hash_reader(File::open(&path)?)?;
                    result.insert(rel, Entry { hash, size });
                } else {
                    bail!("unsupported file: {rel}");
                }
            }
            Ok(())
        }
        let mut result = Manifest::new();
        walk(&self.root, &self.root, &mut result)?;
        validate_manifest(&result)?;
        Ok(result)
    }

    fn snapshot(&mut self, path: &str, expected: &Entry) -> Result<NamedTempFile> {
        let source = self.checked_path(path, false)?;
        let mut temp = NamedTempFile::new_in(&self.private)?;
        std::io::copy(&mut File::open(source)?, &mut temp)?;
        let (hash, size) = hash_reader(File::open(temp.path())?)?;
        ensure!(
            hash == expected.hash && size == expected.size,
            "STALE_SOURCE: {path}"
        );
        Ok(temp)
    }

    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: &Path,
    ) -> Result<()> {
        let (hash, size) = hash_reader(File::open(staged)?)?;
        ensure!(hash == entry.hash && size == entry.size, "HASH_MISMATCH");
        let target = self.checked_path(path, true)?;
        let current = self.current(path)?;
        // A repeated operation after a lost acknowledgment is harmless.
        if current.as_ref() == Some(entry) {
            return Ok(());
        }
        ensure!(
            current.as_ref().map(|e| e.hash.as_str()) == expected,
            "STALE_TARGET: {path}"
        );
        let mut temp = NamedTempFile::new_in(&self.private)?;
        std::io::copy(&mut File::open(staged)?, &mut temp)?;
        temp.as_file().sync_all()?;
        let id = random_id()?;
        let journal_path = self.private.join("recovery").join(format!("{id}.json"));
        let backup = self.private.join("recovery").join(&id);
        let mut journal = Journal {
            path: path.into(),
            backup: id,
            finished: false,
        };
        atomic_json(&journal_path, &journal)?;
        if current.is_some() {
            // Keep the actual displaced inode, including writes through already-open handles.
            fs::rename(&target, &backup)?;
            sync_dir(target.parent().unwrap())?;
            sync_dir(backup.parent().unwrap())?;
            let (displaced, _) = hash_reader(File::open(&backup)?)?;
            if Some(displaced.as_str()) != expected {
                self.recover()?;
                bail!("STALE_TARGET: changed at commit; preserved in recovery: {path}");
            }
        }
        // No clobber: if an editor recreated the path, preserve it and the displaced version.
        if let Err(e) = temp.persist_noclobber(&target) {
            self.recover()?;
            return Err(anyhow::anyhow!("STALE_TARGET: {path}: {e}"));
        }
        sync_dir(target.parent().unwrap())?;
        journal.finished = true;
        atomic_json(&journal_path, &journal)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conditional_write_hash_validation_and_retained_backup() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        fs::write(dir.path().join("a"), b"old").unwrap();
        let old = store.scan().unwrap()["a"].clone();
        let mut stage = NamedTempFile::new().unwrap();
        stage.write_all(b"new").unwrap();
        let (hash, size) = hash_reader(File::open(stage.path()).unwrap()).unwrap();
        let entry = Entry { hash, size };
        assert!(store.install("a", None, &entry, stage.path()).is_err());
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"old");
        store
            .install("a", Some(&old.hash), &entry, stage.path())
            .unwrap();
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"new");
        assert!(fs::read_dir(store.private.join("recovery"))
            .unwrap()
            .any(|p| fs::read(p.unwrap().path()).ok().as_deref() == Some(b"old")));
        let wrong = Entry {
            hash: "0".repeat(64),
            size,
        };
        assert!(store
            .install("a", Some(&entry.hash), &wrong, stage.path())
            .is_err());
    }
    #[test]
    fn crash_after_displacing_target_restores_backup() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        let id = "b".repeat(64);
        fs::write(store.private.join("recovery").join(&id), b"recover me").unwrap();
        let journal = Journal {
            path: "a".into(),
            backup: id.clone(),
            finished: false,
        };
        atomic_json(
            &store.private.join("recovery").join(format!("{id}.json")),
            &journal,
        )
        .unwrap();
        store.recover().unwrap();
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"recover me");
    }
    #[test]
    #[cfg(unix)]
    fn refuses_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        std::os::unix::fs::symlink(other.path(), dir.path().join("escape")).unwrap();
        assert!(store.scan().is_err());
        assert!(store.checked_path("escape/a", true).is_err());
    }
}
