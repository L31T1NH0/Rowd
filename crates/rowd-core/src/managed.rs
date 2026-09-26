#[cfg(any(test, feature = "dev-tools"))]
use crate::storage::{atomic_json, LocalStore};
use crate::{
    config::{ShareDefinition, ShareRequest},
    model::Invitation,
    protocol::{self, Message},
    storage::Store,
    sync::{self, Report},
};
use anyhow::{ensure, Result};
#[cfg(any(test, feature = "dev-tools"))]
use std::fs;
#[cfg(any(test, feature = "dev-tools"))]
use std::path::Path;
#[cfg(any(test, feature = "dev-tools"))]
use std::path::PathBuf;
use std::time::Instant;

pub struct ClientState {
    pub focus_shares: Option<Vec<String>>,
    pub available_shares: Vec<String>,
    pub share_requests: Vec<ShareRequest>,
    pub cancel_intents: Vec<String>,
    pub unlink_requested: bool,
}

pub trait ManagedClient: Store {
    fn configure(&mut self, shares: &[ShareDefinition]) -> Result<()>;
    fn session_state(&mut self) -> Result<ClientState>;
    fn select(&mut self, share_id: &str) -> Result<()>;
    fn acknowledge_share_requests(
        &mut self,
        accepted: &[String],
        rejected: &[String],
        cancelled: &[String],
    ) -> Result<()>;
    fn prepare_unlink(&mut self) -> Result<()>;
    fn finish_unlink(&mut self) -> Result<()>;
}
impl<S: Store> Store for &mut S {
    fn delta_paths(&mut self) -> Result<Option<std::collections::BTreeSet<String>>> {
        (**self).delta_paths()
    }
    fn scan_paths(
        &mut self,
        paths: &std::collections::BTreeSet<String>,
    ) -> Result<Option<crate::model::Manifest>> {
        (**self).scan_paths(paths)
    }
    fn base_token(&self) -> Option<String> {
        (**self).base_token()
    }
    fn set_base_token(&mut self, token: Option<String>) {
        (**self).set_base_token(token)
    }
    fn require_full_scan(&mut self) -> Result<()> {
        (**self).require_full_scan()
    }
    fn metrics(&self) -> crate::storage::StoreMetrics {
        (**self).metrics()
    }
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
        stage: &crate::storage::VerifiedStaged,
    ) -> Result<()> {
        (**self).install(p, expected, e, stage)
    }
    fn acknowledge(&mut self, p: &str, e: &crate::model::Entry) -> Result<()> {
        (**self).acknowledge(p, e)
    }
}
pub fn validate_shares(shares: &[ShareDefinition]) -> Result<()> {
    ensure!(shares.len() <= 256, "too many Shares");
    let mut ids = std::collections::BTreeSet::new();
    for s in shares {
        crate::model::validate_hash(&s.share_id)?;
        ensure!(ids.insert(&s.share_id), "duplicate Share identity");
        crate::ignore::Ignore::validate(&s.ignore_rules)?;
    }
    Ok(())
}
fn share_queue(
    shares: &[ShareDefinition],
    available: &[String],
    focus: Option<&std::collections::BTreeSet<String>>,
) -> std::collections::VecDeque<String> {
    shares
        .iter()
        .filter(|share| {
            share.enabled
                && available.contains(&share.share_id)
                && focus.is_none_or(|ids| ids.contains(&share.share_id))
        })
        .map(|share| share.share_id.clone())
        .collect()
}
pub fn client_round(
    invite: &Invitation,
    root: &str,
    store: &mut impl ManagedClient,
) -> Result<Report> {
    let mut skipped = std::collections::BTreeSet::new();
    let mut errors = Vec::new();
    loop {
        let mut io = crate::tls::connect(invite)?;
        protocol::client_auth(&mut io, &invite.pair_id, &invite.secret, root)?;
        let mut failed = None;
        match client_round_on_excluding(&mut io, root, store, &skipped, &mut failed) {
            Ok(report) => {
                ensure!(
                    errors.is_empty(),
                    "{} Share(s) failed: {}",
                    errors.len(),
                    errors.join("; ")
                );
                return Ok(report);
            }
            Err(error) => match failed {
                Some(id) if skipped.insert(id.clone()) => errors.push(format!("{id}: {error:#}")),
                _ => return Err(error),
            },
        }
    }
}

