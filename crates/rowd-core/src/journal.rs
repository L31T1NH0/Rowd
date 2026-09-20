use crate::{
    model::{Entry, Manifest},
    storage::{atomic_json, Snapshot, Store},
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub sequence: u64,
    pub operation: String,
    pub entry: Option<Entry>,
}
#[derive(Default, Serialize, Deserialize)]
pub struct ShareState {
    pub share_id: String,
    pub manifest: Manifest,
    pub acknowledged: Manifest,
    pub pending: BTreeMap<String, Change>,
    pub sequence: u64,
}
impl ShareState {
    pub fn load(path: &Path, share_id: &str) -> Result<Self> {
        let state: Self = if path.exists() {
            serde_json::from_reader(File::open(path)?)?
        } else {
            Self {
                share_id: share_id.into(),
                ..Self::default()
            }
        };
        ensure!(
            state.share_id == share_id,
            "journal belongs to another Share"
        );
        crate::model::validate_manifest(&state.manifest)?;
        Ok(state)
    }
    pub fn observe(&mut self, manifest: Manifest) {
        let paths: std::collections::BTreeSet<_> = self
            .manifest
            .keys()
            .chain(manifest.keys())
            .cloned()
            .collect();
        for path in paths {
            let before = self.manifest.get(&path);
            let after = manifest.get(&path);
            if before == after {
                continue;
            }
            self.sequence += 1;
            if after == self.acknowledged.get(&path) {
                self.pending.remove(&path);
            } else {
                self.pending.insert(
                    path,
                    Change {
                        sequence: self.sequence,
                        operation: if after.is_none() {
                            "remove"
                        } else if before.is_none() {
                            "create"
                        } else {
                            "modify"
                        }
                        .into(),
                        entry: after.cloned(),
                    },
                );
            }
        }
        self.manifest = manifest;
    }
    pub fn ack(&mut self, path: &str, entry: &Entry) {
        self.acknowledged.insert(path.into(), entry.clone());
        if self.pending.get(path).and_then(|c| c.entry.as_ref()) == Some(entry) {
            self.pending.remove(path);
        }
    }
}

pub struct TrackedStore<S> {
    pub inner: S,
    pub state: ShareState,
    path: PathBuf,
}
impl<S: Store> TrackedStore<S> {
    pub fn new(inner: S, path: PathBuf, id: &str) -> Result<Self> {
        Ok(Self {
            inner,
            state: ShareState::load(&path, id)?,
            path,
        })
    }
    fn save(&self) -> Result<()> {
        atomic_json(&self.path, &self.state)
    }
}
impl<S: Store> Store for TrackedStore<S> {
    fn excluded(&self, path: &str) -> bool {
        self.inner.excluded(path)
    }
    fn scan(&mut self) -> Result<Manifest> {
        let files = self.inner.scan()?;
        self.state.pending.retain(|p, _| !self.inner.excluded(p));
        self.state.manifest.retain(|p, _| !self.inner.excluded(p));
        self.state.observe(files.clone());
        self.save()?;
        Ok(files)
    }
    fn snapshot(&mut self, path: &str, expected: &Entry) -> Result<Snapshot> {
        self.inner.snapshot(path, expected)
    }
    fn install(
        &mut self,
        path: &str,
        expected: Option<&str>,
        entry: &Entry,
        staged: &Path,
    ) -> Result<()> {
        self.inner.install(path, expected, entry, staged)?;
        self.state.manifest.insert(path.into(), entry.clone());
        self.state.ack(path, entry);
        // An incoming, verified installation supersedes the old pending version.
        self.state.pending.remove(path);
        self.save()
    }
    fn acknowledge(&mut self, path: &str, entry: &Entry) -> Result<()> {
        if self.state.acknowledged.get(path) == Some(entry)
            && !self.state.pending.contains_key(path)
        {
            return Ok(());
        }
        self.state.ack(path, entry);
        self.save()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_compaction_and_versioned_ack_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("queue.json");
        let mut s = ShareState::load(&p, "share").unwrap();
        for value in ["A", "B", "C", "D"] {
            let (hash, size) = crate::hash_reader(value.as_bytes()).unwrap();
            s.observe(BTreeMap::from([("a".into(), Entry { hash, size })]));
        }
        atomic_json(&p, &s).unwrap();
        let mut s = ShareState::load(&p, "share").unwrap();
        assert_eq!(s.pending.len(), 1);
        assert_eq!(s.pending["a"].sequence, 4);
        let last = s.manifest["a"].clone();
        let (hash, size) = crate::hash_reader(&b"A"[..]).unwrap();
        s.ack("a", &Entry { hash, size });
        assert_eq!(s.pending.len(), 1);
        s.ack("a", &last);
        s.ack("a", &last);
        assert!(s.pending.is_empty());
        assert!(ShareState::load(&p, "different").is_err());
    }
}
