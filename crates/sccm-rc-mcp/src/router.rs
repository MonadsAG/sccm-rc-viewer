//! `--mode both`: one server that offers live and headless, one session at a
//! time. `connect` takes an optional mode; without one it uses live when the
//! viewer allows control by Claude and is connected to that till, otherwise
//! headless. Every other command goes to the backend of the last connect.

use async_trait::async_trait;
use sccm_rc_core::agent::exec::{bad_args, err};
use sccm_rc_core::agent::proto::{Command, ErrorCode, Reply};

use crate::backend::{Backend, ConnectMode};

const HINT_EXISTING_SESSION: &str = "if your viewer shows this till, switch on 'Allow Control by Claude' there or connect with mode live";

pub struct Router {
    headless: Box<dyn Backend + Send>,
    live: Box<dyn Backend + Send>,
    active: Option<ConnectMode>,
}

/// What the viewer reports, for the automatic choice.
enum Viewer {
    /// Control by Claude is off (no control socket) or the viewer is not running.
    Off,
    NotConnected,
    ConnectedTo(String),
}

impl Viewer {
    fn describe(&self) -> String {
        match self {
            Viewer::Off => "viewer control is off".into(),
            Viewer::NotConnected => "viewer control is on, the viewer is not connected".into(),
            Viewer::ConnectedTo(t) => format!("viewer control is on, connected to {t}"),
        }
    }
}

impl Router {
    pub fn new(headless: Box<dyn Backend + Send>, live: Box<dyn Backend + Send>) -> Self {
        Router {
            headless,
            live,
            active: None,
        }
    }

    async fn viewer(&mut self) -> Viewer {
        match self.live.run(Command::Status).await {
            Reply::Status {
                connected: true,
                target: Some(t),
                ..
            } => Viewer::ConnectedTo(t),
            Reply::Status { .. } => Viewer::NotConnected,
            _ => Viewer::Off,
        }
    }

    /// The target of a still running headless session.
    async fn headless_target(&mut self) -> Option<String> {
        if self.active != Some(ConnectMode::Headless) {
            return None;
        }
        match self.headless.run(Command::Status).await {
            Reply::Status {
                connected: true,
                target,
                ..
            } => Some(target.unwrap_or_default()),
            _ => None,
        }
    }

    /// A failed connect leaves the active mode as it was.
    async fn connect_live(&mut self, target: &str, force: bool, note: Option<String>) -> Reply {
        let reply = self.live.connect(target, force).await;
        if !live_connected(&reply) {
            return reply;
        }
        self.active = Some(ConnectMode::Live);
        let mut extra = Vec::new();
        if let Some(n) = note {
            extra.push(n);
        }
        if force {
            extra.push("force is ignored in live mode".to_string());
        }
        if extra.is_empty() {
            return reply;
        }
        match reply {
            Reply::Status {
                connected,
                target,
                desk_w,
                desk_h,
                mode,
                idle_s,
            } => Reply::Status {
                connected,
                target,
                desk_w,
                desk_h,
                mode: format!("{mode} ({})", extra.join("; ")),
                idle_s,
            },
            other => other,
        }
    }

    async fn connect_headless(&mut self, target: &str, force: bool, note: Option<String>) -> Reply {
        let reply = self.headless.connect(target, force).await;
        match reply {
            Reply::Ok { mut text } => {
                self.active = Some(ConnectMode::Headless);
                text.push_str("; mode headless");
                if let Some(n) = note {
                    text.push_str(&format!(" ({n})"));
                }
                Reply::Ok { text }
            }
            Reply::Error {
                code: ErrorCode::ExistingSession,
                message,
                completed_steps,
            } => Reply::Error {
                code: ErrorCode::ExistingSession,
                message: format!("{message}; {HINT_EXISTING_SESSION}"),
                completed_steps,
            },
            other => other,
        }
    }
}

#[async_trait]
impl Backend for Router {
    async fn connect(&mut self, target: &str, force: bool) -> Reply {
        self.connect_as(target, force, None).await
    }

    async fn connect_as(&mut self, target: &str, force: bool, mode: Option<ConnectMode>) -> Reply {
        if let Some(t) = self.headless_target().await {
            return bad_args(format!(
                "already connected to {t} in headless mode; disconnect first"
            ));
        }
        match mode {
            Some(ConnectMode::Live) => self.connect_live(target, force, None).await,
            Some(ConnectMode::Headless) => self.connect_headless(target, force, None).await,
            None => {
                let viewer = self.viewer().await;
                match &viewer {
                    Viewer::ConnectedTo(t) if same_host(t, target) => {
                        let note = "auto: the viewer is connected to this till".to_string();
                        self.connect_live(target, force, Some(note)).await
                    }
                    _ => {
                        let note = format!("auto: {}", viewer.describe());
                        self.connect_headless(target, force, Some(note)).await
                    }
                }
            }
        }
    }