pub fn client_round_on(
    io: &mut (impl std::io::Read + std::io::Write + Send),
    root: &str,
    store: &mut impl ManagedClient,
) -> Result<Report> {
    client_round_on_excluding(io, root, store, &Default::default(), &mut None)
}

pub fn client_round_on_excluding(
    io: &mut (impl std::io::Read + std::io::Write + Send),
    root: &str,
    store: &mut impl ManagedClient,
    skipped: &std::collections::BTreeSet<String>,
    failed: &mut Option<String>,
) -> Result<Report> {
    protocol::send(io, &Message::StartRound)?;
    let mut queue = None::<std::collections::VecDeque<String>>;
    let mut definitions = None::<Vec<ShareDefinition>>;
    let mut available_snapshot = None::<Vec<String>>;
    let mut aggregate = Report::default();
    let mut pending_wakes = std::collections::BTreeSet::new();
    let mut errors = Vec::new();
    loop {
        let round = (|| -> Result<Option<Report>> {
            let shares = loop {
                match protocol::receive(io)? {
                    Message::WakeShare { share_id } => {
                        pending_wakes.insert(share_id);
                    }
                    Message::Shares { shares } => break shares,
                    _ => anyhow::bail!("expected Share configuration"),
                }
            };
            validate_shares(&shares)?;
            if let Some(previous) = &definitions {
                ensure!(
                    previous.len() == shares.len()
                        && previous.iter().zip(&shares).all(|(a, b)| {
                            a.share_id == b.share_id
                                && a.name == b.name
                                && a.mode == b.mode
                                && a.enabled == b.enabled
                                && a.binding_revision == b.binding_revision
                                && a.ignore_rules == b.ignore_rules
                                && a.request_id == b.request_id
                        }),
                    "Share definitions changed during a cycle"
                );
            } else {
                let configuring = Instant::now();
                store.configure(&shares)?;
                aggregate.metrics.configure_ms += configuring.elapsed().as_millis();
            }
            let client_state = store.session_state()?;
            if definitions.is_none() {
                let available = client_state.available_shares.clone();
                let focus = client_state
                    .focus_shares
                    .clone()
                    .map(|ids| ids.into_iter().collect::<std::collections::BTreeSet<_>>());
                if let Some(focus) = &focus {
                    for id in focus {
                        crate::model::validate_hash(id)?;
                    }
                }
                for id in &available {
                    crate::model::validate_hash(id)?;
                    ensure!(
                        shares.iter().any(|share| &share.share_id == id),
                        "available unknown Share"
                    );
                }
                let mut selected = share_queue(&shares, &available, focus.as_ref());
                selected.retain(|id| !skipped.contains(id));
                queue = Some(selected);
                available_snapshot = Some(available);
                definitions = Some(shares.clone());
            }
            let wanted = queue
                .as_ref()
                .expect("queue initialized")
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            // SAF availability is a round-level hint; select() rechecks the binding before I/O.
            let available_shares = available_snapshot
                .clone()
                .expect("availability initialized");
            protocol::send(
                io,
                &Message::Capabilities {
                    device_id: root.into(),
                    share_requests: client_state.share_requests,
                    cancel_intents: client_state.cancel_intents,
                    available_shares,
                    requested_share_ids: wanted.clone(),
                    audit: client_state.focus_shares.is_none(),
                    unlink_requested: client_state.unlink_requested,
                },
            )?;
            let (accepted, rejected, cancelled) = match protocol::receive(io)? {
                Message::ShareRequestStatus {
                    accepted,
                    rejected,
                    cancelled,
                } => (accepted, rejected, cancelled),
                Message::DeviceUnlinked => {
                    store.prepare_unlink()?;
                    protocol::send(io, &Message::UnlinkAck)?;
                    ensure!(
                        matches!(protocol::receive(io)?, Message::UnlinkComplete),
                        "unlink confirmation missing"
                    );
                    store.finish_unlink()?;
                    return Ok(None);
                }
                _ => anyhow::bail!("expected Share request status"),
            };
            store.acknowledge_share_requests(&accepted, &rejected, &cancelled)?;
            let mut report = Report::default();
            loop {
                match protocol::receive(io)? {
                    Message::ShareSkipped { share_id, reason } => {
                        ensure!(
                            queue.as_ref().and_then(|items| items.front()) == Some(&share_id),
                            "unexpected skipped Share"
                        );
                        queue.as_mut().expect("queue initialized").pop_front();
                        errors.push(format!("{share_id}: {reason}"));
                    }
                    Message::SelectShare { share_id } => {
                        ensure!(
                            queue.as_ref().and_then(|items| items.front()) == Some(&share_id),
                            "unexpected Share selection"
                        );
                        *failed = Some(share_id.clone());
                        store.select(&share_id)?;
                        protocol::send(io, &Message::Ready)?;
                        let result = sync::respond_share(io, &mut *store, Some(&share_id))?;
                        report.transferred += result.transferred;
                        report.conflicts += result.conflicts;
                        report.shares_processed += result.shares_processed;
                        queue.as_mut().expect("queue initialized").pop_front();
                        *failed = None;
                    }
                    Message::SessionDone => {
                        queue.as_mut().expect("queue initialized").clear();
                        return Ok(Some(report));
                    }
                    Message::RoundDeferred { shares } => {
                        queue.as_mut().expect("queue initialized").clear();
                        for id in shares {
                            crate::model::validate_hash(&id)?;
                            pending_wakes.insert(id);
                        }
                        report.round_deferred = true;
                        return Ok(Some(report));
                    }
                    _ => anyhow::bail!("unexpected session message"),
                }
            }
        })();
        match round {
            Ok(None) => return Ok(aggregate),
            Ok(Some(report)) => {
                aggregate.transferred += report.transferred;
                aggregate.conflicts += report.conflicts;
                aggregate.shares_processed += report.shares_processed;
                aggregate.round_deferred |= report.round_deferred;
            }
            Err(error) => return Err(error),
        }
        if queue
            .as_ref()
            .is_none_or(std::collections::VecDeque::is_empty)
        {
            break;
        }
    }
    aggregate.pending_wakes = pending_wakes.into_iter().collect();
    ensure!(
        errors.is_empty(),
        "{} Share(s) failed: {}",
        errors.len(),
        errors.join("; ")
    );
    Ok(aggregate)
}

