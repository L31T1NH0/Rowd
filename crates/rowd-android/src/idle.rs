//! One blocking poll of TCP + a coalescing socketpair. The only idle deadline is the audit.
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::Instant;

pub const LOCAL: u8 = 1;
pub const NETWORK: u8 = 2;
pub const CANCEL: u8 = 4;

pub struct IdleWake {
    reader: UnixStream,
    writer: UnixStream,
    reasons: AtomicU8,
    pub waits: AtomicU64,
    pub polls: AtomicU64,
    pub local: AtomicU64,
    pub remote: AtomicU64,
    pub network: AtomicU64,
    pub cancel: AtomicU64,
    pub timeouts: AtomicU64,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Ready {
    Socket,
    Control(u8),
    Deadline,
}
impl IdleWake {
    pub fn new() -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        Ok(Self {
            reader,
            writer,
            reasons: AtomicU8::new(0),
            waits: AtomicU64::new(0),
            polls: AtomicU64::new(0),
            local: AtomicU64::new(0),
            remote: AtomicU64::new(0),
            network: AtomicU64::new(0),
            cancel: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
        })
    }
    pub fn signal(&self, reason: u8) -> io::Result<()> {
        if self.reasons.fetch_or(reason, Ordering::SeqCst) != 0 {
            return Ok(()); // queued byte, or waiter draining it before taking the reasons
        }
        // A one-byte notification has no partially completed state. EINTR wrote zero bytes.
        loop {
            match (&self.writer).write(&[1]) {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()), // already readable
                Err(e) => return Err(e),
            }
        }
    }
    pub fn reset_cancel(&self) {
        self.reasons.fetch_and(!CANCEL, Ordering::SeqCst);
    }
    fn take(&self) -> io::Result<u8> {
        let mut bytes = [0; 256];
        loop {
            match (&self.reader).read(&mut bytes) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(self.reasons.swap(0, Ordering::SeqCst))
    }
    pub fn wait(
        &self,
        socket: &impl AsRawFd,
        deadline: Instant,
        mut control: impl FnMut() -> io::Result<()>,
    ) -> io::Result<Ready> {
        self.waits.fetch_add(1, Ordering::Relaxed);
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: socket.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.reader.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let count =
                rowd_core::io_retry::interrupted_with_control("idle_poll", &mut control, || {
                    // Recompute after EINTR: retries never extend the audit deadline.
                    let left = deadline.saturating_duration_since(Instant::now());
                    let ms = left
                        .as_millis()
                        .saturating_add(u128::from(!left.is_zero()))
                        .min(i32::MAX as u128) as i32;
                    self.polls.fetch_add(1, Ordering::Relaxed);
                    // SAFETY: fds contains exactly two live descriptors, valid for this call.
                    let result =
                        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
                    if result < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(result)
                    }
                })?;
            if count == 0 {
                self.timeouts.fetch_add(1, Ordering::Relaxed);
                return Ok(Ready::Deadline);
            }
            if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
                return Err(io::ErrorKind::NotConnected.into());
            }
            if fds[1].revents != 0 {
                let reasons = self.take()?;
                if reasons & LOCAL != 0 {
                    self.local.fetch_add(1, Ordering::Relaxed);
                }
                if reasons & NETWORK != 0 {
                    self.network.fetch_add(1, Ordering::Relaxed);
                }
                if reasons & CANCEL != 0 {
                    self.cancel.fetch_add(1, Ordering::Relaxed);
                }
                if reasons != 0 {
                    return Ok(Ready::Control(reasons));
                }
                // Concurrent notification consumed above; no periodic retry, wait again.
            }
            if fds[0].revents != 0 {
                return Ok(Ready::Socket);
            }
        }
    }
    pub fn metrics(&self) -> serde_json::Value {
        serde_json::json!({"idle_waits":self.waits.load(Ordering::Relaxed),"local_wakes":self.local.load(Ordering::Relaxed),
            "remote_wakes":self.remote.load(Ordering::Relaxed),"network_wakes":self.network.load(Ordering::Relaxed),
            "cancel_wakes":self.cancel.load(Ordering::Relaxed),"timeouts":self.timeouts.load(Ordering::Relaxed),
            "polls":self.polls.load(Ordering::Relaxed),"empty_poll_count":0})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    #[test]
    fn local_network_cancel_wake_a_blocked_idle_without_periodic_timeouts() {
        for reason in [LOCAL, NETWORK, CANCEL] {
            let wake = Arc::new(IdleWake::new().unwrap());
            let (socket, _peer) = UnixStream::pair().unwrap();
            let waiter = wake.clone();
            let thread = std::thread::spawn(move || {
                waiter
                    .wait(&socket, Instant::now() + Duration::from_secs(30), || Ok(()))
                    .unwrap()
            });
            std::thread::sleep(Duration::from_millis(80)); // only test harness delays the notification
            let start = Instant::now();
            wake.signal(reason).unwrap();
            assert_eq!(thread.join().unwrap(), Ready::Control(reason));
            assert!(start.elapsed() < Duration::from_millis(500));
            assert_eq!(wake.metrics()["idle_waits"], 1);
            assert_eq!(wake.metrics()["timeouts"], 0);
            assert_eq!(wake.metrics()["empty_poll_count"], 0);
        }
    }
    #[test]
    fn remote_bytes_and_pre_wait_local_notifications_are_not_lost() {
        let wake = IdleWake::new().unwrap();
        let (socket, mut peer) = UnixStream::pair().unwrap();
        wake.signal(LOCAL).unwrap();
        assert_eq!(
            wake.wait(
                &socket,
                Instant::now() + std::time::Duration::from_secs(30),
                || Ok(())
            )
            .unwrap(),
            Ready::Control(LOCAL)
        );
        peer.write_all(b"WakeShare").unwrap();
        assert_eq!(
            wake.wait(
                &socket,
                Instant::now() + std::time::Duration::from_secs(30),
                || Ok(())
            )
            .unwrap(),
            Ready::Socket
        );
        assert_eq!(wake.metrics()["timeouts"], 0);
    }
    #[test]
    fn idle_does_not_return_or_repoll_at_one_second() {
        let wake = Arc::new(IdleWake::new().unwrap());
        let (socket, _peer) = UnixStream::pair().unwrap();
        let waiter = wake.clone();
        let (done, completed) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = waiter.wait(&socket, Instant::now() + Duration::from_secs(30), || Ok(()));
            done.send(result).unwrap();
        });
        assert!(matches!(
            completed.recv_timeout(Duration::from_millis(1250)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(wake.metrics()["idle_waits"], 1);
        assert_eq!(wake.metrics()["polls"], 1);
        assert_eq!(wake.metrics()["timeouts"], 0);
        wake.signal(LOCAL).unwrap();
        assert_eq!(
            completed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
            Ready::Control(LOCAL)
        );
        thread.join().unwrap();
    }
    #[test]
    fn reset_cancel_keeps_local_and_network_notifications() {
        let wake = IdleWake::new().unwrap();
        let (socket, _peer) = UnixStream::pair().unwrap();
        wake.signal(LOCAL | NETWORK | CANCEL).unwrap();
        wake.reset_cancel();
        assert_eq!(
            wake.wait(&socket, Instant::now() + Duration::from_secs(30), || Ok(()))
                .unwrap(),
            Ready::Control(LOCAL | NETWORK)
        );
        assert_eq!(wake.metrics()["cancel_wakes"], 0);
    }
    #[test]
    fn only_audit_deadline_times_out() {
        let wake = IdleWake::new().unwrap();
        let (socket, _peer) = UnixStream::pair().unwrap();
        assert_eq!(
            wake.wait(&socket, Instant::now(), || Ok(())).unwrap(),
            Ready::Deadline
        );
        assert_eq!(wake.metrics()["timeouts"], 1);
    }
}
