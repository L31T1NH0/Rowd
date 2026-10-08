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
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

pub struct VerifiedStaged {
    file: NamedTempFile,
    entry: Entry,
    metadata: Vec<u64>,
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
        let metadata = file.as_file().metadata()?;
        ensure!(metadata.len() == size, "staged size changed");
        Ok(Self {
            file,
            entry: expected.clone(),
            metadata: fingerprint(&metadata),
        })
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn entry(&self) -> &Entry {
        &self.entry
    }

    fn validate(&self) -> Result<()> {
        let metadata = fs::symlink_metadata(self.path())?;
        ensure!(
            metadata.is_file()
                && metadata.len() == self.entry.size
                && fingerprint(&metadata) == self.metadata
                && fingerprint(&self.file.as_file().metadata()?) == self.metadata,
            "staged file changed after verification"
        );
        Ok(())
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

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanStreamMetrics {
    // Wire milliseconds: u64 covers over 584 million years and is supported
    // by serde's internally tagged Message decoder. Producers must check narrowing.
    pub namespace_ms: u64,
    pub time_to_first_hash_ms: Option<u64>,
    pub hashes_calculated: u64,
    pub hashes_reused: u64,
    pub files_staged_during_hash: u64,
    pub duplicate_reads_avoided: u64,
    pub queue_peak_chunks: u64,
    pub queue_wait_ms: u64,
    pub saf_source_lookup_fallback_count: u64,
}
pub trait Store {
    /// Receive on the destination filesystem when the store can publish a local file.
    fn staging_directory(&self) -> Option<PathBuf> {
        None
    }
    /// Consume a received payload; borrowed snapshots remain available for conflict forwarding.
    fn install_received(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: VerifiedStaged,
    ) -> Result<()> {
        self.install(path, expected, entry, &staged)
    }
    fn validate_scan_snapshot(&mut self) -> Result<()> {
        Ok(())
    }
    fn scan_stream_metrics(&mut self) -> Result<ScanStreamMetrics> {
        Ok(ScanStreamMetrics::default())
    }
    fn scan_binding(&mut self) -> Result<String> {
        Ok(String::new())
    }
    /// None retains the synchronous implementation for simple stores. SAF overrides
    /// this with enumeration only; hash production starts after global validation.
    fn stream_namespace(
        &mut self,
        _io: &mut (impl Read + Write),
    ) -> Result<Option<crate::model::Namespace>> {
        Ok(None)
    }
    fn start_hash_stream(
        &mut self,
        _stage_paths: &std::collections::BTreeSet<String>,
    ) -> Result<()> {
        Ok(())
    }
    fn next_hash_chunk(&mut self, _io: &mut (impl Read + Write)) -> Result<Option<Manifest>> {
        Ok(None)
    }
    fn release_hash_staging(&mut self, _paths: &std::collections::BTreeSet<String>) -> Result<()> {
        Ok(())
    }
    fn scan_is_staged(&self) -> bool {
        false
    }
    fn scan_with_control(&mut self, _io: &mut (impl Read + Write)) -> Result<Manifest> {
        self.scan()
    }
    fn commit_scan(&mut self) -> Result<()> {
        Ok(())
    }
    fn discard_scan(&mut self) -> Result<()> {
        Ok(())
    }
    /// Hints only. Returning None selects a full manifest; scan() decides whether
    /// its cache is trusted or a physical audit is required.
    fn delta_paths(&mut self) -> Result<Option<std::collections::BTreeSet<String>>> {
        Ok(None)
    }
    fn scan_paths(
        &mut self,
        _paths: &std::collections::BTreeSet<String>,
    ) -> Result<Option<Manifest>> {
        Ok(None)
    }
    fn delta_scan_with_control(
        &mut self,
        _io: &mut (impl Read + Write),
        paths: &std::collections::BTreeSet<String>,
    ) -> Result<Option<(std::collections::BTreeSet<String>, Manifest)>> {
        let Some(dirty) = self.delta_paths()? else {
            return Ok(None);
        };
        let union: std::collections::BTreeSet<_> = paths.union(&dirty).cloned().collect();
        if union.len() > 1024 || union.iter().any(|path| validate_path(path).is_err()) {
            return Ok(None);
        }
        // A failed local focused read falls back to full namespace validation.
        // Controlled implementations must propagate errors from their transport.
        Ok(self
            .scan_paths(&union)
            .ok()
            .flatten()
            .map(|files| (union, files)))
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
    fn legacy_hash_with_control(
        &mut self,
        _io: &mut (impl Read + Write),
        path: &str,
        entry: &Entry,
    ) -> Result<String> {
        let snapshot = self.snapshot(path, entry)?;
        Ok(crate::legacy_hash_reader(File::open(snapshot.path())?)?.0)
    }
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
    #[serde(default)]
    blake3: bool,
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
    pub bytes: u64,
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
    stream_queue: Option<std::collections::VecDeque<(String, Vec<u64>)>>,
    stream_paths: std::collections::BTreeSet<String>,
    stream_original: Option<crate::model::Namespace>,
    stream_fingerprints: std::collections::BTreeMap<String, Vec<u64>>,
    stream_installed: Manifest,
    stream_installed_fingerprints: std::collections::BTreeMap<String, Vec<u64>>,
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
    #[serde(default)]
    blake3: bool,
    policy: String,
    files: std::collections::BTreeMap<String, CachedEntry>,
}

#[derive(Serialize)]
struct CacheDiskWrite<'a> {
    blake3: bool,
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

fn enumerate_namespace(
    root: &Path,
    ignore: &crate::ignore::Ignore,
) -> Result<crate::model::Namespace> {
    fn walk(
        base: &Path,
        dir: &Path,
        ignore: &crate::ignore::Ignore,
        result: &mut crate::model::Namespace,
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
                .context("non UTF-8 filename")?;
            #[cfg(windows)]
            let rel = rel.replace('\\', "/");
            #[cfg(not(windows))]
            let rel = rel.to_owned();
            let kind = item.file_type()?;
            if ignore.matches(&rel, kind.is_dir()) {
                continue;
            }
            validate_path(&rel)?;
            ensure!(
                !kind.is_symlink() && (kind.is_dir() || kind.is_file()),
                "unsupported file: {rel}"
            );
            let metadata = fs::metadata(&path)?;
            result.insert(
                rel,
                crate::model::NamespaceEntry {
                    size: metadata.len(),
                    modified: metadata
                        .modified()?
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    directory: kind.is_dir(),
                },
            );
            ensure!(
                result.len() <= crate::model::MAX_FILES * 2,
                "too many namespace entries"
            );
            if kind.is_dir() {
                walk(base, &path, ignore, result)?;
            }
        }
        Ok(())
    }
    let mut result = crate::model::Namespace::new();
    walk(root, root, ignore, &mut result)?;
    crate::model::validate_namespace(&result)?;
    Ok(result)
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
            .and_then(|bytes| serde_json::from_slice::<CacheDisk>(bytes).ok())
            .filter(|cache| cache.blake3);
        let cache_trusted = parsed_cache
            .as_ref()
            .is_some_and(|cache| cache.policy == source);
        let cache = parsed_cache.map(|cache| cache.files).unwrap_or_default();
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
            stream_queue: None,
            stream_paths: Default::default(),
            stream_original: None,
            stream_fingerprints: Default::default(),
            stream_installed: Default::default(),
            stream_installed_fingerprints: Default::default(),
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
                blake3: true,
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
        Self::read_recovery_entries(&self.root)
    }

    /// UI snapshots only read atomic journals; they must never claim the writer lock.
    pub fn read_recovery_entries(root: &Path) -> Result<Vec<RecoveryEntry>> {
        ensure!(
            root.is_dir() && root.canonicalize()? == root,
            "Share root is unavailable or changed"
        );
        let private = root.join(".rowd");
        let directory = private.join("recovery");
        for path in [&private, &directory] {
            match fs::symlink_metadata(path) {
                Ok(meta) => ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "unsafe recovery directory"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(e) => return Err(e.into()),
            }
        }
        let mut entries = vec![];
        for file in fs::read_dir(&directory)? {
            let path = file?.path();
            if path.extension().and_then(|p| p.to_str()) != Some("json") {
                continue;
            }
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, // Concurrent cleanup.
                Err(e) => return Err(e.into()),
            };
            let j: Journal = serde_json::from_reader(file)?;
            crate::model::validate_hash(&j.backup)?;
            let backup = directory.join(&j.backup);
            let metadata = backup.symlink_metadata().ok();
            let available = metadata
                .as_ref()
                .is_some_and(|m| m.is_file() && !m.file_type().is_symlink());
            if available || !j.finished {
                entries.push(RecoveryEntry {
                    id: j.backup,
                    path: j.path,
                    backup_available: available,
                    finished: j.finished,
                    bytes: metadata.filter(|_| available).map_or(0, |m| m.len()),
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
                let digest = |path: &Path| {
                    if journal.blake3 {
                        hash_reader(File::open(path)?)
                    } else {
                        crate::legacy_hash_reader(File::open(path)?)
                    }
                };
                let (current, _) = digest(&target)?;
                let restored = backup.is_file() && digest(&backup)?.0 == current;
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
    fn remember_installed(
        &mut self,
        path: &str,
        entry: &Entry,
        target: &Path,
        metadata: Vec<u64>,
    ) -> Result<()> {
        ensure!(
            metadata == fingerprint(&fs::metadata(target)?),
            "STALE_TARGET: installed file edited: {path}"
        );
        if self.stream_original.is_some() {
            self.stream_installed.insert(path.into(), entry.clone());
            self.stream_installed_fingerprints
                .insert(path.into(), metadata);
        }
        Ok(())
    }

    fn install_file(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        temp: NamedTempFile,
        verified_metadata: Vec<u64>,
    ) -> Result<()> {
        ensure!(!self.excluded(path), "ignored path: {path}");
        validate_path(path)?;
        let internal = crate::internal_writes::register(&self.root.join(path), &self.root, entry);
        let target = self.checked_path(path, true)?;
        let target_before = fs::symlink_metadata(&target).ok().map(|m| fingerprint(&m));
        let current = self.current(path)?;
        ensure!(
            target_before == fs::symlink_metadata(&target).ok().map(|m| fingerprint(&m)),
            "STALE_TARGET: changed during validation: {path}"
        );
        // A repeated operation after a lost acknowledgment is harmless.
        if current.as_ref() == Some(entry) {
            crate::trace::remote_installed(path, &entry.hash);
            self.remember_installed(path, entry, &target, target_before.unwrap())?;
            internal.complete();
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
            blake3: true,
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
        let before_publish = fingerprint(&temp.as_file().metadata()?);
        ensure!(
            before_publish == verified_metadata
                && fingerprint(&fs::symlink_metadata(temp.path())?) == verified_metadata
                && temp.as_file().metadata()?.len() == entry.size,
            "staged file changed before publication"
        );
        let published = match temp.persist_noclobber(&target) {
            Ok(file) => file,
            Err(e) => {
                self.recover()?;
                return Err(anyhow::anyhow!("STALE_TARGET: {path}: {e}"));
            }
        };
        let installed = fingerprint(&published.metadata()?);
        // Publication can change ctime; inode, size and mtime must still describe the verified bytes.
        ensure!(
            before_publish.get(..5) == installed.get(..5),
            "STALE_TARGET: file changed at publication: {path}"
        );
        sync_dir(target.parent().unwrap())?;
        journal.finished = true;
        atomic_json(&journal_path, &journal)?;
        crate::trace::remote_installed(path, &entry.hash);
        self.remember_installed(path, entry, &target, installed)?;
        internal.complete();
        self.invalidate_path(path);
        Self::queue_cache_invalidation(
            &self.root,
            false,
            Some(&std::collections::BTreeSet::from([path.to_owned()])),
        )?;
        Ok(())
    }
    fn validate_installed(&self, path: &str, entry: &Entry) -> Result<()> {
        let target = self.checked_path(path, false)?;
        let before = fingerprint(&fs::metadata(&target)?);
        if !before.is_empty() && self.stream_installed_fingerprints.get(path) == Some(&before) {
            return Ok(());
        }
        let (hash, size) = hash_reader(File::open(&target)?)?;
        let mut metrics = self.metrics.get();
        metrics.files_hashed += 1;
        metrics.bytes_hashed += size;
        self.metrics.set(metrics);
        ensure!(
            entry.hash == hash
                && entry.size == size
                && before == fingerprint(&fs::metadata(&target)?),
            "STALE_TARGET: installed file edited during stream: {path}"
        );
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
            let _file = crate::trace::current_context().for_path(path).enter();
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
            let metadata = fs::metadata(&target)?;
            let after = fingerprint(&metadata);
            ensure!(before == after, "STALE_SOURCE: {path}");
            crate::trace::observed_file(path, "focused_scan", &metadata, &hash);
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
    fn scan_binding(&mut self) -> Result<String> {
        let policy = self
            .policy_override
            .clone()
            .map(Ok)
            .unwrap_or_else(|| read_root_policy(&self.root))?;
        let root_metadata = fs::symlink_metadata(&self.root)?;
        ensure!(
            root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
            "Share root changed"
        );
        let identity = fingerprint(&root_metadata)
            .into_iter()
            .take(2)
            .collect::<Vec<_>>();
        Ok(format!("{}|{identity:?}|{}", self.root.display(), policy))
    }
    fn stream_namespace(
        &mut self,
        _io: &mut (impl Read + Write),
    ) -> Result<Option<crate::model::Namespace>> {
        let policy = self
            .policy_override
            .clone()
            .map(Ok)
            .unwrap_or_else(|| read_root_policy(&self.root))?;
        crate::ignore::Ignore::validate(&policy)?;
        self.ignore = crate::ignore::Ignore::parse(&policy);
        let result = enumerate_namespace(&self.root, &self.ignore)?;
        self.stream_queue = Some(
            result
                .iter()
                .filter(|(_, e)| !e.directory)
                .map(|(p, _)| {
                    Ok((
                        p.clone(),
                        fingerprint(&fs::metadata(self.checked_path(p, false)?)?),
                    ))
                })
                .collect::<Result<_>>()?,
        );
        self.stream_fingerprints = self
            .stream_queue
            .as_ref()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        self.stream_original = Some(result.clone());
        self.stream_installed.clear();
        self.stream_installed_fingerprints.clear();
        self.stream_paths = result
            .iter()
            .filter(|(_, e)| !e.directory)
            .map(|(p, _)| p.clone())
            .collect();
        self.policy_text = policy;
        Ok(Some(result))
    }
    fn validate_scan_snapshot(&mut self) -> Result<()> {
        let Some(original) = &self.stream_original else {
            return Ok(());
        };
        let mut expected = original.clone();
        for (path, entry) in &self.stream_installed {
            expected.insert(
                path.clone(),
                crate::model::NamespaceEntry {
                    size: entry.size,
                    modified: 0,
                    directory: false,
                },
            );
            for (index, _) in path.match_indices('/') {
                expected
                    .entry(path[..index].into())
                    .or_insert(crate::model::NamespaceEntry {
                        size: 0,
                        modified: 0,
                        directory: true,
                    });
            }
        }
        let observed = enumerate_namespace(&self.root, &self.ignore)?;
        ensure!(
            observed.len() == expected.len()
                && observed
                    .iter()
                    .all(|(p, e)| expected.get(p).is_some_and(
                        |n| n.directory == e.directory && (n.directory || n.size == e.size)
                    )),
            "STALE_SOURCE: namespace changed during stream"
        );
        for (path, old) in &self.stream_fingerprints {
            let target = self.checked_path(path, false)?;
            if let Some(entry) = self.stream_installed.get(path) {
                self.validate_installed(path, entry)?;
            } else {
                ensure!(
                    *old == fingerprint(&fs::metadata(&target)?),
                    "STALE_SOURCE: file changed during stream: {path}"
                );
            }
        }
        for (path, entry) in self
            .stream_installed
            .iter()
            .filter(|(p, _)| !self.stream_fingerprints.contains_key(*p))
        {
            self.validate_installed(path, entry)?;
        }
        Ok(())
    }
    fn start_hash_stream(
        &mut self,
        _stage_paths: &std::collections::BTreeSet<String>,
    ) -> Result<()> {
        let mut metrics = self.metrics.get();
        metrics.full_scans += 1;
        metrics.files_enumerated += self.stream_paths.len() as u64;
        self.metrics.set(metrics);
        Ok(())
    }
    fn next_hash_chunk(&mut self, _io: &mut (impl Read + Write)) -> Result<Option<Manifest>> {
        let mut files = Manifest::new();
        while files.len() < crate::protocol::SCAN_STREAM_CHUNK_FILES {
            let Some((path, original)) = self
                .stream_queue
                .as_mut()
                .context("no namespace scan")?
                .pop_front()
            else {
                break;
            };
            let source = self.checked_path(&path, false)?;
            let before = fingerprint(
                &fs::metadata(&source).with_context(|| format!("STALE_SOURCE: {path}"))?,
            );
            ensure!(before == original, "STALE_SOURCE: {path}");
            let entry = if let Some(cached) = self
                .cache
                .get(&path)
                .filter(|c| !before.is_empty() && c.metadata == before)
            {
                cached.entry.clone()
            } else {
                let (hash, size) = hash_reader(File::open(&source)?)?;
                ensure!(
                    fingerprint(&fs::metadata(&source)?) == before,
                    "STALE_SOURCE: {path}"
                );
                let entry = Entry { hash, size };
                self.cache.insert(
                    path.clone(),
                    CachedEntry {
                        metadata: before,
                        entry: entry.clone(),
                    },
                );
                let mut metrics = self.metrics.get();
                metrics.files_hashed += 1;
                metrics.bytes_hashed += size;
                self.metrics.set(metrics);
                entry
            };
            files.insert(path, entry);
        }
        Ok((!files.is_empty()).then_some(files))
    }
    fn discard_scan(&mut self) -> Result<()> {
        self.stream_queue = None;
        self.stream_original = None;
        self.stream_fingerprints.clear();
        self.stream_installed.clear();
        self.stream_installed_fingerprints.clear();
        self.persist_cache()
    }
    fn commit_scan(&mut self) -> Result<()> {
        self.validate_scan_snapshot()?;
        self.stream_queue = None;
        self.cache.retain(|p, _| self.stream_paths.contains(p));
        self.cache_trusted = true;
        self.force_full_scan = false;
        self.cache_modified = false;
        self.pending_cache_invalidation = false;
        self.dirty_paths.clear();
        self.known_dirty_paths.clear();
        self.persist_cache()
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
                    .context("non UTF-8 filename")?;
                #[cfg(windows)]
                let rel = rel.replace('\\', "/");
                #[cfg(not(windows))]
                let rel = rel.to_owned();
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
                    let metadata = fs::metadata(&path)?;
                    let _file = crate::trace::current_context()
                        .for_path(&rel)
                        .with("relative_path", rel.clone())
                        .enter();
                    crate::trace_event!(
                        crate::trace::Level::Trace,
                        crate::trace::Component::Scanner,
                        "FILE_ENUMERATED",
                        serde_json::json!({"relative_path":rel})
                    );
                    let before = fingerprint(&metadata);
                    if let Some(cached) = cache
                        .get(&rel)
                        .filter(|c| !before.is_empty() && c.metadata == before)
                    {
                        crate::trace_event!(
                            crate::trace::Level::Debug,
                            crate::trace::Component::Scanner,
                            "FILE_HASH_REUSED",
                            serde_json::json!({"relative_path":rel,"reason":"metadata_matches"})
                        );
                        crate::trace::observed_file(
                            &rel,
                            "audit_scan",
                            &metadata,
                            &cached.entry.hash,
                        );
                        result.insert(rel, cached.entry.clone());
                        continue;
                    }
                    crate::trace_event!(
                        crate::trace::Level::Trace,
                        crate::trace::Component::Scanner,
                        "FILE_HASH_START",
                        serde_json::json!({"relative_path":rel,"reason":"cache_missing_or_metadata_changed"})
                    );
                    let hashing = std::time::Instant::now();
                    let (hash, size) = hash_reader(File::open(&path)?)?;
                    crate::trace_event!(
                        crate::trace::Level::Trace,
                        crate::trace::Component::Scanner,
                        "FILE_HASH_END",
                        serde_json::json!({"relative_path":rel,"bytes":size,"duration_us":hashing.elapsed().as_micros()})
                    );
                    let mut counts = metrics.get();
                    counts.files_hashed += 1;
                    counts.bytes_hashed += size;
                    metrics.set(counts);
                    let after = fingerprint(&fs::metadata(&path)?);
                    ensure!(before == after, "STALE_SOURCE: {rel}");
                    crate::trace::observed_file(&rel, "audit_scan", &metadata, &hash);
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
        crate::trace_event!(
            crate::trace::Level::Debug,
            crate::trace::Component::Scanner,
            if incremental {
                "DELTA_SCAN_START"
            } else {
                "FULL_SCAN_FALLBACK"
            },
            serde_json::json!({"reason":if incremental {"trusted_cache_with_dirty_paths"}else{"cache_or_namespace_requires_full_scan"},"incremental_allowed":self.incremental_allowed,"cache_trusted":self.cache_trusted,"force_full_scan":self.force_full_scan,"policy_matches":policy==self.policy_text,"dirty_path_count":self.dirty_paths.len()})
        );
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
                let _file = crate::trace::current_context().for_path(path).enter();
                if ignore.matches(path, false) {
                    continue;
                }
                let target = self.checked_path(path, false)?;
                match File::open(&target) {
                    Ok(file) => {
                        let metadata = fs::metadata(&target)?;
                        let before = fingerprint(&metadata);
                        let (hash, size) = hash_reader(file)?;
                        let after = fingerprint(&fs::metadata(&target)?);
                        ensure!(before == after, "STALE_SOURCE: {path}");
                        crate::trace::observed_file(path, "focused_scan", &metadata, &hash);
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

    fn staging_directory(&self) -> Option<PathBuf> {
        Some(self.private.clone())
    }

    fn install_received(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: VerifiedStaged,
    ) -> Result<()> {
        ensure!(staged.entry() == entry, "staged entry mismatch");
        staged.validate()?;
        if staged.path().parent() == Some(self.private.as_path()) && !staged.metadata.is_empty() {
            self.install_file(path, expected, entry, staged.file, staged.metadata)
        } else {
            // SAF and foreign/cross-filesystem sources keep the verified-copy fallback.
            self.install(path, expected, entry, &staged)
        }
    }

    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: &VerifiedStaged,
    ) -> Result<()> {
        ensure!(staged.entry() == entry, "staged entry mismatch");
        staged.validate()?;
        let mut temp = NamedTempFile::new_in(&self.private)?;
        let (hash, size) = copy_and_hash(File::open(staged.path())?, &mut temp)?;
        let mut metrics = self.metrics.get();
        metrics.staging_copies += 1;
        metrics.files_hashed += 1;
        metrics.bytes_hashed += size;
        self.metrics.set(metrics);
        ensure!(
            hash == entry.hash && size == entry.size,
            "staged content changed"
        );
        staged.validate()?;
        let metadata = fingerprint(&temp.as_file().metadata()?);
        self.install_file(path, expected, entry, temp, metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn scanners_reject_literal_backslashes_without_aliasing_nested_paths() {
        for invalid in [r"foo\bar.txt", r"foo\bar/file.txt"] {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir_all(root.path().join("foo/bar")).unwrap();
            fs::write(root.path().join("foo/bar.txt"), b"same").unwrap();
            fs::write(root.path().join("foo/bar/file.txt"), b"same").unwrap();
            let mut store = LocalStore::open(root.path()).unwrap();
            assert_eq!(store.scan().unwrap().len(), 2);
            let namespace = enumerate_namespace(root.path(), &store.ignore).unwrap();
            assert!(namespace.contains_key("foo/bar.txt"));
            assert!(namespace.contains_key("foo/bar/file.txt"));

            let invalid = root.path().join(invalid);
            fs::create_dir_all(invalid.parent().unwrap()).unwrap();
            fs::write(invalid, b"same").unwrap();
            assert!(store
                .scan()
                .unwrap_err()
                .to_string()
                .contains("unsafe filename"));
            assert!(enumerate_namespace(root.path(), &store.ignore)
                .unwrap_err()
                .to_string()
                .contains("unsafe filename"));
        }
    }

    #[test]
    #[cfg(unix)]
    fn old_timestamps_are_zero_on_wire_and_preserved_for_snapshot_validation() {
        use std::time::{Duration, UNIX_EPOCH};
        for modified in [UNIX_EPOCH, UNIX_EPOCH - Duration::from_secs(1)] {
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join("old");
            fs::create_dir(&directory).unwrap();
            let path = directory.join("file");
            fs::write(&path, b"old").unwrap();
            let file = File::open(&path).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();
            File::open(&directory)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();
            let original = fingerprint(&file.metadata().unwrap());
            let mut store = LocalStore::open(root.path()).unwrap();
            let namespace = store
                .stream_namespace(&mut std::io::Cursor::new(Vec::new()))
                .unwrap()
                .unwrap();
            assert_eq!(namespace["old"].modified, 0);
            assert_eq!(namespace["old/file"].modified, 0);
            assert_eq!(
                store.scan().unwrap()["old/file"].hash,
                hash_reader(b"old".as_slice()).unwrap().0
            );
            store.validate_scan_snapshot().unwrap();
            assert_eq!(file.metadata().unwrap().modified().unwrap(), modified);
            assert_eq!(directory.metadata().unwrap().modified().unwrap(), modified);
            assert_eq!(fingerprint(&file.metadata().unwrap()), original);

            file.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH - Duration::from_secs(2)))
                .unwrap();
            assert_eq!(
                enumerate_namespace(root.path(), &store.ignore).unwrap()["old/file"].modified,
                0
            );
            assert!(store
                .validate_scan_snapshot()
                .unwrap_err()
                .to_string()
                .contains("STALE_SOURCE"));
        }
    }

    fn received(directory: &Path, bytes: &[u8]) -> VerifiedStaged {
        let mut file = NamedTempFile::new_in(directory).unwrap();
        let (hash, size) = copy_and_hash(bytes, &mut file).unwrap();
        let entry = Entry { hash, size };
        VerifiedStaged::from_digest(file, &entry, &entry.hash, size).unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn received_install_preserves_inode_and_skips_unchanged_final_hash() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(root.path()).unwrap();
        store
            .stream_namespace(&mut std::io::Cursor::new(Vec::new()))
            .unwrap();
        let staged = received(&store.private, b"received bytes");
        let entry = staged.entry().clone();
        let inode = staged.path().metadata().unwrap().ino();
        let path = staged.path().to_owned();
        store
            .install_received("nested/file", None, &entry, staged)
            .unwrap();
        assert_eq!(
            root.path().join("nested/file").metadata().unwrap().ino(),
            inode
        );
        assert!(!path.exists());
        store.validate_scan_snapshot().unwrap();
        assert_eq!(store.metrics().staging_copies, 0);
        assert_eq!(store.metrics().files_hashed, 0);
        assert_eq!(
            fs::read(root.path().join("nested/file")).unwrap(),
            b"received bytes"
        );
        // Unknown identity must fall back to reading and checking the payload.
        store.stream_installed_fingerprints.clear();
        store.validate_scan_snapshot().unwrap();
        assert_eq!(store.metrics().files_hashed, 1);
        assert_eq!(store.metrics().bytes_hashed, entry.size);
        fs::write(root.path().join("nested/file"), b"modified bytes").unwrap();
        assert!(store
            .validate_scan_snapshot()
            .unwrap_err()
            .to_string()
            .contains("STALE_TARGET"));
    }

    #[test]
    #[cfg(unix)]
    fn received_overwrite_preserves_displaced_inode_for_recovery() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(root.path()).unwrap();
        fs::write(root.path().join("file"), b"old").unwrap();
        let old_inode = root.path().join("file").metadata().unwrap().ino();
        let old = store.current("file").unwrap().unwrap();
        let staged = received(&store.private, b"new");
        let entry = staged.entry().clone();
        let new_inode = staged.path().metadata().unwrap().ino();
        store
            .install_received("file", Some(&old.hash), &entry, staged)
            .unwrap();
        assert_eq!(
            root.path().join("file").metadata().unwrap().ino(),
            new_inode
        );
        assert!(fs::read_dir(store.private.join("recovery"))
            .unwrap()
            .any(|item| {
                let path = item.unwrap().path();
                path.metadata().unwrap().ino() == old_inode && fs::read(path).unwrap() == b"old"
            }));
        store.recover().unwrap();
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"new");
        assert_eq!(store.metrics().staging_copies, 0);
    }

    #[test]
    fn foreign_received_stage_uses_verified_copy() {
        let root = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(root.path()).unwrap();
        let staged = received(foreign.path(), b"payload");
        let entry = staged.entry().clone();
        let path = staged.path().to_owned();
        store
            .install_received("file", None, &entry, staged)
            .unwrap();
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"payload");
        assert_eq!(store.metrics().staging_copies, 1);
        assert!(!path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn changed_verified_stage_never_replaces_target() {
        let root = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(root.path()).unwrap();
        fs::write(root.path().join("file"), b"old").unwrap();
        let old = store.current("file").unwrap().unwrap();
        let staged = received(&store.private, b"new");
        let entry = staged.entry().clone();
        fs::write(staged.path(), b"bad").unwrap();
        assert!(store
            .install_received("file", Some(&old.hash), &entry, staged)
            .is_err());
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"old");
    }

    #[test]
    #[cfg(unix)]
    fn edited_installed_file_is_rehashed_and_rejected() {
        let root = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(root.path()).unwrap();
        store
            .stream_namespace(&mut std::io::Cursor::new(Vec::new()))
            .unwrap();
        let staged = received(&store.private, b"good");
        let entry = staged.entry().clone();
        store
            .install_received("file", None, &entry, staged)
            .unwrap();
        let target = root.path().join("file");
        let modified = target.metadata().unwrap().modified().unwrap();
        fs::write(&target, b"evil").unwrap();
        File::open(&target)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(store
            .validate_scan_snapshot()
            .unwrap_err()
            .to_string()
            .contains("STALE_TARGET"));
        assert_eq!(store.metrics().files_hashed, 1);
    }

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
    fn recovery_snapshot_does_not_lock_or_create_a_store() {
        let root = tempfile::tempdir().unwrap();
        assert!(LocalStore::read_recovery_entries(root.path())
            .unwrap()
            .is_empty());
        assert!(!root.path().join(".rowd").exists());
        let mut store = LocalStore::open(root.path()).unwrap();
        let id = "a".repeat(64);
        fs::write(store.private.join("recovery").join(&id), b"backup").unwrap();
        atomic_json(
            &store.private.join("recovery").join(format!("{id}.json")),
            &Journal {
                blake3: true,
                path: "file".into(),
                backup: id,
                finished: false,
                new_hash: None,
                old_hash: None,
            },
        )
        .unwrap();
        let entries = LocalStore::read_recovery_entries(root.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].bytes, 6);
        assert!(
            LocalStore::open_recovery(root.path()).is_err(),
            "writer lock must remain held"
        );
        store.recover().unwrap();
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"backup");
    }

    #[test]
    fn sha256_cache_is_rehashed_and_pending_journals_still_recover() {
        let root = tempfile::tempdir().unwrap();
        let mut store = LocalStore::open(root.path()).unwrap();
        fs::write(root.path().join("file"), b"old").unwrap();
        let old = crate::legacy_hash_reader(b"old".as_slice()).unwrap().0;
        let id = "f".repeat(64);
        atomic_json(
            &store.private.join("recovery").join(format!("{id}.json")),
            &serde_json::json!({
                "path":"file", "backup":id, "finished":false, "old_hash":old, "new_hash":null,
            }),
        )
        .unwrap();
        store.recover().unwrap();
        store.scan().unwrap();
        store.persist_cache().unwrap();
        let cache_path = store.private.join("cache.json");
        let mut cache: serde_json::Value =
            serde_json::from_reader(File::open(&cache_path).unwrap()).unwrap();
        cache.as_object_mut().unwrap().remove("blake3");
        cache["files"]["file"]["entry"]["hash"] = old.into();
        atomic_json(&cache_path, &cache).unwrap();
        drop(store);
        let mut store = LocalStore::open(root.path()).unwrap();
        assert!(!store.cache_trusted);
        assert_eq!(
            store.scan().unwrap()["file"].hash,
            hash_reader(b"old".as_slice()).unwrap().0
        );
        assert!(store.metrics().files_hashed > 0);
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
            blake3: true,
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
                    blake3: true,
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
                blake3: true,
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