#[cfg(any(test, feature = "dev-tools"))]
pub struct LocalDevice {
    root: PathBuf,
    shares: Vec<ShareDefinition>,
    bindings: std::collections::BTreeMap<String, String>,
    focus: Option<Vec<String>>,
    active: Option<LocalStore>,
}
#[cfg(any(test, feature = "dev-tools"))]
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
        let bindings_path = root.join(".rowd/dev-bindings.json");
        let bindings = if bindings_path.exists() {
            serde_json::from_reader(fs::File::open(bindings_path)?)?
        } else {
            Default::default()
        };
        Ok(Self {
            root,
            shares,
            bindings,
            focus: None,
            active: None,
        })
    }

    /// Explicit filesystem binding used by tests and the dev simulator only.
    pub fn bind(&mut self, share_id: &str, path: &str) -> Result<()> {
        crate::model::validate_hash(share_id)?;
        if !path.is_empty() {
            crate::model::validate_path(path)?;
        }
        self.bindings.insert(share_id.into(), path.into());
        atomic_json(&self.root.join(".rowd/dev-bindings.json"), &self.bindings)
    }

    pub fn focus(&mut self, ids: Vec<String>) {
        self.focus = Some(ids);
    }

    fn pending_requests(&self) -> Result<Vec<ShareRequest>> {
        let path = self.root.join(".rowd/share-requests.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_reader(fs::File::open(path)?)?)
    }
}
#[cfg(any(test, feature = "dev-tools"))]
impl ManagedClient for LocalDevice {
    fn session_state(&mut self) -> Result<ClientState> {
        let available_shares = self
            .shares
            .iter()
            .filter(|share| self.bindings.contains_key(&share.share_id))
            .map(|share| share.share_id.clone())
            .collect();
        Ok(ClientState {
            focus_shares: self.focus.clone(),
            available_shares,
            share_requests: self.pending_requests()?,
            cancel_intents: Vec::new(),
            unlink_requested: false,
        })
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
        let mut pending = self.pending_requests()?;
        pending.retain(|request| {
            !accepted.contains(&request.request_id)
                && !rejected.contains(&request.request_id)
                && !cancelled.contains(&request.request_id)
        });
        atomic_json(&path, &pending)
    }
    fn configure(&mut self, shares: &[ShareDefinition]) -> Result<()> {
        validate_shares(shares)?;
        // Removing a definition never removes its data or recovery records.
        atomic_json(&self.root.join(".rowd/shares.json"), &shares)?;
        self.shares = shares.to_vec();
        Ok(())
    }
    fn select(&mut self, id: &str) -> Result<()> {
        self.active = None;
        let share = self
            .shares
            .iter()
            .find(|s| s.share_id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown Share"))?;
        let mut path = self.root.clone();
        let binding = self
            .bindings
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("unbound Share"))?;
        for part in binding.split('/').filter(|s| !s.is_empty()) {
            path.push(part);
            if path.exists() {
                ensure!(
                    !fs::symlink_metadata(&path)?.file_type().is_symlink(),
                    "symlink destination"
                );
            }
        }
        let mut policy = share.ignore_rules.clone();
        if binding.is_empty() {
            for other in self.bindings.values().filter(|path| !path.is_empty()) {
                policy.push_str(&format!("\n{other}/"));
            }
        }
        self.active = Some(LocalStore::open_with_policy(&path, &policy)?);
        Ok(())
    }
    fn prepare_unlink(&mut self) -> Result<()> {
        Ok(())
    }
    fn finish_unlink(&mut self) -> Result<()> {
        Ok(())
    }
}
#[cfg(any(test, feature = "dev-tools"))]
impl Store for LocalDevice {
    fn metrics(&self) -> crate::storage::StoreMetrics {
        self.active.as_ref().map(Store::metrics).unwrap_or_default()
    }
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
        s: &crate::storage::VerifiedStaged,
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
    use crate::config::SyncMode;

