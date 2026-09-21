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
    fn acknowledge(&mut self, _path: &str, _entry: &Entry) -> Result<()> {
        Ok(())
    }
    fn excluded(&self, _path: &str) -> bool {
        false
    }
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
    atomic_write(path, &serde_json::to_vec(value)?)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("state has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
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
    #[serde(default)]
    new_hash: Option<String>,
}

#[derive(Serialize)]
pub struct RecoveryEntry {
    pub id: String,
    pub path: String,
    pub backup_available: bool,
    pub finished: bool,
}

pub struct LocalStore {
    root: PathBuf,
    private: PathBuf,
    _lock: File,
    pub remote_ignore: String,
    ignore: crate::ignore::Ignore,
    cache: std::collections::BTreeMap<String, CachedEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedEntry {
    metadata: Vec<u64>,
    entry: Entry,
}

#[cfg(unix)]
fn fingerprint(meta: &fs::Metadata) -> Vec<u64> {
    use std::os::unix::fs::MetadataExt;
    vec![
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime() as u64,
        meta.mtime_nsec() as u64,
        meta.ctime() as u64,
        meta.ctime_nsec() as u64,
    ]
}
#[cfg(not(unix))]
fn fingerprint(_meta: &fs::Metadata) -> Vec<u64> {
    vec![]
}

impl LocalStore {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_impl(root, true)
    }
    pub fn open_recovery(root: &Path) -> Result<Self> {
        Self::open_impl(root, false)
    }
    fn open_impl(root: &Path, recover: bool) -> Result<Self> {
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
        let cache = File::open(private.join("cache.json"))
            .ok()
            .and_then(|f| serde_json::from_reader(f).ok())
            .unwrap_or_default();
        let ignore = crate::ignore::Ignore::parse(
            &fs::read_to_string(root.join(".rowdignore")).unwrap_or_default(),
        );
        let mut store = Self {
            ignore,
            root,
            private,
            _lock: lock,
            cache,
            remote_ignore: String::new(),
        };
        if recover {
            store.recover()?;
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn private(&self) -> &Path {
        &self.private
    }
    pub fn invalidate(&mut self) {
        self.cache.clear();
    }
    pub fn invalidate_path(&mut self, path: &str) {
        self.cache
            .retain(|p, _| p != path && !p.starts_with(&format!("{path}/")));
    }

    pub fn recovery_entries(&self) -> Result<Vec<RecoveryEntry>> {
        let mut entries = vec![];
        for file in fs::read_dir(self.private.join("recovery"))? {
            let path = file?.path();
            if path.extension().and_then(|p| p.to_str()) != Some("json") {
                continue;
            }
            let j: Journal = serde_json::from_reader(File::open(&path)?)?;
            crate::model::validate_hash(&j.backup)?;
            if self.private.join("recovery").join(&j.backup).exists() || !j.finished {
                entries.push(RecoveryEntry {
                    id: j.backup.clone(),
                    path: j.path,
                    backup_available: self.private.join("recovery").join(j.backup).exists(),
                    finished: j.finished,
                });
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }
    pub fn resolve_recovery(
        &mut self,
        id: &str,
        action: &str,
        output: Option<&Path>,
    ) -> Result<()> {
        crate::model::validate_hash(id)?;
        let record = self.private.join("recovery").join(format!("{id}.json"));
        let mut journal: Journal = serde_json::from_reader(File::open(&record)?)?;
        ensure!(journal.backup == id, "recovery identity mismatch");
        let backup = self.private.join("recovery").join(id);
        match action {
            "keep" => {
                journal.finished = true;
                atomic_json(&record, &journal)?;
            }
            "export" => {
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(output.context("export destination required")?)?;
                std::io::copy(&mut File::open(backup)?, &mut out)?;
                out.sync_all()?;
            }
            "restore" => {
                let current = self.current(&journal.path)?;
                let (hash, size) = hash_reader(File::open(&backup)?)?;
                self.install(
                    &journal.path,
                    current.as_ref().map(|e| e.hash.as_str()),
                    &Entry { hash, size },
                    &backup,
                )?;
                journal.finished = true;
                atomic_json(&record, &journal)?;
                self.invalidate();
            }
            _ => bail!("recovery action must be keep, restore or export"),
        }
        Ok(())
    }

    pub fn cleanup_recovery(&self, id: &str) -> Result<()> {
        crate::model::validate_hash(id)?;
        let directory = self.private.join("recovery");
        let record = directory.join(format!("{id}.json"));
        let journal: Journal = serde_json::from_reader(File::open(&record)?)?;
        ensure!(journal.backup == id, "recovery identity mismatch");
        ensure!(journal.finished, "pending recovery cannot be removed");
        let backup = directory.join(id);
        if backup.exists() {
            fs::remove_file(backup)?;
        }
        fs::remove_file(record)?;
        sync_dir(&directory)
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
            if target.is_file() {
                let (current, _) = hash_reader(File::open(&target)?)?;
                let restored = backup.is_file() && hash_reader(File::open(&backup)?)?.0 == current;
                ensure!(restored || journal.new_hash.as_deref() == Some(&current),
                    "RECOVERY_REQUIRED: {} · use rowd recovery --folder {} (keep, restore ou export)",journal.path,self.root.display());
            }
            journal.finished = true;
            atomic_json(&path, &journal)?;
        }
        Ok(())
    }
}

impl Store for LocalStore {
    fn excluded(&self, path: &str) -> bool {
        self.ignore.matches(path, false)
    }
    fn scan(&mut self) -> Result<Manifest> {
        fn walk(
            base: &Path,
            dir: &Path,
            result: &mut Manifest,
            ignore: &crate::ignore::Ignore,
            cache: &mut std::collections::BTreeMap<String, CachedEntry>,
        ) -> Result<()> {
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
                let kind = item.file_type()?;
                if ignore.matches(&rel, kind.is_dir()) {
                    continue;
                }
                validate_path(&rel)?;
                ensure!(!kind.is_symlink(), "symlink rejected: {rel}");
                if kind.is_dir() {
                    walk(base, &path, result, ignore, cache)?;
                } else if kind.is_file() {
                    let before = fingerprint(&fs::metadata(&path)?);
                    if let Some(cached) = cache
                        .get(&rel)
                        .filter(|c| !before.is_empty() && c.metadata == before)
                    {
                        result.insert(rel, cached.entry.clone());
                        continue;
                    }
                    let (hash, size) = hash_reader(File::open(&path)?)?;
                    let after = fingerprint(&fs::metadata(&path)?);
                    ensure!(before == after, "STALE_SOURCE: {rel}");
                    let entry = Entry { hash, size };
                    cache.insert(
                        rel.clone(),
                        CachedEntry {
                            metadata: after,
                            entry: entry.clone(),
                        },
                    );
                    result.insert(rel, entry);
                } else {
                    bail!("unsupported file: {rel}");
                }
            }
            Ok(())
        }
        let mut result = Manifest::new();
        let local_ignore = match fs::read_to_string(self.root.join(".rowdignore")) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let ignore =
            crate::ignore::Ignore::parse(&format!("{}\n{}", local_ignore, self.remote_ignore));
        self.ignore = ignore.clone();
        walk(
            &self.root,
            &self.root,
            &mut result,
            &ignore,
            &mut self.cache,
        )?;
        validate_manifest(&result)?;
        self.cache.retain(|path, _| result.contains_key(path));
        atomic_json(&self.private.join("cache.json"), &self.cache)?;
        Ok(result)
    }

    fn snapshot(&mut self, path: &str, expected: &Entry) -> Result<NamedTempFile> {
        ensure!(!self.excluded(path), "ignored path: {path}");
        let source = self.checked_path(path, false)?;
        let mut temp = NamedTempFile::new_in(&self.private)?;
        std::io::copy(&mut File::open(source)?, &mut temp)?;
        let (hash, size) = hash_reader(File::open(temp.path())?)?;
        if hash != expected.hash || size != expected.size {
            self.invalidate_path(path);
            atomic_json(&self.private.join("cache.json"), &self.cache)?;
            bail!("STALE_SOURCE: {path}; cache invalidated");
        }
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
        ensure!(!self.excluded(path), "ignored path: {path}");
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
            new_hash: Some(entry.hash.clone()),
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
            new_hash: None,
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

#[cfg(test)]
mod v2_tests {
    use super::*;
    #[test]
    fn ambiguous_recovery_blocks_scan_but_can_be_resolved() {
        let d = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(d.path()).unwrap();
        let id = "c".repeat(64);
        let recovery = store.private().join("recovery");
        fs::write(recovery.join(&id), "previous").unwrap();
        fs::write(d.path().join("a"), "unknown edit").unwrap();
        atomic_json(
            &recovery.join(format!("{id}.json")),
            &Journal {
                path: "a".into(),
                backup: id.clone(),
                finished: false,
                new_hash: Some("d".repeat(64)),
            },
        )
        .unwrap();
        assert!(store
            .recover()
            .unwrap_err()
            .to_string()
            .contains("RECOVERY_REQUIRED"));
        drop(store);
        assert!(LocalStore::open(d.path()).is_err());
        let mut store = LocalStore::open_recovery(d.path()).unwrap();
        store.resolve_recovery(&id, "restore", None).unwrap();
        drop(store);
        let store = LocalStore::open(d.path()).unwrap();
        assert_eq!(fs::read(d.path().join("a")).unwrap(), b"previous");
        assert!(store.recovery_entries().unwrap().len() >= 2);
    }
    #[test]
    fn cache_detects_same_size_changes_and_full_scan_rebuilds() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a"), "AAA").unwrap();
        let mut store = LocalStore::open(d.path()).unwrap();
        let old = store.scan().unwrap();
        drop(store);
        fs::write(d.path().join("a"), "BBB").unwrap();
        let mut store = LocalStore::open(d.path()).unwrap();
        let new = store.scan().unwrap();
        assert_ne!(old, new);
        store.invalidate();
        assert_eq!(store.scan().unwrap(), new);
        fs::write(d.path().join(".rowdignore"), "a\n").unwrap();
        assert!(store.scan().unwrap().is_empty());
    }
}
