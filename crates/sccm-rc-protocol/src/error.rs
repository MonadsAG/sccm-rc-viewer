use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("sspi: {0}")]
    Sspi(String),

    #[error("protocol: {0}")]
    Protocol(String),

    /// The server refused this logon or this user (for example
    /// `ERROR_LOGON_DENIED`: wrong password, or the user is not a Permitted
    /// Viewer on the target). Retrying with the same credentials cannot help
    /// and risks locking the account.
    #[error("logon rejected by the target: {0}")]
    LogonRejected(String),

    #[error("connection refused — target may not have CcmExec running, or TCP/2701 is blocked")]
    Refused,

    #[error("session arbitration denied by remote user")]
    ArbitrationDenied,

    #[error("session arbitration timed out (no response from remote user)")]
    ArbitrationTimeout,

    #[error("a remote-control session is already active on the target (possibly a previous session of yours that was not released)")]
    ExistingSession,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
