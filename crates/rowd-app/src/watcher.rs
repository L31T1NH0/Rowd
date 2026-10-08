use notify::{Event, EventKind};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, TrySendError},
        Mutex,
    },
};

#[cfg(target_os = "linux")]
pub(crate) fn exclude_private(watcher: &mut notify::RecommendedWatcher, root: &std::path::Path) {
    use notify::Watcher;
    // inotify can remove a subtree without removing its parent watch.
    // Best effort: ingress still filters .rowd if exclusion is unavailable.
    // ponytail: a recreated .rowd may be watched again; re-exclude if idle traces show churn.
    let _ = watcher.unwatch(&root.join(".rowd"));
}

#[derive(Default)]
pub(crate) struct Ingress {
    pub roots: Mutex<BTreeMap<String, PathBuf>>,
    received: AtomicU64,
    dropped: AtomicU64,
    queued: AtomicU64,
    overflow: AtomicU64,
    recovery: Mutex<BTreeSet<String>>,
}

fn irrelevant(event: &Event) -> bool {
    // A rescan flag reports lost events, even if delivered alongside Access.
    if event.need_rescan() {
        return false;
    }
    matches!(event.kind, EventKind::Access(_))
        || (!event.paths.is_empty()
            && event
                .paths
                .iter()
                .all(|path| path.components().any(|part| part.as_os_str() == ".rowd")))
    // .rowdignore writes remain relevant: they invalidate the manifest's rules.
}

