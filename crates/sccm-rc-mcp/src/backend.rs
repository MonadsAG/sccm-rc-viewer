//! What the MCP server drives. Implemented by the headless session and by the
//! live viewer client (later tasks).

use async_trait::async_trait;
use sccm_rc_core::agent::proto::{Command, ErrorCode, Reply};

/// Replies must never echo typed text: a secret type_text may be part of any
/// command, and the MCP layer cannot know what a backend put in free text.
#[async_trait]
pub trait Backend {
    async fn connect(&mut self, target: &str, force: bool) -> Reply;
    async fn run(&mut self, cmd: Command) -> Reply;
    async fn shutdown(&mut self);
}

/// Live mode on a platform without Unix sockets.
#[allow(dead_code)]
pub struct Unavailable;

#[async_trait]
impl Backend for Unavailable {
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
