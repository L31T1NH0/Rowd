use crate::{
    config::{ShareConfig, ShareRequest},
    journal::TrackedStore,
    model::Invitation,
    protocol::{self, Message},
    storage::{atomic_json, LocalStore, Store},
    sync::{self, Report},
};
use anyhow::{ensure, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub trait ManagedStore: Store {
    fn configure(&mut self, shares: &[ShareConfig], removed: &[String]) -> Result<()>;
    fn select(&mut self, share_id: &str) -> Result<PathBuf>;
    fn known_shares(&mut self) -> Result<Vec<ShareConfig>>;
    fn pending_share_requests(&mut self) -> Result<Vec<ShareRequest>> {
        Ok(Vec::new())
    }
    fn acknowledge_share_requests(
        &mut self,
        _accepted: &[String],
        _rejected: &[String],
        _cancelled: &[String],
    ) -> Result<()> {
        Ok(())
    }
    fn unlink_requested(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn confirm_unlinked(&mut self) -> Result<()> {
        Ok(())
    }
    fn available_shares(&mut self) -> Result<Vec<String>> {
        Ok(self
            .known_shares()?
            .into_iter()
            .map(|share| share.share_id)
            .collect())
    }
}
impl<S: Store> Store for &mut S {
    fn excluded(&self, p: &str) -> bool {
        (**self).excluded(p)
    }
    fn scan(&mut self) -> Result<crate::model::Manifest> {
        (**self).scan()
    }
    fn snapshot(&mut self, p: &str, e: &crate::model::Entry) -> Result<crate::storage::Snapshot> {
        (**self).snapshot(p, e)
    }
    fn install(
        &mut self,
        p: &str,
        expected: Option<&str>,
        e: &crate::model::Entry,
        stage: &Path,
    ) -> Result<()> {
        (**self).install(p, expected, e, stage)
    }
    fn acknowledge(&mut self, p: &str, e: &crate::model::Entry) -> Result<()> {
        (**self).acknowledge(p, e)
    }
}
pub fn validate_shares(shares: &[ShareConfig]) -> Result<()> {
    ensure!(shares.len() <= 256, "too many Shares");
    let mut ids = std::collections::BTreeSet::new();
    for (n, s) in shares.iter().enumerate() {
        crate::model::validate_hash(&s.share_id)?;
        ensure!(ids.insert(&s.share_id), "duplicate Share identity");
        if !s.android_path.is_empty() {
            crate::model::validate_path(&s.android_path)?;
        }
        for other in &shares[..n] {
            let a = PathBuf::from(s.android_path.to_lowercase());
            let b = PathBuf::from(other.android_path.to_lowercase());
            ensure!(
                (s.android_path.is_empty() != other.android_path.is_empty())
                    || (!a.starts_with(&b) && !b.starts_with(&a)),
                "overlapping Android destinations"
            );
        }
    }
    Ok(())
}
pub fn observe_offline(store: &mut impl ManagedStore) -> Result<()> {
    let available: std::collections::BTreeSet<_> = store.available_shares()?.into_iter().collect();
    for share in store
        .known_shares()?
        .into_iter()
        .filter(|share| available.contains(&share.share_id))
    {
        let path = store.select(&share.share_id)?;
        TrackedStore::new(&mut *store, path, &share.share_id)?.scan()?;
    }
    Ok(())
}
pub fn client_round(
    invite: &Invitation,
    root: &str,
    store: &mut impl ManagedStore,
) -> Result<Report> {
    // Persist local changes even when TCP cannot connect.
    observe_offline(store)?;
    let mut io = crate::tls::connect(invite)?;
    protocol::client_auth(
        &mut io,
        &invite.pair_id,
        &invite.folder_id,
        &invite.secret,
        root,
    )?;
    let Message::Shares { shares, removed } = protocol::receive(&mut io)? else {
        anyhow::bail!("expected Share configuration")
    };
    validate_shares(&shares)?;
    store.configure(&shares, &removed)?;
    let available_shares = store.available_shares()?;
    for id in &available_shares {
        crate::model::validate_hash(id)?;
        ensure!(
            shares.iter().any(|share| &share.share_id == id),
            "available unknown Share"
        );
    }
    protocol::send(
        &mut io,
        &Message::Capabilities {
            device_id: root.into(),
            polling: true,
            managed_shares: true,
            share_requests: store.pending_share_requests()?,
            available_shares,
            unlink_requested: store.unlink_requested()?,
        },
    )?;
    let (accepted, rejected, cancelled) = match protocol::receive(&mut io)? {
        Message::ShareRequestStatus {
            accepted,
            rejected,
            cancelled,
            ..
        } => (accepted, rejected, cancelled),
        Message::DeviceUnlinked => {
            store.confirm_unlinked()?;
            return Ok(Report::default());
        }
        _ => anyhow::bail!("expected Share request status"),
    };
    store.acknowledge_share_requests(&accepted, &rejected, &cancelled)?;
    let mut report = Report::default();
    loop {
        match protocol::receive(&mut io)? {
            Message::SelectShare { share_id } => {
                ensure!(
                    shares.iter().any(|s| s.share_id == share_id),
                    "unknown Share"
                );
                let path = store.select(&share_id)?;
                let mut tracked = TrackedStore::new(&mut *store, path, &share_id)?;
                protocol::send(&mut io, &Message::Ready)?;
                let r = sync::respond_share(&mut io, &mut tracked, Some(&share_id))?;
                report.transferred += r.transferred;
                report.conflicts += r.conflicts;
            }
            Message::SessionDone => return Ok(report),
            _ => anyhow::bail!("unexpected session message"),
        }
    }
}

pub struct LocalDevice {
    root: PathBuf,
    shares: Vec<ShareConfig>,
    active: Option<LocalStore>,
}
impl LocalDevice {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root.join(".rowd"))?;
        let root = root.canonicalize()?;
        let path = root.join(".rowd/shares.json");
        let shares = if path.exists() {
            serde_json::from_reader(fs::File::open(path)?)?
        } else {
            vec![]
        };
        Ok(Self {
            root,
            shares,
            active: None,
        })
    }
}
impl ManagedStore for LocalDevice {
    fn known_shares(&mut self) -> Result<Vec<ShareConfig>> {
        Ok(self.shares.clone())
    }
    fn pending_share_requests(&mut self) -> Result<Vec<ShareRequest>> {
        let path = self.root.join(".rowd/share-requests.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_reader(fs::File::open(path)?)?)
    }
    fn acknowledge_share_requests(
        &mut self,
        accepted: &[String],
        rejected: &[String],
        cancelled: &[String],
    ) -> Result<()> {
        if accepted.is_empty() && rejected.is_empty() && cancelled.is_empty() {
            return Ok(());
        }
        let path = self.root.join(".rowd/share-requests.json");
        let mut pending = self.pending_share_requests()?;
        pending.retain(|request| {
            !accepted.contains(&request.request_id)
                && !rejected.contains(&request.request_id)
                && !cancelled.contains(&request.request_id)
        });
        atomic_json(&path, &pending)
    }
    fn configure(&mut self, shares: &[ShareConfig], _removed: &[String]) -> Result<()> {
        validate_shares(shares)?;
        if shares.iter().any(|s| s.android_path.is_empty()) {
            for s in shares.iter().filter(|s| !s.android_path.is_empty()) {
                if !self.shares.iter().any(|old| old.share_id == s.share_id) {
                    ensure!(
                        !self.root.join(&s.android_path).exists(),
                        "Android destination overlaps existing legacy content: {}",
                        s.android_path
                    );
                }
            }
        }
        // Removing a definition never removes its data or recovery records.
        atomic_json(&self.root.join(".rowd/shares.json"), &shares)?;
        self.shares = shares.to_vec();
        Ok(())
    }
    fn select(&mut self, id: &str) -> Result<PathBuf> {
        self.active = None;
        let share = self
            .shares
            .iter()
            .find(|s| s.share_id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown Share"))?;
        let mut path = self.root.clone();
        for part in share.android_path.split('/').filter(|s| !s.is_empty()) {
            path.push(part);
            if path.exists() {
                ensure!(
                    !fs::symlink_metadata(&path)?.file_type().is_symlink(),
                    "symlink destination"
                );
            }
        }
        let mut store = LocalStore::open(&path)?;
        store.remote_ignore = share.ignore.clone();
        if share.android_path.is_empty() {
            for other in self.shares.iter().filter(|s| !s.android_path.is_empty()) {
                store
                    .remote_ignore
                    .push_str(&format!("\n{}/", other.android_path));
            }
        }
        let state = store.private().join(format!("journal-{id}.json"));
        self.active = Some(store);
        Ok(state)
    }
}
impl Store for LocalDevice {
    fn excluded(&self, p: &str) -> bool {
        self.active.as_ref().is_some_and(|s| s.excluded(p))
    }
    fn scan(&mut self) -> Result<crate::model::Manifest> {
        self.active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no active Share"))?
            .scan()
    }
    fn snapshot(&mut self, p: &str, e: &crate::model::Entry) -> Result<crate::storage::Snapshot> {
        self.active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no active Share"))?
            .snapshot(p, e)
    }
    fn install(
        &mut self,
        p: &str,
        x: Option<&str>,
        e: &crate::model::Entry,
        s: &Path,
    ) -> Result<()> {
        self.active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no active Share"))?
            .install(p, x, e, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::SyncMode, model::Manifest, storage::Snapshot};

    fn share(path: &str) -> ShareConfig {
        ShareConfig {
            share_id: "a".repeat(64),
            name: "Documentos".into(),
            root: PathBuf::new(),
            android_path: path.into(),
            mode: SyncMode::Bidirectional,
            enabled: true,
            ignore: String::new(),
            request_id: None,
            remap_policy: None,
        }
    }

    struct Unassigned {
        share: ShareConfig,
    }

    impl Store for Unassigned {
        fn excluded(&self, _: &str) -> bool {
            false
        }
        fn scan(&mut self) -> Result<Manifest> {
            panic!("an unassigned Share must not be scanned")
        }
        fn snapshot(&mut self, _: &str, _: &crate::model::Entry) -> Result<Snapshot> {
            unreachable!()
        }
        fn install(
            &mut self,
            _: &str,
            _: Option<&str>,
            _: &crate::model::Entry,
            _: &Path,
        ) -> Result<()> {
            unreachable!()
        }
    }

    impl ManagedStore for Unassigned {
        fn configure(&mut self, _: &[ShareConfig], _: &[String]) -> Result<()> {
            Ok(())
        }
        fn select(&mut self, _: &str) -> Result<PathBuf> {
            panic!("an unassigned Share must not be selected")
        }
        fn known_shares(&mut self) -> Result<Vec<ShareConfig>> {
            Ok(vec![self.share.clone()])
        }
        fn available_shares(&mut self) -> Result<Vec<String>> {
            Ok(vec![])
        }
    }

    #[test]
    fn offline_observation_skips_shares_without_an_android_folder() {
        let mut store = Unassigned {
            share: share("legacy-hint"),
        };
        observe_offline(&mut store).unwrap();
    }

    #[test]
    fn local_device_accepts_remap_without_removing_old_content() {
        let directory = tempfile::tempdir().unwrap();
        let mut device = LocalDevice::open(directory.path()).unwrap();
        device.configure(&[share("old")], &[]).unwrap();
        fs::create_dir_all(directory.path().join("old")).unwrap();
        fs::write(directory.path().join("old/keep.txt"), b"keep").unwrap();

        device.configure(&[share("new")], &[]).unwrap();

        assert_eq!(device.known_shares().unwrap()[0].android_path, "new");
        assert_eq!(
            fs::read(directory.path().join("old/keep.txt")).unwrap(),
            b"keep"
        );
    }
}
