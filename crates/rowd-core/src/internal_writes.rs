//! Functional watcher state, independent of tracing. Content is the authority.
use crate::{hash_reader, model::Entry};
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};

struct Expected {
    entry: Entry,
    parents: Vec<PathBuf>,
    finished: bool,
    expires: Instant,
}
fn registry() -> &'static (Mutex<BTreeMap<PathBuf, Expected>>, Condvar) {
    static REGISTRY: OnceLock<(Mutex<BTreeMap<PathBuf, Expected>>, Condvar)> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}
pub struct InternalWrite {
    path: PathBuf,
    complete: bool,
}
pub fn register(path: &Path, root: &Path, entry: &Entry) -> InternalWrite {
    let mut parents = Vec::new();
    let mut parent = path.parent();
    while let Some(p) = parent.filter(|p| p.starts_with(root) && *p != root) {
        if !p.exists() {
            parents.push(p.to_owned());
        }
        parent = p.parent();
    }
    let (lock, _) = registry();
    let mut state = lock.lock().unwrap();
    state.retain(|_, e| e.expires > Instant::now());
    state.insert(
        path.to_owned(),
        Expected {
            entry: entry.clone(),
            parents,
            finished: false,
            expires: Instant::now() + Duration::from_secs(30),
        },
    );
    crate::trace_event!(
        crate::trace::Level::Debug,
        crate::trace::Component::Watcher,
        "INTERNAL_WRITE_REGISTERED",
        serde_json::json!({"path":path,"hash":entry.hash})
    );
    InternalWrite {
        path: path.to_owned(),
        complete: false,
    }
}
impl InternalWrite {
    pub fn complete(mut self) {
        let (lock, wake) = registry();
        if let Some(expected) = lock.lock().unwrap().get_mut(&self.path) {
            expected.finished = true;
        }
        self.complete = true;
        wake.notify_all();
    }
}
impl Drop for InternalWrite {
    fn drop(&mut self) {
        if !self.complete {
            registry().0.lock().unwrap().remove(&self.path);
            registry().1.notify_all();
        }
    }
}
/// Suppress only a known installation whose actual bytes still match. A concurrent
/// external edit or deletion is never hidden by a time window or a trace label.
pub fn matches(path: &Path, directory_created: bool) -> bool {
    let (lock, wake) = registry();
    let mut state = lock.lock().unwrap();
    state.retain(|_, e| e.expires > Instant::now());
    let Some(target) = state
        .iter()
        .find(|(p, e)| {
            p.as_path() == path || (directory_created && e.parents.iter().any(|p| p == path))
        })
        .map(|(p, _)| p.clone())
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.get(&target).is_some_and(|e| !e.finished) {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        state = wake.wait_timeout(state, remaining).unwrap().0;
    }
    let Some(expected) = state.get(&target).map(|e| e.entry.clone()) else {
        return false;
    };
    drop(state);
    let matched = File::open(&target)
        .ok()
        .and_then(|f| hash_reader(f).ok())
        .is_some_and(|(hash, size)| hash == expected.hash && size == expected.size);
    crate::trace_event!(
        crate::trace::Level::Debug,
        crate::trace::Component::Watcher,
        if matched {
            "INTERNAL_WRITE_MATCHED"
        } else {
            "INTERNAL_WRITE_MISMATCH"
        },
        serde_json::json!({"path":path})
    );
    if !matched {
        lock.lock().unwrap().remove(&target);
    }
    matched
}
