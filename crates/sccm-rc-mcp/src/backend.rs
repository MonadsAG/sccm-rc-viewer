//! What the MCP server drives. Implemented by the headless session and by the
//! live viewer client (later tasks).

use async_trait::async_trait;
use sccm_rc_core::agent::proto::{Command, ErrorCode, Reply};

#[async_trait]
pub trait Backend {
    async fn connect(&mut self, target: &str, force: bool) -> Reply;
    async fn run(&mut self, cmd: Command) -> Reply;
    async fn shutdown(&mut self);
}

/// Placeholder until the real backends exist.
pub struct Unimplemented;

#[async_trait]
impl Backend for Unimplemented {
    async fn connect(&mut self, _target: &str, _force: bool) -> Reply {
        not_implemented()
    }
    async fn run(&mut self, _cmd: Command) -> Reply {
        not_implemented()
    }
    async fn shutdown(&mut self) {}
}

fn not_implemented() -> Reply {
    Reply::Error {
        code: ErrorCode::Session,
        message: "backend not implemented yet".into(),
        completed_steps: None,
    }
}
