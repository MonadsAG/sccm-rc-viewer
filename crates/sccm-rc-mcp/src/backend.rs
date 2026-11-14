//! What the MCP server drives. Implemented by the headless session, by the
//! live viewer client and by the router that offers both (`--mode both`).

use async_trait::async_trait;
use sccm_rc_core::agent::exec::bad_args;
use sccm_rc_core::agent::proto::{Command, ErrorCode, Reply};

/// The mode a `connect` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectMode {
    Live,
    Headless,
}

impl ConnectMode {
    pub fn name(self) -> &'static str {
        match self {
            ConnectMode::Live => "live",
            ConnectMode::Headless => "headless",
        }
    }
}

/// Replies must never echo typed text: a secret type_text may be part of any
/// command, and the MCP layer cannot know what a backend put in free text.
#[async_trait]
pub trait Backend: Send {
    async fn connect(&mut self, target: &str, force: bool) -> Reply;
    async fn run(&mut self, cmd: Command) -> Reply;
    async fn shutdown(&mut self);

    /// The one mode this backend serves; `None` for the router (both).
    fn mode(&self) -> Option<ConnectMode> {
        None
    }

    /// `connect` with an optional wanted mode. A single-mode backend refuses
    /// the other mode instead of silently using its own.
    async fn connect_as(&mut self, target: &str, force: bool, mode: Option<ConnectMode>) -> Reply {
        match (mode, self.mode()) {
            (Some(want), Some(have)) if want != have => bad_args(format!(
                "this server runs in {} mode; register it with --mode both to use {} mode",
                have.name(),
                want.name()
            )),
            _ => self.connect(target, force).await,
        }
    }
}

/// Live mode on a platform without Unix sockets.
#[allow(dead_code)]
pub struct Unavailable;

#[async_trait]
impl Backend for Unavailable {
    fn mode(&self) -> Option<ConnectMode> {
        Some(ConnectMode::Live)
    }
    async fn connect(&mut self, _target: &str, _force: bool) -> Reply {
        unavailable()
    }
    async fn run(&mut self, _cmd: Command) -> Reply {
        unavailable()
    }
    async fn shutdown(&mut self) {}
}

#[allow(dead_code)]
fn unavailable() -> Reply {
    Reply::Error {
        code: ErrorCode::ControlDisabled,
        message: "live mode is only available on macOS/Linux".into(),
        completed_steps: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_mode_backend_refuses_the_other_mode() {
        let mut b = Unavailable;
        match b.connect_as("t", false, Some(ConnectMode::Headless)).await {
            Reply::Error { code, message, .. } => {
                assert_eq!(code, ErrorCode::BadArgs);
                assert!(message.contains("--mode both"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        // Its own mode, or none, reaches connect().
        for mode in [Some(ConnectMode::Live), None] {
            match b.connect_as("t", false, mode).await {
                Reply::Error { code, .. } => assert_eq!(code, ErrorCode::ControlDisabled),
                other => panic!("{other:?}"),
            }
        }
    }
}
