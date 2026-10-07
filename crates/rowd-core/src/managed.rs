#[cfg(any(test, feature = "dev-tools"))]
use crate::storage::{atomic_json, LocalStore};
use crate::{
    config::{ShareDefinition, ShareRequest},
    model::Invitation,
    protocol::{self, Message},
    storage::Store,
    sync::{self, Report},
};
use anyhow::{ensure, Context, Result};
#[cfg(any(test, feature = "dev-tools"))]
use std::fs;
#[cfg(any(test, feature = "dev-tools"))]
use std::path::Path;
#[cfg(any(test, feature = "dev-tools"))]
use std::path::PathBuf;
use std::time::Instant;

pub struct ClientState {
    pub focus_shares: Option<Vec<String>>,
    pub audit: bool,
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
    fn finish_unlink(&mut self) -> Result<()>;
    /// Report a local deferred failure only after the session has been drained safely.
    fn finish_round(&mut self, _report: &Report) -> Result<()> {
        Ok(())
    }
}
impl<S: Store> Store for &mut S {
    fn staging_directory(&self) -> Option<std::path::PathBuf> {
        (**self).staging_directory()
    }
    fn install_received(
        &mut self,
        p: &str,
        expected: Option<&str>,
        e: &crate::model::Entry,
        stage: crate::storage::VerifiedStaged,
    ) -> Result<()> {
        (**self).install_received(p, expected, e, stage)
    }
    fn validate_scan_snapshot(&mut self) -> Result<()> {
        (**self).validate_scan_snapshot()
    }
    fn scan_binding(&mut self) -> Result<String> {
        (**self).scan_binding()
    }
    fn stream_namespace(
        &mut self,
        io: &mut (impl std::io::Read + std::io::Write),
    ) -> Result<Option<crate::model::Namespace>> {
        (**self).stream_namespace(io)
    }
    fn start_hash_stream(&mut self, paths: &std::collections::BTreeSet<String>) -> Result<()> {
        (**self).start_hash_stream(paths)
    }
    fn next_hash_chunk(
        &mut self,
        io: &mut (impl std::io::Read + std::io::Write),
    ) -> Result<Option<crate::model::Manifest>> {
        (**self).next_hash_chunk(io)
    }
    fn release_hash_staging(&mut self, paths: &std::collections::BTreeSet<String>) -> Result<()> {
        (**self).release_hash_staging(paths)
    }
    fn scan_stream_metrics(&mut self) -> Result<crate::storage::ScanStreamMetrics> {
        (**self).scan_stream_metrics()
    }
    fn scan_is_staged(&self) -> bool {
        (**self).scan_is_staged()
    }
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
    fn delta_scan_with_control(
        &mut self,
        io: &mut (impl std::io::Read + std::io::Write),
        paths: &std::collections::BTreeSet<String>,
    ) -> Result<Option<(std::collections::BTreeSet<String>, crate::model::Manifest)>> {
        (**self).delta_scan_with_control(io, paths)
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
    fn scan_with_control(
        &mut self,
        io: &mut (impl std::io::Read + std::io::Write),
    ) -> Result<crate::model::Manifest> {
        (**self).scan_with_control(io)
    }
    fn commit_scan(&mut self) -> Result<()> {
        (**self).commit_scan()
    }
    fn discard_scan(&mut self) -> Result<()> {
        (**self).discard_scan()
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
// The negotiated queue is a set of remaining eligible Shares. The coordinator
// chooses their order (including a manual subset); each may be consumed once.
fn take_share(queue: &mut std::collections::VecDeque<String>, id: &str) -> Result<()> {
    let index = queue
        .iter()
        .position(|candidate| candidate == id)
        .context("unexpected Share selection: not negotiated or already consumed")?;
    queue.remove(index);
    Ok(())
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

/// Only a completed session is safe to reuse after an error.
#[derive(Debug)]
pub struct RoundFailure {
    pub stream_reusable: bool,
}
impl std::fmt::Display for RoundFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "round failed (stream_reusable={})", self.stream_reusable)
    }
}
impl std::error::Error for RoundFailure {}

pub fn client_round_on_excluding(
    io: &mut (impl std::io::Read + std::io::Write + Send),
    root: &str,
    store: &mut impl ManagedClient,
    skipped: &std::collections::BTreeSet<String>,
    failed: &mut Option<String>,
) -> Result<Report> {
    let _round = crate::trace::current_context()
        .with("round_id", crate::trace::new_id("round"))
        .enter();
    crate::trace_event!(
        crate::trace::Level::Info,
        crate::trace::Component::Round,
        "ROUND_CREATED",
        serde_json::json!({})
    );
    crate::trace_event!(
        crate::trace::Level::Trace,
        crate::trace::Component::Round,
        "ROUND_START",
        serde_json::json!({})
    );
    let mut session_complete = false;
    let result = client_round_session(io, root, store, skipped, failed, &mut session_complete)
        .and_then(|report| {
            store.finish_round(&report)?;
            Ok(report)
        });
    let _failed_share = failed.as_ref().map(|id| {
        crate::trace::current_context()
            .with("share_id", id.as_str())
            .enter()
    });
    crate::trace_event!(
        crate::trace::Level::Info,
        crate::trace::Component::Round,
        "ROUND_END",
        serde_json::json!({"result":if result.is_ok(){"success"}else{"failed"},
            "error":result.as_ref().err().map(|e| crate::trace::TraceError::new("round","client_round",e)),
            "transferred":result.as_ref().ok().map(|r|r.transferred),
            "conflicts":result.as_ref().ok().map(|r|r.conflicts),
            "round_deferred":result.as_ref().ok().map(|r|r.round_deferred),
            "failed_share":failed})
    );
    result.map_err(|error| {
        error.context(RoundFailure {
            stream_reusable: session_complete,
        })
    })
}

fn client_round_session(
    io: &mut (impl std::io::Read + std::io::Write + Send),
    root: &str,
    store: &mut impl ManagedClient,
    skipped: &std::collections::BTreeSet<String>,
    failed: &mut Option<String>,
    session_complete: &mut bool,
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
                for (index, share) in shares.iter().enumerate() {
                    let _share = crate::trace::current_context()
                        .with("share_id", share.share_id.clone())
                        .with("share_name", share.name.clone())
                        .with("share_index", index + 1)
                        .with("share_total", shares.len())
                        .enter();
                    let reason = if !share.enabled {
                        "disabled"
                    } else if !available.contains(&share.share_id) {
                        "binding_unavailable"
                    } else if focus.as_ref().is_some_and(|f| !f.contains(&share.share_id)) {
                        "outside_focus"
                    } else if skipped.contains(&share.share_id) {
                        "previous_failure"
                    } else {
                        "enabled_available_requested"
                    };
                    crate::trace_event!(
                        crate::trace::Level::Trace,
                        crate::trace::Component::Scheduler,
                        "SHARE_CONSIDERED",
                        serde_json::json!({"reason":reason})
                    );
                    crate::trace_event!(
                        crate::trace::Level::Trace,
                        crate::trace::Component::Scheduler,
                        if reason == "enabled_available_requested" {
                            "SHARE_SELECTED"
                        } else {
                            "SHARE_SKIPPED"
                        },
                        serde_json::json!({"reason":reason})
                    );
                }
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
                    audit: client_state.audit,
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
                    store.finish_unlink()?;
                    return Ok(None);
                }
                _ => anyhow::bail!("expected Share request status"),
            };
            store.acknowledge_share_requests(&accepted, &rejected, &cancelled)?;
            let mut report = Report::default();
            let mut deferred_share = None;
            loop {
                let message = protocol::receive(io)?;
                if let Message::ShareSkipped { share_id, .. } | Message::SelectShare { share_id } =
                    &message
                {
                    if let Err(error) =
                        take_share(queue.as_mut().expect("queue initialized"), share_id)
                    {
                        protocol::send_share_error(
                            io,
                            &error,
                            if cfg!(target_os = "android") {
                                "android"
                            } else {
                                "responder"
                            },
                            Some(share_id),
                            "select_share",
                            None,
                            "protocol",
                        );
                        return Err(error);
                    }
                }
                match message {
                    Message::ShareSkipped { share_id, reason } => {
                        errors.push(format!("{share_id}: {reason}"));
                    }
                    Message::SelectShare { share_id } => {
                        let definition = shares.iter().position(|s| s.share_id == share_id);
                        let mut context = crate::trace::current_context()
                            .with("share_id", share_id.clone())
                            .with("share_total", shares.len());
                        if let Some(index) = definition {
                            context = context
                                .with("share_name", shares[index].name.clone())
                                .with("share_index", index + 1);
                        }
                        let _share = context.enter();
                        *failed = Some(share_id.clone());
                        if let Err(error) = store.select(&share_id) {
                            // A binding can disappear after Capabilities. Report the
                            // logical rejection before closing, rather than making
                            // the coordinator mistake it for a transient TCP EOF.
                            protocol::send_share_error(
                                io,
                                &error,
                                if cfg!(target_os = "android") {
                                    "android"
                                } else {
                                    "responder"
                                },
                                Some(&share_id),
                                "select_share",
                                None,
                                "filesystem",
                            );
                            return Err(error);
                        }
                        protocol::send(io, &Message::Ready)?;
                        let result = sync::respond_share(io, &mut *store, Some(&share_id))?;
                        if result.round_deferred {
                            deferred_share = Some(share_id.clone());
                        }
                        report.transferred += result.transferred;
                        report.conflicts += result.conflicts;
                        report.shares_processed += result.shares_processed;
                        *failed = None;
                    }
                    Message::Scoped { share_id, message }
                        if deferred_share.as_deref() == Some(share_id.as_str())
                            && matches!(&*message, Message::AuditPreempt { .. }) =>
                    {
                        if let Message::AuditPreempt { shares } = *message {
                            for id in shares {
                                crate::model::validate_hash(&id)?;
                                pending_wakes.insert(id);
                            }
                        }
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
            Ok(None) => {
                *session_complete = true;
                return Ok(aggregate);
            }
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
    *session_complete = true;
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
            audit: self.focus.is_none(),
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
    fn finish_unlink(&mut self) -> Result<()> {
        Ok(())
    }
}
#[cfg(any(test, feature = "dev-tools"))]
impl Store for LocalDevice {
    fn scan_binding(&mut self) -> Result<String> {
        self.active
            .as_mut()
            .context("no active Share")?
            .scan_binding()
    }
    fn stream_namespace(
        &mut self,
        io: &mut (impl std::io::Read + std::io::Write),
    ) -> Result<Option<crate::model::Namespace>> {
        self.active
            .as_mut()
            .context("no active Share")?
            .stream_namespace(io)
    }
    fn start_hash_stream(&mut self, paths: &std::collections::BTreeSet<String>) -> Result<()> {
        self.active
            .as_mut()
            .context("no active Share")?
            .start_hash_stream(paths)
    }
    fn next_hash_chunk(
        &mut self,
        io: &mut (impl std::io::Read + std::io::Write),
    ) -> Result<Option<crate::model::Manifest>> {
        self.active
            .as_mut()
            .context("no active Share")?
            .next_hash_chunk(io)
    }
    fn validate_scan_snapshot(&mut self) -> Result<()> {
        self.active
            .as_mut()
            .context("no active Share")?
            .validate_scan_snapshot()
    }
    fn commit_scan(&mut self) -> Result<()> {
        self.active
            .as_mut()
            .context("no active Share")?
            .commit_scan()
    }
    fn discard_scan(&mut self) -> Result<()> {
        self.active
            .as_mut()
            .context("no active Share")?
            .discard_scan()
    }
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

    pub(super) fn share(path: &str) -> ShareDefinition {
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
    fn manual_selection_consumes_only_negotiated_remaining_shares() {
        let mut shares = vec![
            share("A"),
            share("B"),
            share("C"),
            share("disabled"),
            share("unavailable"),
        ];
        for (index, share) in shares.iter_mut().enumerate() {
            share.share_id = format!("{index:064x}");
        }
        shares[3].enabled = false;
        let available = shares[..4]
            .iter()
            .map(|share| share.share_id.clone())
            .collect::<Vec<_>>();
        let mut queue = share_queue(&shares, &available, None);
        take_share(&mut queue, &shares[2].share_id).unwrap();
        assert_eq!(
            queue,
            std::collections::VecDeque::from([
                shares[0].share_id.clone(),
                shares[1].share_id.clone()
            ])
        );
        for id in [
            &shares[2].share_id,
            &shares[3].share_id,
            &shares[4].share_id,
            &"f".repeat(64),
        ] {
            assert!(take_share(&mut queue, id).is_err());
        }
        let focus = std::collections::BTreeSet::from([shares[0].share_id.clone()]);
        let mut focused = share_queue(&shares, &available, Some(&focus));
        assert!(take_share(&mut focused, &shares[2].share_id).is_err());
        take_share(&mut queue, &shares[0].share_id).unwrap();
        take_share(&mut queue, &shares[1].share_id).unwrap();
        assert!(take_share(&mut queue, &shares[0].share_id).is_err());
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

/// Exercise early failure, normal completion, and early unlink on the actual round entrypoint.
#[cfg(test)]
pub(crate) fn check_round_lifecycle() {
    use std::io::{Cursor, Read, Write};
    struct Client<'a> {
        device: &'a mut LocalDevice,
        local_failure: bool,
        selection_failure: bool,
    }
    impl Store for Client<'_> {
        fn scan(&mut self) -> Result<crate::model::Manifest> {
            self.device.scan()
        }
        fn snapshot(
            &mut self,
            path: &str,
            entry: &crate::model::Entry,
        ) -> Result<crate::storage::Snapshot> {
            self.device.snapshot(path, entry)
        }
        fn install(
            &mut self,
            path: &str,
            expected: Option<&str>,
            entry: &crate::model::Entry,
            staged: &crate::storage::VerifiedStaged,
        ) -> Result<()> {
            self.device.install(path, expected, entry, staged)
        }
    }
    impl ManagedClient for Client<'_> {
        fn configure(&mut self, shares: &[ShareDefinition]) -> Result<()> {
            self.device.configure(shares)
        }
        fn session_state(&mut self) -> Result<ClientState> {
            self.device.session_state()
        }
        fn select(&mut self, id: &str) -> Result<()> {
            if self.selection_failure {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "SAF binding became unavailable after Capabilities",
                )
                .into());
            }
            self.device.select(id)
        }
        fn acknowledge_share_requests(
            &mut self,
            accepted: &[String],
            rejected: &[String],
            cancelled: &[String],
        ) -> Result<()> {
            self.device
                .acknowledge_share_requests(accepted, rejected, cancelled)
        }
        fn finish_unlink(&mut self) -> Result<()> {
            self.device.finish_unlink()
        }
        fn finish_round(&mut self, _: &Report) -> Result<()> {
            if self.local_failure {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "local SAF scan failed",
                )
                .into())
            } else {
                Ok(())
            }
        }
    }
    struct ScriptIo {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
        interrupted: bool,
    }
    impl Read for ScriptIo {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if std::mem::take(&mut self.interrupted) {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            self.input.read(buffer)
        }
    }
    impl Write for ScriptIo {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.output.write(data)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for mode in [
        "eof",
        "success",
        "unlink",
        "local_failure",
        "selection_failure",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut device = LocalDevice::open(directory.path()).unwrap();
        let selected = tests::share("selected");
        let shares = if mode == "selection_failure" {
            device.bind(&selected.share_id, "selected").unwrap();
            vec![selected.clone()]
        } else {
            vec![]
        };
        let mut store = Client {
            device: &mut device,
            local_failure: mode == "local_failure",
            selection_failure: mode == "selection_failure",
        };
        let mut input = Vec::new();
        if mode != "eof" {
            protocol::send(&mut input, &Message::Shares { shares }).unwrap();
            protocol::send(
                &mut input,
                &if mode == "unlink" {
                    Message::DeviceUnlinked
                } else {
                    Message::ShareRequestStatus {
                        accepted: vec![],
                        rejected: vec![],
                        cancelled: vec![],
                    }
                },
            )
            .unwrap();
            if mode == "selection_failure" {
                protocol::send(
                    &mut input,
                    &Message::SelectShare {
                        share_id: selected.share_id.clone(),
                    },
                )
                .unwrap();
            }
            if mode == "success" || mode == "local_failure" {
                protocol::send(&mut input, &Message::SessionDone).unwrap();
            }
        }
        let mut io = ScriptIo {
            input: Cursor::new(input),
            output: vec![],
            interrupted: true,
        };
        let result = client_round_on(&mut io, "test-device", &mut store);
        if mode == "eof" {
            let error = result.unwrap_err();
            assert!(
                !error
                    .downcast_ref::<RoundFailure>()
                    .unwrap()
                    .stream_reusable
            );
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
        } else if mode == "selection_failure" {
            let error = result.unwrap_err();
            assert!(
                !error
                    .downcast_ref::<RoundFailure>()
                    .unwrap()
                    .stream_reusable
            );
            let mut output = Cursor::new(io.output);
            assert!(matches!(
                protocol::receive(&mut output).unwrap(),
                Message::StartRound
            ));
            assert!(matches!(
                protocol::receive(&mut output).unwrap(),
                Message::Capabilities { .. }
            ));
            let rejection = protocol::receive(&mut output).unwrap_err();
            assert!(rejection
                .to_string()
                .contains("SAF binding became unavailable"));
            assert!(!rejection.is::<std::io::Error>()); // PC sees rejection, not EOF.
        } else if mode == "local_failure" {
            assert!(
                result
                    .unwrap_err()
                    .downcast_ref::<RoundFailure>()
                    .unwrap()
                    .stream_reusable
            );
        } else {
            assert!(result.is_ok(), "{mode}: {result:?}");
        }
    }
}