impl Ingress {
    pub fn send(&self, tx: &SyncSender<notify::Result<Event>>, event: notify::Result<Event>) {
        self.received.fetch_add(1, Ordering::Relaxed);
        if event.as_ref().is_ok_and(irrelevant) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if event.as_ref().is_ok_and(|event| {
            !event.need_rescan()
                && !event.paths.is_empty()
                && event.paths.iter().all(|path| {
                    path.components().any(|part| part.as_os_str() == ".rowd")
                        || rowd_core::internal_writes::matches(
                            path,
                            matches!(
                                event.kind,
                                EventKind::Create(notify::event::CreateKind::Folder)
                            ),
                        )
                })
        }) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match tx.try_send(event) {
            Ok(()) => {
                self.queued.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(event)) => {
                self.overflow.fetch_add(1, Ordering::Relaxed);
                let roots = self.roots.lock().unwrap();
                let mut recovery = self.recovery.lock().unwrap();
                match event {
                    Ok(event) if !event.paths.is_empty() && !event.need_rescan() => {
                        for (id, root) in roots.iter() {
                            if event.paths.iter().any(|path| path.starts_with(root)) {
                                recovery.insert(id.clone());
                            }
                        }
                    }
                    _ => {
                        recovery.extend(roots.keys().cloned());
                    }
                }
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
    pub fn drain_metrics(&self) -> [u64; 4] {
        [&self.received, &self.dropped, &self.queued, &self.overflow]
            .map(|counter| counter.swap(0, Ordering::Relaxed))
    }
    pub fn take_recovery(&self) -> BTreeSet<String> {
        std::mem::take(&mut *self.recovery.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind};
    #[cfg(target_os = "linux")]
    #[test]
    fn private_subtree_reads_are_not_observed_but_user_edits_still_are() {
        use notify::{RecursiveMode, Watcher};
        use std::{
            fs,
            time::{Duration, Instant},
        };

        let root = tempfile::tempdir().unwrap();
        let private = root.path().join(".rowd/recovery");
        let public = root.path().join("photos");
        fs::create_dir_all(&private).unwrap();
        fs::create_dir(&public).unwrap();
        let journal = private.join("journal.json");
        fs::write(&journal, "{}").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            tx.send(event).unwrap();
        })
        .unwrap();
        watcher
            .watch(root.path(), RecursiveMode::Recursive)
            .unwrap();
        exclude_private(&mut watcher, root.path());
        std::thread::sleep(Duration::from_millis(100));
        rx.try_iter().for_each(drop);

        for _ in 0..200 {
            fs::read(&journal).unwrap();
        }
        fs::write(private.join("internal.json"), "{}").unwrap();
        let photo = public.join("new.jpg");
        fs::write(&photo, "photo").unwrap();
        let ignore = root.path().join(".rowdignore");
        fs::write(&ignore, "*.tmp").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut photo_seen = false;
        let mut ignore_seen = false;
        while !photo_seen || !ignore_seen {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
                .unwrap();
            assert!(
                !event.paths.iter().any(|path| path.starts_with(&private)),
                "{event:?}"
            );
            if !matches!(event.kind, EventKind::Access(_)) {
                photo_seen |= event.paths.contains(&photo);
                ignore_seen |= event.paths.contains(&ignore);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        for event in rx.try_iter() {
            assert!(!event
                .unwrap()
                .paths
                .iter()
                .any(|path| path.starts_with(&private)));
        }
    }

    #[test]
    fn opens_and_internal_events_never_occupy_queue_or_request_recovery() {
        let ingress = Ingress::default();
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        for _ in 0..50_000 {
            ingress.send(
                &tx,
                Ok(
                    Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)))
                        .add_path("/share/file".into()),
                ),
            );
            ingress.send(
                &tx,
                Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                    .add_path("/share/.rowd/cache".into())),
            );
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(ingress.drain_metrics(), [100_000, 100_000, 0, 0]);
        assert!(ingress.take_recovery().is_empty());
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Remove(RemoveKind::File),
        ] {
            ingress.send(&tx, Ok(Event::new(kind).add_path("/share/file".into())));
            assert_eq!(rx.try_recv().unwrap().unwrap().kind, kind);
        }
        ingress.send(
            &tx,
            Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                .add_path("/share/.rowdignore".into())),
        );
        assert!(rx.try_recv().is_ok());
    }
    #[test]
    fn internal_mixed_rename_is_filtered_before_queue_and_external_edits_survive() {
        use notify::event::RenameMode;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("image.png");
        let entry = rowd_core::model::Entry {
            hash: rowd_core::hash_reader(b"image".as_slice()).unwrap().0,
            size: 5,
        };
        let write = rowd_core::internal_writes::register(&target, root.path(), &entry);
        std::fs::write(&target, b"image").unwrap();
        write.complete();
        let ingress = Ingress::default();
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let rename = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(root.path().join(".rowd/.tmp-install"))
            .add_path(target.clone());
        ingress.send(&tx, Ok(rename));
        assert!(
            rx.try_recv().is_err(),
            "own rename must never wait in the queue until TTL expiry"
        );
        std::fs::write(&target, b"external edit").unwrap();
        ingress.send(
            &tx,
            Ok(Event::new(EventKind::Modify(ModifyKind::Any)).add_path(target.clone())),
        );
        assert!(rx.try_recv().is_ok());
        std::fs::remove_file(&target).unwrap();
        ingress.send(
            &tx,
            Ok(Event::new(EventKind::Remove(RemoveKind::File)).add_path(target)),
        );
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn true_overflow_coalesces_only_affected_shares_and_rescan_is_preserved() {
        let ingress = Ingress::default();
        ingress
            .roots
            .lock()
            .unwrap()
            .extend([("a".into(), "/a".into()), ("b".into(), "/b".into())]);
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        for _ in 0..10_000 {
            ingress.send(
                &tx,
                Ok(Event::new(EventKind::Create(CreateKind::File)).add_path("/b/file".into())),
            );
        }
        assert_eq!(ingress.drain_metrics(), [10_000, 0, 1, 9_999]);
        assert_eq!(ingress.take_recovery(), BTreeSet::from(["b".into()]));
        assert!(ingress.take_recovery().is_empty());
        let mut rescan = Event::new(EventKind::Access(AccessKind::Any));
        rescan.attrs.set_flag(notify::event::Flag::Rescan);
        ingress.send(&tx, Ok(rescan));
        assert_eq!(
            ingress.take_recovery(),
            BTreeSet::from(["a".into(), "b".into()])
        );
    }
}