    async fn run(&mut self, cmd: Command) -> Reply {
        let disconnect = matches!(cmd, Command::Disconnect);
        let reply = match self.active {
            Some(ConnectMode::Live) => self.live.run(cmd).await,
            Some(ConnectMode::Headless) => self.headless.run(cmd).await,
            None => match cmd {
                Command::Status => {
                    let viewer = self.viewer().await;
                    Reply::Status {
                        connected: false,
                        target: None,
                        desk_w: 0,
                        desk_h: 0,
                        mode: format!("both ({})", viewer.describe()),
                        idle_s: 0,
                    }
                }
                Command::Disconnect => Reply::Ok {
                    text: "not connected".into(),
                },
                _ => err(ErrorCode::NotConnected, "not connected; call connect first"),
            },
        };
        if disconnect {
            self.active = None;
        }
        reply
    }

    async fn shutdown(&mut self) {
        self.headless.shutdown().await;
        self.live.shutdown().await;
    }
}

/// Whether a live connect worked: the viewer answers `status` with
/// `connected: false` when it has no session, and with an error when its
/// control is off.
fn live_connected(reply: &Reply) -> bool {
    matches!(
        reply,
        Reply::Status {
            connected: true,
            ..
        } | Reply::Ok { .. }
    )
}

/// Same till, as far as the names tell: case and a trailing dot ignored, and
/// a short name matches the first label of an FQDN. No DNS lookup, so an IP
/// and a name never match.
pub fn same_host(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.trim().trim_end_matches('.').to_ascii_lowercase()
    }
    let (a, b) = (norm(a), norm(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let is_ip = |s: &str| s.parse::<std::net::IpAddr>().is_ok();
    if is_ip(&a) || is_ip(&b) {
        return false;
    }
    let (a_short, b_short) = (
        a.split('.').next().unwrap_or(""),
        b.split('.').next().unwrap_or(""),
    );
    // Only a short name against an FQDN; two different FQDNs stay different.
    (a.contains('.') != b.contains('.')) && a_short == b_short
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;

    /// Records calls; answers `status` with `status`, `connect` with `connect`.
    struct Fake {
        name: &'static str,
        log: Log,
        status: Arc<Mutex<Reply>>,
        connect: Arc<Mutex<Reply>>,
    }

    #[async_trait]
    impl Backend for Fake {
        async fn connect(&mut self, target: &str, force: bool) -> Reply {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} connect {target} {force}", self.name));
            self.connect.lock().unwrap().clone()
        }
        async fn run(&mut self, cmd: Command) -> Reply {
            let name = match cmd {
                Command::Status => "status",
                Command::Disconnect => "disconnect",
                _ => "input",
            };
            self.log
                .lock()
                .unwrap()
                .push(format!("{} {name}", self.name));
            match cmd {
                Command::Status => self.status.lock().unwrap().clone(),
                _ => Reply::Ok {
                    text: format!("{} {name}", self.name),
                },
            }
        }
        async fn shutdown(&mut self) {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} shutdown", self.name));
        }
    }

    fn status(connected: bool, target: Option<&str>, mode: &str) -> Reply {
        Reply::Status {
            connected,
            target: target.map(str::to_string),
            desk_w: 1024,
            desk_h: 768,
            mode: mode.into(),
            idle_s: 0,
        }
    }

    fn off() -> Reply {
        err(ErrorCode::ControlDisabled, "enable control")
    }

    struct Setup {
        router: Router,
        log: Log,
        headless_status: Arc<Mutex<Reply>>,
        live_connect: Arc<Mutex<Reply>>,
    }

    fn setup(viewer: Reply, headless_connect: Reply) -> Setup {
        let log: Log = Arc::default();
        let headless_status = Arc::new(Mutex::new(status(false, None, "headless")));
        let headless = Fake {
            name: "headless",
            log: log.clone(),
            status: headless_status.clone(),
            connect: Arc::new(Mutex::new(headless_connect)),
        };
        let live_connect = Arc::new(Mutex::new(viewer.clone()));
        let live = Fake {
            name: "live",
            log: log.clone(),
            status: Arc::new(Mutex::new(viewer)),
            connect: live_connect.clone(),
        };
        Setup {
            router: Router::new(Box::new(headless), Box::new(live)),
            log,
            headless_status,
            live_connect,
        }
    }

    fn connected_ok() -> Reply {
        Reply::Ok {
            text: "connected to till-1".into(),
        }
    }

    fn calls(log: &Log) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn auto_uses_live_when_viewer_is_on_that_till() {
        let mut s = setup(status(true, Some("TILL-1"), "live"), connected_ok());
        match s.router.connect_as("till-1", false, None).await {
            Reply::Status { mode, .. } => assert!(mode.contains("auto: the viewer"), "{mode}"),
            other => panic!("{other:?}"),
        }
        s.router.run(Command::Status).await;
        assert_eq!(
            calls(&s.log),
            ["live status", "live connect till-1 false", "live status"]
        );
    }

    #[tokio::test]
    async fn auto_uses_headless_when_viewer_control_is_off() {
        let mut s = setup(off(), connected_ok());
        match s.router.connect_as("till-1", false, None).await {
            Reply::Ok { text } => {
                assert!(
                    text.contains("mode headless (auto: viewer control is off)"),
                    "{text}"
                )
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            calls(&s.log),
            ["live status", "headless connect till-1 false"]
        );
    }

    #[tokio::test]
    async fn auto_uses_headless_when_viewer_is_on_another_till() {
        let mut s = setup(status(true, Some("till-9"), "live"), connected_ok());
        match s.router.connect_as("till-1", false, None).await {
            Reply::Ok { text } => assert!(text.contains("connected to till-9"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_uses_headless_when_viewer_is_not_connected() {
        let mut s = setup(status(false, None, "live"), connected_ok());
        match s.router.connect_as("till-1", false, None).await {
            Reply::Ok { text } => assert!(text.contains("not connected"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn explicit_mode_never_falls_back() {
        let mut s = setup(off(), connected_ok());
        let r = s
            .router
            .connect_as("till-1", false, Some(ConnectMode::Live))
            .await;
        assert_eq!(r, off());
        assert_eq!(calls(&s.log), ["live connect till-1 false"]);

        let mut s = setup(status(true, Some("till-1"), "live"), connected_ok());
        let r = s
            .router
            .connect_as("till-1", true, Some(ConnectMode::Headless))
            .await;
        assert!(matches!(r, Reply::Ok { .. }));
        assert_eq!(calls(&s.log), ["headless connect till-1 true"]);
    }

    #[tokio::test]
    async fn force_is_reported_as_ignored_in_live() {
        let mut s = setup(status(true, Some("till-1"), "live"), connected_ok());
        match s.router.connect_as("till-1", true, None).await {
            Reply::Status { mode, .. } => assert!(mode.contains("force is ignored"), "{mode}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn existing_session_gets_a_hint() {
        let mut s = setup(
            off(),
            err(ErrorCode::ExistingSession, "another session is active"),
        );
        match s.router.connect_as("till-1", false, None).await {
            Reply::Error { code, message, .. } => {
                assert_eq!(code, ErrorCode::ExistingSession);
                assert!(message.contains("Allow Control by Claude"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        // A failed connect leaves no active backend.
        assert_eq!(
            s.router.run(Command::Disconnect).await,
            Reply::Ok {
                text: "not connected".into()
            }
        );
    }

    #[tokio::test]
    async fn commands_follow_the_active_backend_until_disconnect() {
        let mut s = setup(off(), connected_ok());
        s.router.connect_as("till-1", false, None).await;
        *s.headless_status.lock().unwrap() = status(true, Some("till-1"), "headless");
        s.router
            .run(Command::Key {
                combo: "Enter".into(),
                secret: false,
            })
            .await;
        s.router.run(Command::Disconnect).await;
        *s.headless_status.lock().unwrap() = status(false, None, "headless");
        let r = s
            .router
            .run(Command::Key {
                combo: "Enter".into(),
                secret: false,
            })
            .await;
        assert!(matches!(
            r,
            Reply::Error {
                code: ErrorCode::NotConnected,
                ..
            }
        ));
        assert_eq!(
            calls(&s.log),
            [
                "live status",
                "headless connect till-1 false",
                "headless input",
                "headless disconnect"
            ]
        );
    }

    #[tokio::test]
    async fn a_running_headless_session_blocks_a_new_connect() {
        let mut s = setup(status(true, Some("till-2"), "live"), connected_ok());
        s.router
            .connect_as("till-1", false, Some(ConnectMode::Headless))
            .await;
        *s.headless_status.lock().unwrap() = status(true, Some("till-1"), "headless");
        match s
            .router
            .connect_as("till-2", false, Some(ConnectMode::Live))
            .await
        {
            Reply::Error { code, message, .. } => {
                assert_eq!(code, ErrorCode::BadArgs);
                assert!(message.contains("already connected to till-1"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        // Once that session has ended (e.g. idle timeout), connect works again.
        *s.headless_status.lock().unwrap() = status(false, None, "headless");
        assert!(matches!(
            s.router
                .connect_as("till-2", false, Some(ConnectMode::Live))
                .await,
            Reply::Status { .. }
        ));
    }

    #[tokio::test]
    async fn status_without_session_describes_the_viewer() {
        let mut s = setup(off(), connected_ok());
        match s.router.run(Command::Status).await {
            Reply::Status {
                connected, mode, ..
            } => {
                assert!(!connected);
                assert_eq!(mode, "both (viewer control is off)");
            }
            other => panic!("{other:?}"),
        }
    }

    fn key() -> Command {
        Command::Key {
            combo: "Enter".into(),
            secret: false,
        }
    }

    fn is_not_connected(r: &Reply) -> bool {
        matches!(
            r,
            Reply::Error {
                code: ErrorCode::NotConnected,
                ..
            }
        )
    }

    #[tokio::test]
    async fn a_failed_live_connect_leaves_no_active_backend() {
        let mut s = setup(off(), connected_ok());
        let r = s
            .router
            .connect_as("till-1", false, Some(ConnectMode::Live))
            .await;
        assert_eq!(r, off());
        let r = s
            .router
            .run(Command::Screenshot {
                region: None,
                max_width: None,
            })
            .await;
        assert!(is_not_connected(&r), "{r:?}");
        match s.router.run(Command::Status).await {
            Reply::Status {
                connected, mode, ..
            } => {
                assert!(!connected);
                assert_eq!(mode, "both (viewer control is off)");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(calls(&s.log), ["live connect till-1 false", "live status"]);
    }

    #[tokio::test]
    async fn a_live_connect_to_a_viewer_without_session_fails() {
        let mut s = setup(status(false, None, "live"), connected_ok());
        match s
            .router
            .connect_as("till-1", false, Some(ConnectMode::Live))
            .await
        {
            Reply::Status { connected, .. } => assert!(!connected),
            other => panic!("{other:?}"),
        }
        let r = s.router.run(key()).await;
        assert!(is_not_connected(&r), "{r:?}");
    }

    #[tokio::test]
    async fn headless_connect_works_after_a_failed_live_connect() {
        let mut s = setup(off(), connected_ok());
        s.router
            .connect_as("till-1", false, Some(ConnectMode::Live))
            .await;
        let r = s
            .router
            .connect_as("till-1", false, Some(ConnectMode::Headless))
            .await;
        assert!(matches!(r, Reply::Ok { .. }), "{r:?}");
        s.router.run(key()).await;
        assert_eq!(
            calls(&s.log),
            [
                "live connect till-1 false",
                "headless connect till-1 false",
                "headless input"
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_live_connect_keeps_the_running_live_session() {
        let mut s = setup(status(true, Some("till-1"), "live"), connected_ok());
        s.router
            .connect_as("till-1", false, Some(ConnectMode::Live))
            .await;
        *s.live_connect.lock().unwrap() = off();
        let r = s
            .router
            .connect_as("till-2", false, Some(ConnectMode::Live))
            .await;
        assert_eq!(r, off());
        s.router.run(key()).await;
        assert_eq!(
            calls(&s.log),
            [
                "live connect till-1 false",
                "live connect till-2 false",
                "live input"
            ]
        );
    }

    #[tokio::test]
    async fn a_successful_live_connect_activates_live() {
        let mut s = setup(status(true, Some("till-1"), "live"), connected_ok());
        match s
            .router
            .connect_as("till-1", true, Some(ConnectMode::Live))
            .await
        {
            Reply::Status {
                connected, mode, ..
            } => {
                assert!(connected);
                assert_eq!(mode, "live (force is ignored in live mode)");
            }
            other => panic!("{other:?}"),
        }
        s.router.run(key()).await;
        assert_eq!(calls(&s.log), ["live connect till-1 true", "live input"]);
    }

    #[tokio::test]
    async fn shutdown_reaches_both_backends() {
        let mut s = setup(off(), connected_ok());
        s.router.shutdown().await;
        assert_eq!(calls(&s.log), ["headless shutdown", "live shutdown"]);
    }

    #[test]
    fn same_host_cases() {
        assert!(same_host("TILL-1", "till-1"));
        assert!(same_host("till-1.example.ch.", "till-1.example.ch"));
        assert!(same_host("till-1", "till-1.example.ch"));
        assert!(same_host(" till-1.example.ch", "TILL-1"));
        assert!(same_host("10.0.0.5", "10.0.0.5"));
        assert!(!same_host("till-1", "till-2"));
        assert!(!same_host("till-1.a.ch", "till-1.b.ch"));
        assert!(!same_host("10.0.0.5", "till-1"));
        assert!(!same_host("10.0.0.5", "10.0.0"));
        assert!(!same_host("", ""));
    }
}
