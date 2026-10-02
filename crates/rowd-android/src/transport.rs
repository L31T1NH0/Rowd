use std::io::ErrorKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    RecoverableIoInterruption,
    TransportInvalid,
    ProtocolFatal,
    ShareOperationError,
    FilesystemError,
    Cancelled,
    NetworkGenerationChanged,
}
impl FailureKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::RecoverableIoInterruption => "recoverable_io_interruption",
            Self::TransportInvalid => "transport_reconnect",
            Self::ProtocolFatal => "protocol_fatal",
            Self::ShareOperationError => "share_error",
            Self::FilesystemError => "filesystem_error",
            Self::Cancelled => "cancelled",
            Self::NetworkGenerationChanged => "network_generation_changed",
        }
    }
    pub fn invalidates(self) -> bool {
        matches!(
            self,
            Self::TransportInvalid | Self::ProtocolFatal | Self::NetworkGenerationChanged
        )
    }
}
#[derive(Debug)]
pub struct LocalFilesystemError(pub String);
impl std::fmt::Display for LocalFilesystemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for LocalFilesystemError {}

#[derive(Debug)]
pub struct ConnectionAttemptFailure;
impl std::fmt::Display for ConnectionAttemptFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection attempt failed")
    }
}
impl std::error::Error for ConnectionAttemptFailure {}

#[derive(Debug)]
pub struct CancelledError;
impl std::fmt::Display for CancelledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sincronização cancelada")
    }
}
impl std::error::Error for CancelledError {}

/// Preserve remote hints even when a completed session reports a local failure.
#[derive(Debug)]
pub struct PendingWakes(pub Vec<String>);
impl std::fmt::Display for PendingWakes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "completed session ({} pending wakes)", self.0.len())
    }
}
impl std::error::Error for PendingWakes {}

pub fn classify(error: &anyhow::Error, generation_changed: bool, cancelled: bool) -> FailureKind {
    if generation_changed {
        return FailureKind::NetworkGenerationChanged;
    }
    if error.is::<ConnectionAttemptFailure>() {
        return FailureKind::TransportInvalid;
    }
    if let Some(io) = error.downcast_ref::<std::io::Error>() {
        if matches!(
            io.kind(),
            ErrorKind::UnexpectedEof
                | ErrorKind::BrokenPipe
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::NotConnected
        ) {
            return FailureKind::TransportInvalid;
        }
    }
    // An unfinished frame/session cannot safely be replayed, even for a local error or cancel.
    let round = error.downcast_ref::<rowd_core::managed::RoundFailure>();
    if round.is_some_and(|round| !round.stream_reusable) {
        return FailureKind::ProtocolFatal;
    }
    if cancelled || error.is::<CancelledError>() {
        return FailureKind::Cancelled;
    }
    if error.is::<LocalFilesystemError>() {
        return FailureKind::FilesystemError;
    }
    if round.is_some() {
        return FailureKind::ShareOperationError;
    }
    if let Some(io) = error.downcast_ref::<std::io::Error>() {
        if io.kind() == ErrorKind::Interrupted {
            return FailureKind::RecoverableIoInterruption;
        }
        return FailureKind::ShareOperationError;
    }
    // Errors before starting a protocol round do not prove stream corruption.
    FailureKind::ShareOperationError
}

#[derive(Debug, PartialEq, Eq)]
pub enum PollWake {
    None,
    Share(String),
    TransportInvalid,
}
impl PollWake {
    pub fn json(&self) -> String {
        match self {
            Self::None => serde_json::json!({"kind":"none"}),
            Self::Share(id) => serde_json::json!({"kind":"share","share_id":id}),
            Self::TransportInvalid => serde_json::json!({"kind":"transport_invalid"}),
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classification_requires_evidence_to_clear() {
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
        ] {
            let error = anyhow::Error::new(std::io::Error::from(kind));
            assert!(classify(&error, false, false).invalidates());
        }
        let local = anyhow::Error::new(LocalFilesystemError("SAF scan failed".into()));
        assert_eq!(classify(&local, false, false), FailureKind::FilesystemError);
        assert!(!classify(&local, false, false).invalidates());
        let completed_local = anyhow::Error::new(LocalFilesystemError("local scan".into()))
            .context(rowd_core::managed::RoundFailure {
                stream_reusable: true,
            });
        assert_eq!(
            classify(&completed_local, false, false),
            FailureKind::FilesystemError
        );
        assert!(!classify(&completed_local, false, false).invalidates());
        let unfinished = local.context(rowd_core::managed::RoundFailure {
            stream_reusable: false,
        });
        assert!(classify(&unfinished, false, false).invalidates());
        let interrupted = anyhow::Error::new(std::io::Error::from(ErrorKind::Interrupted));
        assert!(!classify(&interrupted, false, false).invalidates());
        assert!(classify(&interrupted, true, false).invalidates());
        assert!(!classify(&interrupted, false, true).invalidates());
        let completed =
            anyhow::anyhow!("Share skipped").context(rowd_core::managed::RoundFailure {
                stream_reusable: true,
            });
        assert!(!classify(&completed, false, false).invalidates());
    }
    #[test]
    fn poll_result_is_explicit() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&PollWake::TransportInvalid.json()).unwrap()
                ["kind"],
            "transport_invalid"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&PollWake::Share("X".into()).json()).unwrap()
                ["share_id"],
            "X"
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum WakeReadiness {
    Idle,
    Eof,
    Buffered(u8),
    SocketData,
}

