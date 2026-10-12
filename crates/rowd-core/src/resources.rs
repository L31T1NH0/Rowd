//! Shared admission for local preparation; wire ownership remains with its coordinator.
use anyhow::Result;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Condvar, Mutex, OnceLock, Weak},
};

struct State {
    issued: u64,
    serving: u64,
    active: usize,
}
struct Budget {
    state: Mutex<State>,
    wake: Condvar,
    limit: usize,
}
pub struct Permit(Arc<Budget>, usize);
impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.active -= self.1;
        self.0.wake.notify_all();
    }
}

pub fn workers(name: &str, maximum: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, maximum)
}

pub fn acquire(path: &Path) -> Result<Permit> {
    #[cfg(unix)]
    let key = {
        use std::os::unix::fs::MetadataExt;
        format!("device:{}", std::fs::metadata(path)?.dev())
    };
    // Conservative shared budget when the platform exposes no stable device identity.
    #[cfg(not(unix))]
    let key = {
        let _ = path;
        "local-storage".to_owned()
    };
    static ALL: OnceLock<Mutex<HashMap<String, Weak<Budget>>>> = OnceLock::new();
    let budget = {
        let mut all = ALL.get_or_init(Default::default).lock().unwrap();
        all.retain(|_, b| b.strong_count() > 0);
        if let Some(b) = all.get(&key).and_then(Weak::upgrade) {
            b
        } else {
            let b = Arc::new(Budget {
                state: Mutex::new(State {
                    issued: 0,
                    serving: 0,
                    active: 0,
                }),
                wake: Condvar::new(),
                limit: workers("ROWD_IO_WORKERS", 4),
            });
            all.insert(key, Arc::downgrade(&b));
            b
        }
    };
    let weight = if std::fs::metadata(path)?.len() > 8 * 1024 * 1024 {
        budget.limit
    } else {
        1
    };
    Ok(admit(budget, weight))
}

pub fn acquire_hash() -> Permit {
    static HASH: OnceLock<Arc<Budget>> = OnceLock::new();
    let budget = HASH
        .get_or_init(|| {
            Arc::new(Budget {
                state: Mutex::new(State {
                    issued: 0,
                    serving: 0,
                    active: 0,
                }),
                wake: Condvar::new(),
                limit: workers("ROWD_HASH_WORKERS", 4),
            })
        })
        .clone();
    admit(budget, 1)
}

fn admit(budget: Arc<Budget>, weight: usize) -> Permit {
    let mut state = budget.state.lock().unwrap();
    let ticket = state.issued;
    state.issued += 1;
    while ticket != state.serving || state.active + weight > budget.limit {
        state = budget.wake.wait(state).unwrap();
    }
    state.serving += 1;
    state.active += weight;
    budget.wake.notify_all();
    drop(state);
    Permit(budget, weight)
}

#[cfg(test)]
mod tests {
    #[test]
    fn concurrent_credits_are_shared_and_released() {
        let dir = tempfile::tempdir().unwrap();
        let first = super::acquire(dir.path()).unwrap();
        let mut held = vec![];
        for _ in 1..first.0.limit {
            held.push(super::acquire(dir.path()).unwrap());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                let _permit = super::acquire(dir.path()).unwrap();
                tx.send(()).unwrap();
            });
            assert!(rx
                .recv_timeout(std::time::Duration::from_millis(30))
                .is_err());
            drop(first);
            drop(held);
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        });
    }
}
