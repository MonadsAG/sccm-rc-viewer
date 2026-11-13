//! Live backend: relays commands to the user's running viewer over its
//! control socket. Never connects to a till itself; the viewer's session is
//! the only session.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use sccm_rc_core::agent::proto::{Command, ErrorCode, Reply};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::backend::Backend;

const SOCKET_NAME: &str = "control.sock";
const MSG_DISABLED: &str = "enable 'Allow control by Claude' in the viewer";
const MSG_TIMEOUT: &str = "the viewer did not answer in time";
/// Reply deadline for commands without a time of their own.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Added to a wait's or batch's own time budget.
const SLACK: Duration = Duration::from_secs(10);
/// The viewer's pause between batch steps, rounded up.
const STEP_BUDGET: Duration = Duration::from_millis(100);
/// The viewer types text in chunks of this many characters (64 key events),
/// with `TEXT_GAP` between chunks.
const TEXT_CHARS_PER_CHUNK: u32 = 32;
const TEXT_GAP: Duration = sccm_rc_core::agent::exec::STEP_GAP;

pub struct Live {
    socket: PathBuf,
    conn: Option<BufReader<UnixStream>>,
    slack: Duration,
}

impl Live {
    pub fn new() -> Self {
        Self::with_socket(sccm_rc_core::paths::data_dir().join(SOCKET_NAME))
    }

    pub fn with_socket(path: impl Into<PathBuf>) -> Self {
        Live {
            socket: path.into(),
            conn: None,
            slack: SLACK,
        }
    }

    async fn exchange(&mut self, cmd: &Command) -> Reply {
        if let Err(message) = cmd.validate() {
            return err(ErrorCode::BadArgs, message);
        }
        let mut line = match serde_json::to_string(cmd) {
            Ok(l) => l,
            Err(_) => return err(ErrorCode::BadArgs, "command cannot be encoded".into()),
        };
        line.push('\n');
        let limit = reply_timeout(cmd, self.slack);
        let mut conn = match self.conn.take() {
            Some(c) => c,
            None => match connect(&self.socket).await {
                Some(c) => c,
                None => return disabled(),
            },
        };
        // The connection is only kept after a complete exchange.
        let res: Result<std::io::Result<String>, _> = tokio::time::timeout(limit, async {
            conn.get_mut().write_all(line.as_bytes()).await?;
            let mut reply = String::new();
            match conn.read_line(&mut reply).await? {
                0 => Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
                _ => Ok(reply),
            }
        })
        .await;
        match res {
            Err(_) => err(ErrorCode::Session, MSG_TIMEOUT.into()),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "viewer socket closed");
                disabled()
            }
            Ok(Ok(text)) => match serde_json::from_str::<Reply>(text.trim_end()) {
                Ok(reply) => {
                    self.conn = Some(conn);
                    reply
                }
                Err(_) => err(
                    ErrorCode::Session,
                    "unreadable reply from the viewer".into(),
                ),
            },
        }
    }
}

async fn connect(path: &Path) -> Option<BufReader<UnixStream>> {
    match UnixStream::connect(path).await {
        Ok(s) => Some(BufReader::new(s)),
        Err(e) => {
            tracing::info!(error = %e, "viewer control socket unavailable");
            None
        }
    }
}

fn err(code: ErrorCode, message: String) -> Reply {
    Reply::Error {
        code,
        message,
        completed_steps: None,
    }
}

fn disabled() -> Reply {
    err(ErrorCode::ControlDisabled, MSG_DISABLED.into())
}

/// The time a command takes in the viewer by its nature: a wait's timeout,
/// the pauses between a long text's chunks.
fn own_time(cmd: &Command) -> Duration {
    match cmd {
        Command::Wait { timeout_ms, .. } => Duration::from_millis(*timeout_ms),
        Command::TypeText { text, .. } => {
            let chunks =
                u32::try_from(text.chars().count()).unwrap_or(u32::MAX) / TEXT_CHARS_PER_CHUNK;
            TEXT_GAP.saturating_mul(chunks)
        }
        _ => Duration::ZERO,
    }
}

