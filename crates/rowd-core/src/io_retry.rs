//! Retry only individual, non-consuming wait/read syscalls. Never replay a frame or write.
use std::io;

pub fn interrupted<T>(
    operation: &str,
    mut syscall: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut attempt = 0u64;
    loop {
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
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
