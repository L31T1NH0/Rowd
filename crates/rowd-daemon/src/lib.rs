//! Rowd daemon protocol and platform implementation.
pub mod protocol;
pub use protocol::{Reply, Request, DAEMON_IPC_VERSION};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(not(unix))]
compile_error!("rowd-daemon V1 supports Unix only");