fn reply_timeout(cmd: &Command, slack: Duration) -> Duration {
    match cmd {
        Command::Wait { .. } => own_time(cmd) + slack,
        Command::TypeText { .. } => DEFAULT_TIMEOUT + own_time(cmd),
        Command::Batch { steps } => {
            let waits: Duration = steps.iter().map(own_time).sum();
            waits + STEP_BUDGET * steps.len() as u32 + slack
        }
        _ => DEFAULT_TIMEOUT,
    }
}

#[async_trait]
impl Backend for Live {
    async fn connect(&mut self, target: &str, _force: bool) -> Reply {
        match self.exchange(&Command::Status).await {
            Reply::Status {
                connected,
                target: Some(t),
                desk_w,
                desk_h,
                mode,
                idle_s,
            } if t != target => Reply::Status {
                connected,
                mode: format!(
                    "{mode}; note: the viewer is connected to {t}; live mode uses the viewer's session"
                ),
                target: Some(t),
                desk_w,
                desk_h,
                idle_s,
            },
            other => other,
        }
    }

    async fn run(&mut self, cmd: Command) -> Reply {
        tracing::debug!(command = %variant_name(&cmd), "relaying to the viewer");
        self.exchange(&cmd).await
    }

    async fn shutdown(&mut self) {
        self.conn = None;
    }
}