pub fn wait_for_wake(
    mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
    mut peek: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
) -> std::io::Result<WakeReadiness> {
    let mut byte = [0];
    match rowd_core::io_retry::interrupted("poll_wake_tls_read", || read(&mut byte)) {
        Ok(0) => return Ok(WakeReadiness::Eof),
        Ok(_) => return Ok(WakeReadiness::Buffered(byte[0])),
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        Err(e) => return Err(e),
    }
    match rowd_core::io_retry::interrupted("poll_wake_peek", || peek(&mut byte)) {
        Ok(0) => Ok(WakeReadiness::Eof),
        Ok(_) => Ok(WakeReadiness::SocketData),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            Ok(WakeReadiness::Idle)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    #[test]
    fn eintr_in_poll_retries_before_returning_idle() {
        let mut read_attempts = 0;
        let mut peek_attempts = 0;
        let result = wait_for_wake(
            |_| {
                read_attempts += 1;
                Err(if read_attempts == 1 {
                    ErrorKind::Interrupted
                } else {
                    ErrorKind::WouldBlock
                }
                .into())
            },
            |_| {
                peek_attempts += 1;
                Err(if peek_attempts == 1 {
                    ErrorKind::Interrupted
                } else {
                    ErrorKind::TimedOut
                }
                .into())
            },
        )
        .unwrap();
        assert_eq!(result, WakeReadiness::Idle);
        assert_eq!((read_attempts, peek_attempts), (2, 2));
    }
    #[test]
    fn real_eof_and_broken_stream_require_reconnect() {
        assert_eq!(
            wait_for_wake(|_| Err(ErrorKind::WouldBlock.into()), |_| Ok(0)).unwrap(),
            WakeReadiness::Eof
        );
        assert_eq!(
            wait_for_wake(|_| Ok(0), |_| panic!("TLS EOF does not peek")).unwrap(),
            WakeReadiness::Eof
        );
        for kind in [ErrorKind::BrokenPipe, ErrorKind::ConnectionReset] {
            assert_eq!(
                wait_for_wake(|_| Err(ErrorKind::WouldBlock.into()), |_| Err(kind.into()))
                    .unwrap_err()
                    .kind(),
                kind
            );
        }
    }
}

/// A SAF failure before ScanReady can finish through the existing ScanDeferred exchange.
pub fn deferred_scan_result<T>(
    result: anyhow::Result<T>,
    errors: &mut Vec<String>,
) -> anyhow::Result<T> {
    match result {
        Err(error) if error.is::<LocalFilesystemError>() || error.is::<CancelledError>() => {
            errors.push(format!("{error:#}"));
            Err(rowd_core::sync::ScanDeferred.into())
        }
        other => other,
    }
}
#[cfg(test)]
mod scan_tests {
    use super::*;
    #[test]
    fn local_saf_failure_defers_without_invalidating_transport() {
        let mut errors = vec![];
        let result = deferred_scan_result::<()>(
            Err(LocalFilesystemError("SAF pollScanJson failed".into()).into()),
            &mut errors,
        );
        assert!(result.unwrap_err().is::<rowd_core::sync::ScanDeferred>());
        assert_eq!(errors.len(), 1);
        let error: anyhow::Error = LocalFilesystemError(errors.remove(0)).into();
        assert!(!classify(&error, false, false).invalidates());
        let transport: anyhow::Error = std::io::Error::from(ErrorKind::ConnectionReset).into();
        assert_eq!(
            deferred_scan_result::<()>(Err(transport), &mut errors)
                .unwrap_err()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            ErrorKind::ConnectionReset
        );
        assert!(errors.is_empty());
        let deferred =
            deferred_scan_result::<()>(Err(CancelledError.into()), &mut errors).unwrap_err();
        assert!(deferred.is::<rowd_core::sync::ScanDeferred>());
        let cancelled =
            anyhow::Error::new(CancelledError).context(rowd_core::managed::RoundFailure {
                stream_reusable: true,
            });
        assert_eq!(classify(&cancelled, false, true), FailureKind::Cancelled);
        assert!(!classify(&cancelled, false, true).invalidates());
    }
}
