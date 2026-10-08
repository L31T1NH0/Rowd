//! Retry only individual, non-consuming wait/read syscalls. Never replay a frame or write.
use std::io;

pub fn interrupted<T>(operation: &str, syscall: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    interrupted_with_control(operation, || Ok(()), syscall)
}

/// Check control before the syscall and between EINTR retries, without replaying frames.
pub fn interrupted_with_control<T>(
    operation: &str,
    control: impl FnMut() -> io::Result<()>,
    syscall: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    retry(operation, false, control, syscall)
}

/// Expected idle polling is not a failed syscall. Other errors and EINTR remain visible.
pub fn poll<T>(operation: &str, syscall: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    poll_with_control(operation, || Ok(()), syscall)
}
pub fn poll_with_control<T>(
    operation: &str,
    control: impl FnMut() -> io::Result<()>,
    syscall: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    retry(operation, true, control, syscall)
}
fn retry<T>(
    operation: &str,
    polling: bool,
    mut control: impl FnMut() -> io::Result<()>,
    mut syscall: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut attempt = 0u64;
    loop {
        control()?;
        match syscall() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                attempt += 1;
                crate::trace_event!(
                    crate::trace::Level::Debug,
                    crate::trace::Component::Connection,
                    "IO_INTERRUPTED_RETRY",
                    serde_json::json!({"operation":operation,"attempt":attempt})
                );
            }
            result => {
                if let Err(error) = &result {
                    if polling
                        && matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        )
                    {
                        return result;
                    }
                    crate::trace_event!(
                        crate::trace::Level::Debug,
                        crate::trace::Component::Connection,
                        "IO_SYSCALL_FAILED",
                        serde_json::json!({"syscall":operation,"operation":operation,
                            "error_kind":format!("{:?}", error.kind()),"errno":error.raw_os_error(),
                            "message":error.to_string()})
                    );
                }
                return result;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_poll_is_quiet_but_real_failures_remain_visible() {
        let _session = crate::trace::TEST_SESSION_LOCK.lock().unwrap();
        let round_id = crate::trace::new_id("idle-poll-test");
        let _context = crate::trace::TraceContext::default()
            .with("round_id", round_id.clone())
            .enter();
        let directory = tempfile::tempdir().unwrap();
        crate::trace::start(directory.path(), "pc", None).unwrap();
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut] {
            assert_eq!(
                poll::<()>("accept", || Err(kind.into()))
                    .unwrap_err()
                    .kind(),
                kind
            );
        }
        assert_eq!(
            poll::<()>("accept", || Err(io::ErrorKind::ConnectionReset.into()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionReset
        );
        crate::trace::stop("test_complete").unwrap();
        let trace = std::fs::read_to_string(directory.path().join("Latest-trace/trace-0001.jsonl"))
            .unwrap();
        let failures: Vec<serde_json::Value> = trace
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|event: &serde_json::Value| {
                event["event"] == "IO_SYSCALL_FAILED" && event["context"]["round_id"] == round_id
            })
            .collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["fields"]["error_kind"], "ConnectionReset");
    }

    #[test]
    fn interrupted_peek_and_wait_retry_without_invalidating() {
        for operation in [
            "scan_peek",
            "poll_wake_peek",
            "poll_wake_tls_read",
            "accept",
        ] {
            let mut attempts = 0;
            let result = interrupted(operation, || {
                attempts += 1;
                if attempts == 1 {
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    Ok(1)
                }
            });
            assert_eq!(result.unwrap(), 1);
            assert_eq!(attempts, 2);
        }
    }
    #[test]
    fn cancellation_between_eintr_retries_stops_without_replaying_any_more_io() {
        use std::cell::Cell;
        let attempts = Cell::new(0);
        let result = interrupted_with_control::<usize>(
            "peek",
            || {
                if attempts.get() == 3 {
                    Err(io::Error::other("cancelled"))
                } else {
                    Ok(())
                }
            },
            || {
                attempts.set(attempts.get() + 1);
                Err(io::ErrorKind::Interrupted.into())
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "cancelled");
        assert_eq!(attempts.get(), 3);
    }
    #[test]
    fn eof_and_transport_errors_are_not_retried() {
        assert_eq!(interrupted("peek", || Ok(0)).unwrap(), 0);
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::WouldBlock,
        ] {
            assert_eq!(
                interrupted::<usize>("peek", || Err(kind.into()))
                    .unwrap_err()
                    .kind(),
                kind
            );
        }
    }
}