    fn share(path: &str) -> ShareDefinition {
        ShareDefinition {
            share_id: "a".repeat(64),
            name: path.into(),
            mode: SyncMode::Bidirectional,
            enabled: true,
            binding_revision: 0,
            ignore_rules: String::new(),
            request_id: None,
            remap_policy: None,
        }
    }

    #[test]
    fn local_device_accepts_remap_without_removing_old_content() {
        let directory = tempfile::tempdir().unwrap();
        let mut device = LocalDevice::open(directory.path()).unwrap();
        device.bind(&"a".repeat(64), "old").unwrap();
        device.configure(&[share("old")]).unwrap();
        fs::create_dir_all(directory.path().join("old")).unwrap();
        fs::write(directory.path().join("old/keep.txt"), b"keep").unwrap();

        device.bind(&"a".repeat(64), "new").unwrap();
        device.configure(&[share("new")]).unwrap();

        assert_eq!(device.shares[0].name, "new");
        assert_eq!(
            fs::read(directory.path().join("old/keep.txt")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn focused_round_only_selects_dirty_enabled_shares() {
        let first = share("first");
        let mut second = share("second");
        second.share_id = "b".repeat(64);
        let available = vec![first.share_id.clone(), second.share_id.clone()];
        let focus = [second.share_id.clone()].into_iter().collect();
        assert_eq!(
            share_queue(&[first.clone(), second.clone()], &available, Some(&focus)),
            std::collections::VecDeque::from([second.share_id.clone()])
        );
        assert_eq!(
            share_queue(&[first.clone(), second.clone()], &available, None).len(),
            2
        );
        second.enabled = false;
        assert!(share_queue(&[first, second], &available, Some(&focus)).is_empty());

        let shares: Vec<_> = (0..50)
            .map(|index| {
                let mut item = share(&format!("share-{index}"));
                item.share_id = format!("{index:064x}");
                item
            })
            .collect();
        let available: Vec<_> = shares.iter().map(|share| share.share_id.clone()).collect();
        let focus = [shares[37].share_id.clone()].into_iter().collect();
        assert_eq!(share_queue(&shares, &available, Some(&focus)).len(), 1);
    }
}