/// Never the Debug/Serialize form: a type_text may carry a secret.
fn variant_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Status => "status",
        Command::Screenshot { .. } => "screenshot",
        Command::Click { .. } => "click",
        Command::Move { .. } => "move",
        Command::Scroll { .. } => "scroll",
        Command::TypeText { .. } => "type_text",
        Command::Key { .. } => "key",
        Command::Batch { .. } => "batch",
        Command::Wait { .. } => "wait",
        Command::Disconnect => "disconnect",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::net::UnixListener;

    fn temp_sock(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sccm-live-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("control.sock")
    }

    fn status(target: &str) -> Reply {
        Reply::Status {
            connected: true,
            target: Some(target.into()),
            desk_w: 800,
            desk_h: 600,
            mode: "live".into(),
            idle_s: 0,
        }
    }

    /// Fake viewer: answers every line with `answer(request line)`; counts
    /// accepted connections. Closes each connection after `per_conn` lines
    /// when given.
    fn serve(
        path: &Path,
        accepted: Arc<AtomicUsize>,
        per_conn: Option<usize>,
        answer: impl Fn(&str) -> Option<Reply> + Send + Sync + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                let mut r = BufReader::new(s);
                let mut n = 0;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    if let Some(rep) = answer(&line) {
                        let mut out = serde_json::to_string(&rep).unwrap();
                        out.push('\n');
                        r.get_mut().write_all(out.as_bytes()).await.unwrap();
                    } else {
                        // never answers
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                    n += 1;
                    if per_conn == Some(n) {
                        break;
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn relays_command_and_reply() {
        let path = temp_sock("relay");
        let accepted = Arc::new(AtomicUsize::new(0));
        let _srv = serve(&path, accepted.clone(), None, |line| {
            let cmd: Command = serde_json::from_str(line).unwrap();
            assert!(matches!(cmd, Command::Key { ref combo } if combo == "ctrl+a"));
            Some(Reply::Ok {
                text: "pressed".into(),
            })
        });
        let mut live = Live::with_socket(&path);
        for _ in 0..2 {
            let r = live
                .run(Command::Key {
                    combo: "ctrl+a".into(),
                })
                .await;
            assert_eq!(
                r,
                Reply::Ok {
                    text: "pressed".into()
                }
            );
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "connection is reused");
    }

    #[tokio::test]
    async fn missing_socket_reports_control_disabled() {
        let mut live = Live::with_socket(temp_sock("missing"));
        match live.run(Command::Status).await {
            Reply::Error { code, message, .. } => {
                assert_eq!(code, ErrorCode::ControlDisabled);
                assert_eq!(message, MSG_DISABLED);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_never_dials_a_till() {
        let path = temp_sock("connect");
        let accepted = Arc::new(AtomicUsize::new(0));
        let _srv = serve(&path, accepted.clone(), None, |line| {
            // Only Status ever reaches the viewer.
            assert!(matches!(
                serde_json::from_str::<Command>(line).unwrap(),
                Command::Status
            ));
            Some(status("till-7"))
        });
        let mut live = Live::with_socket(&path);
        assert_eq!(live.connect("till-7", true).await, status("till-7"));
        match live.connect("other-till", false).await {
            Reply::Status { target, mode, .. } => {
                assert_eq!(target.as_deref(), Some("till-7"));
                assert!(mode.contains(
                    "the viewer is connected to till-7; live mode uses the viewer's session"
                ));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reconnects_after_viewer_restart() {
        let path = temp_sock("restart");
        let accepted = Arc::new(AtomicUsize::new(0));
        let srv = serve(&path, accepted.clone(), None, |_| Some(status("t")));
        let mut live = Live::with_socket(&path);
        assert!(matches!(
            live.run(Command::Status).await,
            Reply::Status { .. }
        ));
        srv.abort();
        let _ = srv.await;
        let _ = std::fs::remove_file(&path);
        // Viewer gone: the dead connection reports ControlDisabled and is dropped.
        assert!(matches!(
            live.run(Command::Status).await,
            Reply::Error {
                code: ErrorCode::ControlDisabled,
                ..
            }
        ));
        let _srv2 = serve(&path, accepted.clone(), None, |_| Some(status("t")));
        assert!(matches!(
            live.run(Command::Status).await,
            Reply::Status { .. }
        ));
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn silent_viewer_times_out_and_drops_the_connection() {
        let path = temp_sock("timeout");
        let accepted = Arc::new(AtomicUsize::new(0));
        let _srv = serve(&path, accepted, None, |_| None);
        let mut live = Live::with_socket(&path);
        live.slack = Duration::from_millis(50);
        let r = live
            .run(Command::Wait {
                change: true,
                stable_ms: None,
                timeout_ms: 10,
            })
            .await;
        assert_eq!(r, err(ErrorCode::Session, MSG_TIMEOUT.into()));
        assert!(live.conn.is_none());
    }

    #[tokio::test]
    async fn invalid_command_is_not_sent() {
        let mut live = Live::with_socket(temp_sock("invalid"));
        let r = live.run(Command::Batch { steps: vec![] }).await;
        assert!(matches!(
            r,
            Reply::Error {
                code: ErrorCode::BadArgs,
                ..
            }
        ));
    }

    #[test]
    fn timeouts_follow_the_command() {
        let s = SLACK;
        assert_eq!(reply_timeout(&Command::Status, s), DEFAULT_TIMEOUT);
        let w = Command::Wait {
            change: true,
            stable_ms: None,
            timeout_ms: 5000,
        };
        assert_eq!(reply_timeout(&w, s), Duration::from_secs(15));
        let b = Command::Batch {
            steps: vec![w.clone(), Command::Key { combo: "a".into() }],
        };
        assert_eq!(reply_timeout(&b, s), Duration::from_millis(5000 + 200) + s);
    }

    #[test]
    fn long_text_gets_time_for_its_paced_chunks() {
        let s = SLACK;
        let text = |n: usize| Command::TypeText {
            text: "x".repeat(n),
            secret: false,
        };
        // Short text: the plain deadline.
        assert_eq!(reply_timeout(&text(10), s), DEFAULT_TIMEOUT);
        // 32 characters per chunk, 100 ms between chunks.
        assert_eq!(
            reply_timeout(&text(32_000), s),
            DEFAULT_TIMEOUT + Duration::from_secs(100)
        );
        // In a batch the text's time adds to the steps' budget.
        let b = Command::Batch {
            steps: vec![text(3200), Command::Key { combo: "a".into() }],
        };
        assert_eq!(
            reply_timeout(&b, s),
            Duration::from_secs(10) + STEP_BUDGET * 2 + s
        );
    }
}
