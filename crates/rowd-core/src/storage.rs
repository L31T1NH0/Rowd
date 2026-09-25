use crate::{
    copy_and_hash, hash_reader,
    model::{validate_manifest, validate_path, Entry, Manifest},
    random_id,
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

pub struct VerifiedStaged {
    file: NamedTempFile,
    entry: Entry,
}

impl VerifiedStaged {
    pub fn from_digest(
        file: NamedTempFile,
        expected: &Entry,
        hash: &str,
        size: u64,
    ) -> Result<Self> {
        ensure!(
            hash == expected.hash && size == expected.size,
            "HASH_MISMATCH"
        );
        Ok(Self {
            file,
            entry: expected.clone(),
        })
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn entry(&self) -> &Entry {
        &self.entry
    }
}

pub type Snapshot = VerifiedStaged;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct StoreMetrics {
    pub files_enumerated: u64,
    pub files_hashed: u64,
    pub bytes_hashed: u64,
    pub staging_copies: u64,
    pub full_scans: u64,
}
pub trait Store {
    /// Hints only. Returning None always selects the full audit.
    fn delta_paths(&mut self) -> Result<Option<std::collections::BTreeSet<String>>> {
        Ok(None)
    }
    fn scan_paths(
        &mut self,
        _paths: &std::collections::BTreeSet<String>,
    ) -> Result<Option<Manifest>> {
        Ok(None)
    }
    fn base_token(&self) -> Option<String> {
        None
    }
    fn set_base_token(&mut self, _token: Option<String>) {}
    fn require_full_scan(&mut self) -> Result<()> {
        Ok(())
    }
    fn metrics(&self) -> StoreMetrics {
        StoreMetrics::default()
    }
    fn acknowledge(&mut self, _path: &str, _entry: &Entry) -> Result<()> {
        Ok(())
    }
    fn excluded(&self, _path: &str) -> bool {
        false
    }
    fn scan(&mut self) -> Result<Manifest>;
    fn snapshot(&mut self, path: &str, expected: &Entry) -> Result<Snapshot>;
    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: &VerifiedStaged,
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

fn read_root_policy(root: &Path) -> Result<String> {
    let path = root.join(".rowdignore");
    match fs::symlink_metadata(&path) {
        Ok(meta) => ensure!(
            !meta.file_type().is_symlink(),
            "symlink .rowdignore is not an authoritative Share policy"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error.into()),
    }
    Ok(fs::read_to_string(path)?)
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
    #[serde(default)]
    old_hash: Option<String>,
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
    policy_override: Option<String>,
    ignore: crate::ignore::Ignore,
    cache: std::collections::BTreeMap<String, CachedEntry>,
    cache_trusted: bool,
    incremental_allowed: bool,
    force_full_scan: bool,
    dirty_paths: std::collections::BTreeSet<String>,
    known_dirty_paths: std::collections::BTreeSet<String>,
    policy_text: String,
    cache_modified: bool,
    pending_cache_invalidation: bool,
    metrics: Cell<StoreMetrics>,
    base_token: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct CacheInvalidation {
    full: bool,
    paths: std::collections::BTreeSet<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedEntry {
    metadata: Vec<u64>,
    entry: Entry,
}

#[derive(Deserialize)]
struct CacheDisk {
    policy: String,
    files: std::collections::BTreeMap<String, CachedEntry>,
}

#[derive(Serialize)]
struct CacheDiskWrite<'a> {
    policy: &'a str,
    files: &'a std::collections::BTreeMap<String, CachedEntry>,
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
        Self::open_impl(root, true, None, true)
    }
    pub fn open_existing(root: &Path) -> Result<Self> {
        Self::open_impl(root, true, None, false)
    }
    pub fn open_recovery(root: &Path) -> Result<Self> {
        Self::open_impl(root, false, None, false)
    }
    #[cfg(any(test, feature = "dev-tools"))]
    pub fn open_with_policy(root: &Path, policy: &str) -> Result<Self> {
        crate::ignore::Ignore::validate(policy)?;
        Self::open_impl(root, true, Some(policy), true)
    }
    fn open_impl(
        root: &Path,
        recover: bool,
        policy: Option<&str>,
        create_root: bool,
    ) -> Result<Self> {
        if create_root {
            fs::create_dir_all(root)?;
        } else {
            ensure!(
                root.is_dir(),
                "Share root is unavailable: {}",
                root.display()
            );
            ensure!(
                root.canonicalize()? == root,
                "Share root identity changed: {}",
                root.display()
            );
        }
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
        let source = match policy {
            Some(policy) => policy.to_owned(),
            None => read_root_policy(&root)?,
        };
        crate::ignore::Ignore::validate(&source)?;
        let cache_bytes = fs::read(private.join("cache.json")).ok();
        let parsed_cache = cache_bytes
            .as_deref()
            .and_then(|bytes| serde_json::from_slice::<CacheDisk>(bytes).ok());
        let cache_trusted = parsed_cache
            .as_ref()
            .is_some_and(|cache| cache.policy == source);
        let cache = parsed_cache.map(|cache| cache.files).unwrap_or_else(|| {
            cache_bytes
                .as_deref()
                .and_then(|bytes| serde_json::from_slice(bytes).ok())
                .unwrap_or_default()
        });
        let ignore = crate::ignore::Ignore::parse(&source);
        let mut store = Self {
            ignore,
            root,
            private,
            _lock: lock,
            cache,
            cache_trusted,
            incremental_allowed: false,
            force_full_scan: false,
            dirty_paths: Default::default(),
            known_dirty_paths: Default::default(),
            policy_text: source,
            cache_modified: false,
            pending_cache_invalidation: false,
            metrics: Cell::new(StoreMetrics::default()),
            base_token: None,
            policy_override: policy.map(str::to_owned),
        };
        let invalidation_path = store.private.join("cache-dirty.json");
        if invalidation_path.try_exists()? {
            let invalidation: CacheInvalidation = File::open(&invalidation_path)
                .ok()
                .and_then(|file| serde_json::from_reader(file).ok())
                .unwrap_or(CacheInvalidation {
                    full: true,
                    paths: Default::default(),
                });
            if invalidation.full
                || invalidation
                    .paths
                    .iter()
                    .any(|path| validate_path(path).is_err())
            {
                store.invalidate();
                store.force_full_scan = true;
            } else {
                for path in &invalidation.paths {
                    store.invalidate_path(path);
                }
                store.dirty_paths = invalidation.paths;
            }
            store.pending_cache_invalidation = true;
        }
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
    /// Only the server with a live, trusted watcher may skip tree enumeration.
    pub fn allow_incremental_scan(&mut self) {
        #[cfg(unix)]
        {
            self.incremental_allowed = true;
        }
    }
    pub fn invalidate(&mut self) {
        self.cache_modified |= !self.cache.is_empty();
        self.cache.clear();
        self.force_full_scan = true;
    }
    pub fn invalidate_path(&mut self, path: &str) {
        self.known_dirty_paths.insert(path.to_owned());
        let before = self.cache.len();
        self.cache
            .retain(|p, _| p != path && !p.starts_with(&format!("{path}/")));
        self.cache_modified |= self.cache.len() != before;
        if validate_path(path).is_ok() {
            self.dirty_paths.insert(path.to_owned());
        } else {
            self.force_full_scan = true;
        }
    }

    pub fn persist_cache(&self) -> Result<()> {
        atomic_json(
            &self.private.join("cache.json"),
            &CacheDiskWrite {
                policy: &self.policy_text,
                files: &self.cache,
            },
        )
    }

    /// Durable watcher hint. The hash cache remains derived; a failed or malformed hint
    /// forces a full hash scan rather than trusting old cache entries.
    pub fn queue_cache_invalidation(
        root: &Path,
        full: bool,
        paths: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<()> {
        if !full && paths.is_none_or(|paths| paths.is_empty()) {
            return Ok(());
        }
        ensure!(
            root.is_dir() && root.canonicalize()? == root,
            "Share root identity changed"
        );
        let private = root.join(".rowd");
        if private.try_exists()? {
            ensure!(
                private.is_dir() && !fs::symlink_metadata(&private)?.file_type().is_symlink(),
                "invalid Share metadata directory"
            );
        } else {
            fs::create_dir(&private)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700))?;
        }
        let target = private.join("cache-dirty.json");
        let mut hint: CacheInvalidation = if target.try_exists()? {
            File::open(&target)
                .ok()
                .and_then(|file| serde_json::from_reader(file).ok())
                .unwrap_or(CacheInvalidation {
                    full: true,
                    paths: Default::default(),
                })
        } else {
            CacheInvalidation::default()
        };
        if full {
            hint.full = true;
        } else if !hint.full {
            for path in paths.into_iter().flatten() {
                if path.is_empty() || path == ".rowdignore" || validate_path(path).is_err() {
                    hint.full = true;
                    break;
                }
                hint.paths.insert(path.clone());
                if hint.paths.len() > 1024 {
                    hint.full = true;
                    break;
                }
            }
        }
        if hint.full {
            hint.paths.clear();
        }
        atomic_json(&target, &hint)
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
                let mut staged = NamedTempFile::new_in(&self.private)?;
                let (hash, size) = copy_and_hash(File::open(&backup)?, &mut staged)?;
                let entry = Entry { hash, size };
                let verified = VerifiedStaged::from_digest(staged, &entry, &entry.hash, size)?;
                self.install(
                    &journal.path,
                    current.as_ref().map(|e| e.hash.as_str()),
                    &entry,
                    &verified,
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
                let mut metrics = self.metrics.get();
                metrics.files_hashed += 1;
                metrics.bytes_hashed += size;
                self.metrics.set(metrics);
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
                let before_displacement =
                    !backup.exists() && journal.old_hash.as_deref() == Some(&current);
                ensure!(restored || before_displacement || journal.new_hash.as_deref() == Some(&current),
                    "RECOVERY_REQUIRED: {} in {} · use rowd recovery for this Share (keep, restore or export)",journal.path,self.root.display());
            } else {
                ensure!(
                    journal.old_hash.is_none() && !backup.exists(),
                    "RECOVERY_REQUIRED: {} · target and backup missing",
                    journal.path
                );
            }
            journal.finished = true;
            atomic_json(&path, &journal)?;
        }
        Ok(())
    }
}

impl Store for LocalStore {
    fn delta_paths(&mut self) -> Result<Option<std::collections::BTreeSet<String>>> {
        if !self.incremental_allowed || !self.cache_trusted || self.force_full_scan {
            return Ok(None);
        }
        let policy = match &self.policy_override {
            Some(policy) => policy.clone(),
            None => read_root_policy(&self.root)?,
        };
        if policy != self.policy_text || !self.dirty_paths.is_subset(&self.known_dirty_paths) {
            return Ok(None);
        }
        Ok(Some(self.dirty_paths.clone()))
    }
    fn scan_paths(
        &mut self,
        paths: &std::collections::BTreeSet<String>,
    ) -> Result<Option<Manifest>> {
        if self.delta_paths()?.is_none() || paths.iter().any(|p| validate_path(p).is_err()) {
            return Ok(None);
        }
        let mut files = Manifest::new();
        for path in paths {
            if self.excluded(path) {
                return Ok(None);
            }
            let target = self.checked_path(path, false)?;
            let before = match fs::symlink_metadata(&target) {
                Ok(meta) if meta.file_type().is_file() => fingerprint(&meta),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.cache.remove(path);
                    continue;
                }
                _ => return Ok(None),
            };
            let (hash, size) = hash_reader(File::open(&target)?)?;
            let after = fingerprint(&fs::metadata(&target)?);
            ensure!(before == after, "STALE_SOURCE: {path}");
            let entry = Entry { hash, size };
            self.cache.insert(
                path.clone(),
                CachedEntry {
                    metadata: after,
                    entry: entry.clone(),
                },
            );
            files.insert(path.clone(), entry);
            let mut counts = self.metrics.get();
            counts.files_enumerated += 1;
            counts.files_hashed += 1;
            counts.bytes_hashed += size;
            self.metrics.set(counts);
        }
        self.dirty_paths.clear();
        self.known_dirty_paths.clear();
        Ok(Some(files))
    }
    fn base_token(&self) -> Option<String> {
        self.base_token.clone()
    }
    fn set_base_token(&mut self, token: Option<String>) {
        self.base_token = token;
    }
    fn require_full_scan(&mut self) -> Result<()> {
        self.force_full_scan = true;
        Ok(())
    }
    fn metrics(&self) -> StoreMetrics {
        self.metrics.get()
    }
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
            metrics: &Cell<StoreMetrics>,
            cache_changed: &mut bool,
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
                    walk(base, &path, result, ignore, cache, metrics, cache_changed)?;
                } else if kind.is_file() {
                    let mut counts = metrics.get();
                    counts.files_enumerated += 1;
                    metrics.set(counts);
                    let before = fingerprint(&fs::metadata(&path)?);
                    if let Some(cached) = cache
                        .get(&rel)
                        .filter(|c| !before.is_empty() && c.metadata == before)
                    {
                        result.insert(rel, cached.entry.clone());
                        continue;
                    }
                    let (hash, size) = hash_reader(File::open(&path)?)?;
                    let mut counts = metrics.get();
                    counts.files_hashed += 1;
                    counts.bytes_hashed += size;
                    metrics.set(counts);
                    let after = fingerprint(&fs::metadata(&path)?);
                    ensure!(before == after, "STALE_SOURCE: {rel}");
                    let entry = Entry { hash, size };
                    *cache_changed = true;
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
        let policy = match &self.policy_override {
            Some(policy) => policy.clone(),
            None => read_root_policy(&self.root)?,
        };
        crate::ignore::Ignore::validate(&policy)?;
        let ignore = crate::ignore::Ignore::parse(&policy);
        self.ignore = ignore.clone();
        let mut cache_changed = false;
        let incremental = self.incremental_allowed
            && self.cache_trusted
            && !self.force_full_scan
            && policy == self.policy_text
            && self.dirty_paths.iter().all(|path| {
                matches!(
                    fs::symlink_metadata(self.root.join(path)),
                    Ok(metadata) if metadata.is_file()
                ) || !self.root.join(path).exists()
            });
        let mut result = if incremental {
            self.cache
                .iter()
                .map(|(path, cached)| (path.clone(), cached.entry.clone()))
                .collect()
        } else {
            Manifest::new()
        };
        if incremental {
            for path in &self.dirty_paths {
                if ignore.matches(path, false) {
                    continue;
                }
                let target = self.checked_path(path, false)?;
                match File::open(&target) {
                    Ok(file) => {
                        let before = fingerprint(&fs::metadata(&target)?);
                        let (hash, size) = hash_reader(file)?;
                        let after = fingerprint(&fs::metadata(&target)?);
                        ensure!(before == after, "STALE_SOURCE: {path}");
                        let mut counts = self.metrics.get();
                        counts.files_enumerated += 1;
                        counts.files_hashed += 1;
                        counts.bytes_hashed += size;
                        self.metrics.set(counts);
                        let entry = Entry { hash, size };
                        self.cache.insert(
                            path.clone(),
                            CachedEntry {
                                metadata: after,
                                entry: entry.clone(),
                            },
                        );
                        result.insert(path.clone(), entry);
                        cache_changed = true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            let mut counts = self.metrics.get();
            counts.full_scans += 1;
            self.metrics.set(counts);
            walk(
                &self.root,
                &self.root,
                &mut result,
                &ignore,
                &mut self.cache,
                &self.metrics,
                &mut cache_changed,
            )?;
        }
        validate_manifest(&result)?;
        let cache_len = self.cache.len();
        self.cache.retain(|path, _| result.contains_key(path));
        self.policy_text = policy;
        if cache_changed
            || self.cache.len() != cache_len
            || self.pending_cache_invalidation
            || self.force_full_scan
            || self.cache_modified
            || !self.cache_trusted
        {
            self.persist_cache()?;
        }
        if self.pending_cache_invalidation {
            fs::remove_file(self.private.join("cache-dirty.json"))?;
            self.pending_cache_invalidation = false;
        }
        self.dirty_paths.clear();
        self.known_dirty_paths.clear();
        self.force_full_scan = false;
        self.cache_modified = false;
        self.cache_trusted = true;
        Ok(result)
    }

    fn snapshot(&mut self, path: &str, expected: &Entry) -> Result<Snapshot> {
        ensure!(!self.excluded(path), "ignored path: {path}");
        let source = self.checked_path(path, false)?;
        let mut temp = NamedTempFile::new_in(&self.private)?;
        let (hash, size) = copy_and_hash(File::open(source)?, &mut temp)?;
        let mut metrics = self.metrics.get();
        metrics.staging_copies += 1;
        metrics.files_hashed += 1;
        metrics.bytes_hashed += size;
        self.metrics.set(metrics);
        if hash != expected.hash || size != expected.size {
            self.invalidate_path(path);
            self.persist_cache()?;
            bail!("STALE_SOURCE: {path}; cache invalidated");
        }
        VerifiedStaged::from_digest(temp, expected, &hash, size)
    }

    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: &VerifiedStaged,
    ) -> Result<()> {
        ensure!(staged.entry() == entry, "staged entry mismatch");
        let mut temp = NamedTempFile::new_in(&self.private)?;
        let size = std::io::copy(&mut File::open(staged.path())?, &mut temp)?;
        let mut metrics = self.metrics.get();
        metrics.staging_copies += 1;
        self.metrics.set(metrics);
        ensure!(size == entry.size, "staged size changed");
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
        temp.as_file().sync_all()?;
        let id = random_id()?;
        let journal_path = self.private.join("recovery").join(format!("{id}.json"));
        let backup = self.private.join("recovery").join(&id);
        let mut journal = Journal {
            path: path.into(),
            backup: id,
            finished: false,
            new_hash: Some(entry.hash.clone()),
            old_hash: current.as_ref().map(|entry| entry.hash.clone()),
        };
        atomic_json(&journal_path, &journal)?;
        if current.is_some() {
            // Keep the actual displaced inode, including writes through already-open handles.
            fs::rename(&target, &backup)?;
            sync_dir(target.parent().unwrap())?;
            sync_dir(backup.parent().unwrap())?;
            let (displaced, _) = hash_reader(File::open(&backup)?)?;
            let mut metrics = self.metrics.get();
            metrics.files_hashed += 1;
            metrics.bytes_hashed += backup.metadata()?.len();
            self.metrics.set(metrics);
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
        self.invalidate_path(path);
        Self::queue_cache_invalidation(
            &self.root,
            false,
            Some(&std::collections::BTreeSet::from([path.to_owned()])),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verified_staging_carries_the_checked_entry_and_cleans_up() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"content").unwrap();
        let path = file.path().to_path_buf();
        let (hash, size) = hash_reader(b"content".as_slice()).unwrap();
        let entry = Entry {
            hash: hash.clone(),
            size,
        };
        assert!(VerifiedStaged::from_digest(file, &entry, &hash, size + 1).is_err());
        assert!(!path.exists());

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"content").unwrap();
        let staged = VerifiedStaged::from_digest(file, &entry, &hash, size).unwrap();
        assert_eq!(staged.entry(), &entry);
        let path = staged.path().to_path_buf();
        drop(staged);
        assert!(!path.exists());
    }
    #[test]
    fn existing_share_open_never_creates_a_missing_root() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        assert!(LocalStore::open_existing(&missing).is_err());
        assert!(!missing.exists());
    }

    #[test]
    #[cfg(unix)]
    fn existing_share_open_rejects_root_redirect() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let redirected = directory.path().join("redirected");
        symlink(&destination, &redirected).unwrap();
        assert!(LocalStore::open_existing(&redirected).is_err());
        assert!(!destination.join(".rowd").exists());
    }

    #[test]
    #[cfg(unix)]
    fn local_store_rejects_symlinked_ignore_policy() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("external"), "secret/\n").unwrap();
        symlink(
            directory.path().join("external"),
            directory.path().join(".rowdignore"),
        )
        .unwrap();
        assert!(LocalStore::open(directory.path()).is_err());
    }

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
        let stage = VerifiedStaged::from_digest(stage, &entry, &entry.hash, size).unwrap();
        assert!(store.install("a", None, &entry, &stage).is_err());
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"old");
        store.install("a", Some(&old.hash), &entry, &stage).unwrap();
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"new");
        assert!(fs::read_dir(store.private.join("recovery"))
            .unwrap()
            .any(|p| fs::read(p.unwrap().path()).ok().as_deref() == Some(b"old")));
        let wrong = Entry {
            hash: "0".repeat(64),
            size,
        };
        assert!(store
            .install("a", Some(&entry.hash), &wrong, &stage)
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
            old_hash: None,
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
    fn recovery_recognizes_each_publish_boundary() {
        for boundary in ["before_rename", "after_rename", "after_publish"] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = LocalStore::open(dir.path()).unwrap();
            let id = "e".repeat(64);
            let target = dir.path().join("a");
            let backup = store.private.join("recovery").join(&id);
            fs::write(&target, b"old").unwrap();
            let old_hash = hash_reader(File::open(&target).unwrap()).unwrap().0;
            let new_hash = hash_reader(b"new".as_slice()).unwrap().0;
            let record = store.private.join("recovery").join(format!("{id}.json"));
            atomic_json(
                &record,
                &Journal {
                    path: "a".into(),
                    backup: id,
                    finished: false,
                    new_hash: Some(new_hash),
                    old_hash: Some(old_hash),
                },
            )
            .unwrap();
            if boundary != "before_rename" {
                fs::rename(&target, &backup).unwrap();
            }
            if boundary == "after_publish" {
                fs::write(&target, b"new").unwrap();
            }
            store.recover().unwrap();
            assert_eq!(
                fs::read(&target).unwrap(),
                if boundary == "after_publish" {
                    b"new"
                } else {
                    b"old"
                }
            );
            assert!(
                serde_json::from_reader::<_, Journal>(File::open(record).unwrap())
                    .unwrap()
                    .finished
            );
        }
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
                old_hash: None,
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
    fn full_invalidation_persists_an_empty_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a");
        fs::write(&source, b"content").unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        store.scan().unwrap();
        fs::remove_file(&source).unwrap();
        store.invalidate();
        assert!(store.scan().unwrap().is_empty());
        drop(store);
        let mut store = LocalStore::open(dir.path()).unwrap();
        store.allow_incremental_scan();
        assert!(store.scan().unwrap().is_empty());
        let disk: serde_json::Value =
            serde_json::from_slice(&fs::read(store.private().join("cache.json")).unwrap()).unwrap();
        assert!(disk["files"].as_object().unwrap().is_empty());

        fs::write(&source, b"again").unwrap();
        store.invalidate();
        store.scan().unwrap();
        fs::remove_file(&source).unwrap();
        store.invalidate_path("a");
        assert!(store.scan().unwrap().is_empty());
        let disk: serde_json::Value =
            serde_json::from_slice(&fs::read(store.private().join("cache.json")).unwrap()).unwrap();
        assert!(disk["files"].as_object().unwrap().is_empty());
    }
    #[test]
    fn legacy_or_wrong_policy_cache_never_enables_incremental_scan() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), b"content").unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        store.scan().unwrap();
        let cache = store.private().join("cache.json");
        drop(store);

        let disk: serde_json::Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
        atomic_json(&cache, &disk["files"]).unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        store.allow_incremental_scan();
        store.scan().unwrap();
        assert_eq!(store.metrics().full_scans, 1);
        drop(store);

        let mut disk: serde_json::Value =
            serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
        disk["policy"] = "different policy".into();
        atomic_json(&cache, &disk).unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        store.allow_incremental_scan();
        store.scan().unwrap();
        assert_eq!(store.metrics().full_scans, 1);
        let rebuilt: serde_json::Value =
            serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
        assert_eq!(rebuilt["policy"], "");
    }
    #[test]
    fn watcher_hint_is_small_durable_and_rechecked_on_scan() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), b"old").unwrap();
        fs::write(dir.path().join("b"), b"stable").unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        let before = store.scan().unwrap();
        let cache_path = store.private().join("cache.json");
        let cache_bytes = fs::read(&cache_path).unwrap();
        drop(store);

        fs::write(dir.path().join("a"), b"new").unwrap();
        LocalStore::queue_cache_invalidation(
            &dir.path().canonicalize().unwrap(),
            false,
            Some(&std::collections::BTreeSet::from(["a".into()])),
        )
        .unwrap();
        assert_eq!(fs::read(&cache_path).unwrap(), cache_bytes);
        let mut store = LocalStore::open(dir.path()).unwrap();
        store.allow_incremental_scan();
        let after = store.scan().unwrap();
        assert_ne!(before["a"], after["a"]);
        assert_eq!(before["b"], after["b"]);
        #[cfg(unix)]
        {
            assert_eq!(store.metrics().files_enumerated, 1);
            assert_eq!(store.metrics().full_scans, 0);
        }
        assert!(!store.private().join("cache-dirty.json").exists());
        drop(store);

        LocalStore::queue_cache_invalidation(&dir.path().canonicalize().unwrap(), false, None)
            .unwrap();
        assert!(!dir.path().join(".rowd/cache-dirty.json").exists());

        fs::remove_file(dir.path().join("b")).unwrap();
        fs::write(dir.path().join("c"), b"added").unwrap();
        LocalStore::queue_cache_invalidation(
            &dir.path().canonicalize().unwrap(),
            false,
            Some(&std::collections::BTreeSet::from(["b".into(), "c".into()])),
        )
        .unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        store.allow_incremental_scan();
        let changed = store.scan().unwrap();
        assert!(!changed.contains_key("b"));
        assert!(changed.contains_key("c"));
        drop(store);

        fs::write(dir.path().join("a"), b"newer").unwrap();
        fs::write(dir.path().join(".rowd/cache-dirty.json"), b"broken").unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        store.allow_incremental_scan();
        assert_ne!(after["a"], store.scan().unwrap()["a"]);
        assert_eq!(store.metrics().full_scans, 1);
    }
    #[test]
    #[cfg(unix)]
    fn unchanged_scan_does_not_rewrite_hash_cache() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a");
        fs::write(&source, b"old").unwrap();
        let mut store = LocalStore::open(dir.path()).unwrap();
        store.scan().unwrap();
        let cache = store.private().join("cache.json");
        let first = fs::metadata(&cache).unwrap().ino();
        store.scan().unwrap();
        assert_eq!(fs::metadata(&cache).unwrap().ino(), first);

        fs::write(&source, b"changed").unwrap();
        store.scan().unwrap();
        let second = fs::metadata(&cache).unwrap().ino();
        assert_ne!(second, first);
        fs::remove_file(&source).unwrap();
        assert!(store.scan().unwrap().is_empty());
        assert_ne!(fs::metadata(&cache).unwrap().ino(), second);
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
