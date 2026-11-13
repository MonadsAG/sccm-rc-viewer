//! Headless backend: the MCP server runs its own windowless SCCM RC session.
//!
//! The session (connect, RDP, active loop, graceful disconnect) runs on its
//! own thread with a current-thread runtime, as in the viewer. A sink copies
//! every graphics update into a shared RGBA frame and feeds a `FrameWatch`;
//! input goes through the session's input channel. Dropping the input sender
//! ends the active session, after which the thread disconnects so the till is
//! released.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use sccm_rc_core::agent::proto::{Button, Command, ErrorCode, Reply};
use sccm_rc_core::agent::shot::{self, ShotError, ShotSpace};
use sccm_rc_core::agent::wait::{FrameWatch, WaitResult};
use sccm_rc_core::agent::{keys, text};
use sccm_rc_core::rdp::{
    self, FastPathInputEvent, FrameView, InputReceiver, InputSender, MousePdu, PointerFlags,
    RdpOptions, SessionSink, UpdateRegion,
};
use sccm_rc_core::{audit, Error as CoreError, LogonCredentials, SccmSession};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::backend::Backend;
use crate::creds;

/// No tool call for this long → graceful disconnect.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long to wait for the session thread to confirm the disconnect.
const RELEASE_WAIT: Duration = Duration::from_secs(3);
/// How long `connect` waits for the first desktop image.
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(10);
/// Bound on SCCM logon + RDP negotiation.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(45);
const POLL: Duration = Duration::from_millis(50);
const STEP_GAP: Duration = Duration::from_millis(100);
const DEFAULT_MAX_WIDTH: u32 = 1280;
/// Longest single `wait`: the MCP loop is serial, so a wait blocks every tool.
const MAX_WAIT_MS: u64 = 5 * 60 * 1000;
const MAX_SCROLL: i16 = 50;
/// One wheel notch in RDP rotation units.
const WHEEL_NOTCH: i16 = 120;
/// Events per input batch for typed text.
const TEXT_CHUNK: usize = 64;
/// Requested desktop size; the shadowed till reports its own.
const DESK_W: u16 = 1920;
const DESK_H: u16 = 1080;

const MSG_EXISTING_SESSION: &str = "another remote-control session is active, e.g. your viewer or a session that was not released; try again later";
const MSG_IN_USE: &str = "the till is in use by a logged-on user; use force to take it over";

/// Latest desktop image (RGBA8). `w == 0` until the first update.
#[derive(Default)]
pub struct Frame {
    pub rgba: Vec<u8>,
    pub w: u16,
    pub h: u16,
}

/// State shared between the session thread and the backend.
#[derive(Default)]
pub struct Shared {
    pub frame: Mutex<Frame>,
    pub watch: Mutex<FrameWatch>,
    /// Why the session ended; set once.
    pub ended: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn end(&self, reason: impl Into<String>) {
        let mut e = lock(&self.ended);
        if e.is_none() {
            *e = Some(reason.into());
        }
    }

    fn ended(&self) -> Option<String> {
        lock(&self.ended).clone()
    }

    fn desk_size(&self) -> (u16, u16) {
        let f = lock(&self.frame);
        (f.w, f.h)
    }
}

/// Copies graphics updates into `Shared`.
struct Sink {
    shared: Arc<Shared>,
}

impl SessionSink for Sink {
    fn on_graphics_update(&mut self, frame: &dyn FrameView, r: UpdateRegion) {
        let (w, h, data) = (frame.width(), frame.height(), frame.data());
        {
            let mut f = lock(&self.shared.frame);
            if f.w != w || f.h != h || f.rgba.len() != data.len() {
                f.rgba = data.to_vec();
                f.w = w;
                f.h = h;
            } else if w > 0 && h > 0 {
                let stride = w as usize * 4;
                let right = (r.right as usize).min(w as usize - 1);
                let bottom = (r.bottom as usize).min(h as usize - 1);
                let left = (r.left as usize).min(right);
                for row in r.top as usize..=bottom {
                    let a = row * stride + left * 4;
                    let b = row * stride + (right + 1) * 4;
                    if b <= data.len() {
                        f.rgba[a..b].copy_from_slice(&data[a..b]);
                    }
                }
            }
        }
        lock(&self.shared.watch).on_update(Instant::now());
    }

    fn on_terminate(&mut self, reason: String) {
        info!(%reason, "session terminated by the till");
        self.shared
            .end(format!("the till ended the session: {reason}"));
    }
}

/// A running session as the backend sees it.
pub struct Session {
    input: Arc<Mutex<Option<InputSender>>>,
    shared: Arc<Shared>,
    done: Option<oneshot::Receiver<()>>,
    view_only: bool,
}

impl Session {
    pub fn new(
        input: InputSender,
        shared: Arc<Shared>,
        done: Option<oneshot::Receiver<()>>,
        view_only: bool,
    ) -> Self {
        Self {
            input: Arc::new(Mutex::new(Some(input))),
            shared,
            done,
            view_only,
        }
    }

    /// Ask the session to end: closing the input channel ends the active
    /// session, then the session thread disconnects.
    fn stop(&self) {
        lock(&self.input).take();
    }

    fn stopped(&self) -> bool {
        lock(&self.input).is_none()
    }

    /// Stop and wait up to 3 s for the till to be released. `false` if the
    /// release was not confirmed in time.
    async fn stop_and_wait(&mut self) -> bool {
        self.stop();
        match self.done.take() {
            Some(d) => tokio::time::timeout(RELEASE_WAIT, d).await.is_ok(),
            None => true,
        }
    }

    fn ended(&self) -> Option<String> {
        self.shared.ended().or_else(|| {
            self.stopped()
                .then(|| "the session was stopped".to_string())
        })
    }

    fn has_frame(&self) -> bool {
        self.shared.desk_size().0 > 0
    }

    async fn send(&self, events: Vec<FastPathInputEvent>) -> Result<(), Reply> {
        if events.is_empty() {
            return Ok(());
        }
        let tx = lock(&self.input).clone();
        let Some(tx) = tx else {
            return Err(session_ended());
        };
        tx.send(events).await.map_err(|_| session_ended())
    }
}

pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<Session, Reply>> + Send>>;
type Connector = Box<dyn FnMut(String, bool) -> ConnectFuture + Send>;
type Audit = Arc<dyn Fn(&str, &str) + Send + Sync>;

struct Conn {
    target: String,
    session: Session,
    shot: Option<ShotSpace>,
    watchdog: tokio::task::JoinHandle<()>,
}

pub struct Headless {
    connector: Connector,
    audit: Audit,
    idle: Duration,
    last_call: Arc<Mutex<Instant>>,
    conn: Option<Conn>,
}

impl Headless {
    /// The real backend: credentials from env / last user / Keychain.
    pub fn new() -> Self {
        // An explicit logon the till rejected is never retried in this
        // process: every retry is another failed logon (lockout).
        let rejected: Arc<Mutex<Option<String>>> = Arc::default();
        Self::with_connector(move |target, force| {
            let rejected = rejected.clone();
            Box::pin(async move { real_connect(target, force, rejected).await })
        })
    }

    pub fn with_connector(
        connector: impl FnMut(String, bool) -> ConnectFuture + Send + 'static,
    ) -> Self {
        Self {
            connector: Box::new(connector),
            audit: Arc::new(|t: &str, a: &str| audit::log_agent(t, a)),
            idle: IDLE_TIMEOUT,
            last_call: Arc::new(Mutex::new(Instant::now())),
            conn: None,
        }
    }

    #[cfg(test)]
    fn with_audit(mut self, f: impl Fn(&str, &str) + Send + Sync + 'static) -> Self {
        self.audit = Arc::new(f);
        self
    }

    #[cfg(test)]
    fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    fn touch(&self) {
        *lock(&self.last_call) = Instant::now();
    }

    fn idle_s(&self) -> u64 {
        lock(&self.last_call).elapsed().as_secs()
    }

    fn audit(&self, action: &str) {
        if let Some(c) = &self.conn {
            (self.audit)(&c.target, action);
        }
    }

    /// Drop a session that ended on its own; returns why.
    async fn reap(&mut self) -> Option<String> {
        let reason = self.conn.as_ref()?.session.ended()?;
        let mut c = self.conn.take()?;
        c.watchdog.abort();
        c.session.stop_and_wait().await;
        Some(reason)
    }

    async fn close(&mut self, why: &str) -> Option<(String, bool)> {
        let mut c = self.conn.take()?;
        c.watchdog.abort();
        (self.audit)(&c.target, why);
        let released = c.session.stop_and_wait().await;
        if !released {
            warn!(target = %c.target, "till release not confirmed within 3 s");
        }
        Some((c.target, released))
    }

    fn spawn_watchdog(&self, target: String, session: &Session) -> tokio::task::JoinHandle<()> {
        let (idle, last) = (self.idle, self.last_call.clone());
        let (input, shared, audit) = (
            session.input.clone(),
            session.shared.clone(),
            self.audit.clone(),
        );
        tokio::spawn(async move {
            loop {
                let since = lock(&last).elapsed();
                if since >= idle {
                    if lock(&input).take().is_some() {
                        let mins = idle.as_secs() / 60;
                        warn!(%target, "idle timeout: disconnecting");
                        shared.end(format!("disconnected after {mins} min without a tool call"));
                        audit(&target, "disconnect (idle timeout)");
                    }
                    return;
                }
                tokio::time::sleep(idle - since).await;
            }
        })
    }

    fn desk_size(&self) -> (u16, u16) {
        self.conn
            .as_ref()
            .map(|c| c.session.shared.desk_size())
            .unwrap_or((0, 0))
    }

    /// Map a point from the tool's coordinate space to desktop pixels.
    fn map_point(&self, x: f64, y: f64, relative: bool) -> Result<(u16, u16), Reply> {
        let (w, h) = self.desk_size();
        if w == 0 || h == 0 {
            return Err(no_image());
        }
        let shot = self.conn.as_ref().and_then(|c| c.shot);
        let r = if relative {
            shot::relative_to_desktop(x, y, w, h)
        } else if let Some(s) = shot {
            s.to_desktop_checked(x, y, w, h)
        } else {
            // No screenshot yet: coordinates are desktop pixels.
            if !(x.is_finite() && y.is_finite())
                || x < 0.0
                || y < 0.0
                || x > (w - 1) as f64
                || y > (h - 1) as f64
            {
                Err(ShotError::OutOfBounds)
            } else {
                Ok((x.round() as u16, y.round() as u16))
            }
        };
        r.map_err(|e| match e {
            ShotError::StaleShot => bad_args(format!("{e}; take a new screenshot")),
            e => bad_args(e.to_string()),
        })
    }

    fn session(&self) -> Result<&Session, Reply> {
        self.conn
            .as_ref()
            .map(|c| &c.session)
            .ok_or_else(not_connected)
    }

    async fn step(&mut self, cmd: Command, idle_s: u64) -> Reply {
        match self.step_inner(cmd, idle_s).await {
            Ok(r) | Err(r) => r,
        }
    }

    async fn step_inner(&mut self, cmd: Command, idle_s: u64) -> Result<Reply, Reply> {
        match cmd {
            Command::Status => Ok(match &self.conn {
                Some(c) => {
                    let (w, h) = c.session.shared.desk_size();
                    Reply::Status {
                        connected: true,
                        target: Some(c.target.clone()),
                        desk_w: w,
                        desk_h: h,
                        mode: "headless".into(),
                        idle_s,
                    }
                }
                None => Reply::Status {
                    connected: false,
                    target: None,
                    desk_w: 0,
                    desk_h: 0,
                    mode: "headless".into(),
                    idle_s,
                },
            }),
            Command::Disconnect => Ok(ok(match self.close("disconnect").await {
                Some((t, true)) => format!("disconnected from {t}"),
                Some((t, false)) => format!(
                    "disconnected from {t} (release not confirmed within 3 s; the till may report an existing session for a while)"
                ),
                None => "not connected".into(),
            })),
            Command::Screenshot { region, max_width } => {
                self.session()?;
                let (rgba, w, h) = {
                    let shared = self.session()?.shared.clone();
                    let f = lock(&shared.frame);
                    if f.w == 0 {
                        return Err(no_image());
                    }
                    (f.rgba.clone(), f.w, f.h)
                };
                let mw = max_width.unwrap_or(DEFAULT_MAX_WIDTH);
                self.audit(&match region {
                    Some(r) => format!("screenshot region {},{} {}x{}", r.x, r.y, r.w, r.h),
                    None => "screenshot".into(),
                });
                let (png, space) =
                    tokio::task::spawn_blocking(move || shot::take(&rgba, w, h, region, mw))
                        .await
                        .map_err(|e| session_err(format!("screenshot failed: {e}")))?
                        .map_err(|e| match e {
                            ShotError::BadRegion => bad_args(e.to_string()),
                            e => session_err(format!("screenshot failed: {e}")),
                        })?;
                if let Some(c) = self.conn.as_mut() {
                    c.shot = Some(space);
                }
                Ok(Reply::Image {
                    png_base64: base64::engine::general_purpose::STANDARD.encode(png),
                    desk_w: w,
                    desk_h: h,
                    scale: space.scale(),
                })
            }
            Command::Click {
                x,
                y,
                relative,
                button,
                double,
            } => {
                self.session()?;
                let (dx, dy) = self.map_point(x, y, relative)?;
                let b = match button {
                    Button::Left => PointerFlags::LEFT_BUTTON,
                    Button::Right => PointerFlags::RIGHT_BUTTON,
                    Button::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
                };
                let name = format!("{button:?}").to_lowercase();
                let kind = if double { "double-click" } else { "click" };
                self.audit(&format!("{kind} {name} at {dx},{dy}"));
                let s = self.session()?;
                s.send(vec![mouse(PointerFlags::MOVE, dx, dy)]).await?;
                for i in 0..if double { 2 } else { 1 } {
                    if i > 0 {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    s.send(vec![mouse(b | PointerFlags::DOWN, dx, dy)]).await?;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    s.send(vec![mouse(b, dx, dy)]).await?;
                }
                Ok(ok(format!("{kind} at desktop {dx},{dy}")))
            }
            Command::Move { x, y, relative } => {
                self.session()?;
                let (dx, dy) = self.map_point(x, y, relative)?;
                self.audit(&format!("move to {dx},{dy}"));
                self.session()?
                    .send(vec![mouse(PointerFlags::MOVE, dx, dy)])
                    .await?;
                Ok(ok(format!("moved to desktop {dx},{dy}")))
            }
            Command::Scroll {
                x,
                y,
                amount,
                relative,
            } => {
                self.session()?;
                if amount == 0 || amount.abs() > MAX_SCROLL {
                    return Err(bad_args(format!(
                        "amount must be between -{MAX_SCROLL} and {MAX_SCROLL} and not 0"
                    )));
                }
                let (dx, dy) = self.map_point(x, y, relative)?;
                self.audit(&format!("scroll {amount} at {dx},{dy}"));
                let units = if amount > 0 { WHEEL_NOTCH } else { -WHEEL_NOTCH };
                let mut ev = vec![mouse(PointerFlags::MOVE, dx, dy)];
                ev.extend((0..amount.unsigned_abs()).map(|_| {
                    FastPathInputEvent::MouseEvent(MousePdu {
                        flags: PointerFlags::VERTICAL_WHEEL,
                        number_of_wheel_rotation_units: units,
                        x_position: dx,
                        y_position: dy,
                    })
                }));
                self.session()?.send(ev).await?;
                Ok(ok(format!("scrolled {amount} at desktop {dx},{dy}")))
            }
            Command::TypeText { text, secret } => {
                self.session()?;
                let n = text.chars().count();
                let (events, skipped) = text::encode_text(&text);
                self.audit(&if secret {
                    format!("type_text (secret, {n} chars)")
                } else {
                    format!("type_text {text:?}")
                });
                drop(text);
                let s = self.session()?;
                for chunk in events.chunks(TEXT_CHUNK) {
                    s.send(chunk.to_vec()).await?;
                }
                let sent = n - skipped.len();
                let mut t = format!("typed {sent} characters");
                if !skipped.is_empty() {
                    if secret {
                        t.push_str(&format!(
                            "; skipped {} that cannot be sent",
                            skipped.len()
                        ));
                    } else {
                        t.push_str(&format!(
                            "; skipped {} that cannot be sent: {skipped:?}",
                            skipped.len()
                        ));
                    }
                }
                Ok(ok(t))
            }
            Command::Key { combo } => {
                self.session()?;
                let k = keys::parse_combo(&combo).map_err(|e| bad_args(e.to_string()))?;
                self.audit(&format!("key {combo}"));
                self.session()?.send(keys::press_combo(&k)).await?;
                Ok(ok(format!("pressed {combo}")))
            }
            Command::Wait {
                change,
                stable_ms,
                timeout_ms,
            } => {
                let shared = self.session()?.shared.clone();
                if timeout_ms > MAX_WAIT_MS {
                    return Err(bad_args(format!(
                        "timeout_ms must be at most {MAX_WAIT_MS}"
                    )));
                }
                let mark = lock(&shared.watch).mark();
                let started = Instant::now();
                let timeout = Duration::from_millis(timeout_ms);
                loop {
                    let r = lock(&shared.watch).evaluate(
                        mark,
                        change,
                        stable_ms,
                        started,
                        timeout,
                        Instant::now(),
                    );
                    if let Some(r) = r {
                        return Ok(ok(match r {
                            WaitResult::Changed => "changed",
                            WaitResult::Stable => "stable",
                            WaitResult::Timeout => "timeout",
                        }
                        .into()));
                    }
                    if self.session()?.ended().is_some() {
                        return Err(session_ended());
                    }
                    self.touch();
                    tokio::time::sleep(POLL).await;
                }
            }
            Command::Batch { .. } => Err(bad_args("batch steps cannot contain another batch")),
        }
    }
}

impl Default for Headless {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Backend for Headless {
    async fn connect(&mut self, target: &str, force: bool) -> Reply {
        self.touch();
        self.reap().await;
        if let Some(c) = &self.conn {
            return bad_args(format!("already connected to {}", c.target));
        }
        let target = target.trim().to_string();
        if target.is_empty() {
            return bad_args("connect needs a non-empty target");
        }
        (self.audit)(
            &target,
            if force {
                "connect (force: take over a logged-on user)"
            } else {
                "connect"
            },
        );
        let session = match (self.connector)(target.clone(), force).await {
            Ok(s) => s,
            Err(reply) => {
                if let Reply::Error { message, .. } = &reply {
                    (self.audit)(&target, &format!("connect failed: {message}"));
                }
                return reply;
            }
        };
        self.touch();
        let mut text = format!("connected to {target}");
        if force {
            text.push_str(" with force (a logged-on user's session, if any, was taken over)");
        }
        if session.view_only {
            text.push_str("; view only: the till grants no control, input will be ignored");
        }
        if !session.has_frame() {
            text.push_str("; no desktop image yet, use wait before screenshot");
        }
        let watchdog = self.spawn_watchdog(target.clone(), &session);
        self.conn = Some(Conn {
            target,
            session,
            shot: None,
            watchdog,
        });
        ok(text)
    }

    async fn run(&mut self, cmd: Command) -> Reply {
        if let Err(e) = cmd.validate() {
            return bad_args(e);
        }
        let idle_s = self.idle_s();
        self.touch();
        let reaped = self.reap().await;
        let r = match cmd {
            Command::Status | Command::Disconnect => self.step(cmd, idle_s).await,
            _ if self.conn.is_none() => match reaped {
                Some(why) => Reply::Error {
                    code: ErrorCode::NotConnected,
                    message: format!("session ended ({why}); connect again"),
                    completed_steps: None,
                },
                None => not_connected(),
            },
            Command::Batch { steps } => {
                let n = steps.len();
                let mut notes = Vec::new();
                for (i, s) in steps.into_iter().enumerate() {
                    if i > 0 {
                        tokio::time::sleep(STEP_GAP).await;
                    }
                    self.touch();
                    match self.step(s, idle_s).await {
                        Reply::Error { code, message, .. } => {
                            self.touch();
                            return Reply::Error {
                                code,
                                message: format!("step {i}: {message}"),
                                completed_steps: Some(i),
                            };
                        }
                        Reply::Ok { text } => notes.push(text),
                        _ => {}
                    }
                }
                ok(format!("batch done: {n} steps ({})", notes.join("; ")))
            }
            cmd => self.step(cmd, idle_s).await,
        };
        self.touch();
        r
    }

    async fn shutdown(&mut self) {
        if let Some((t, released)) = self.close("disconnect (shutdown)").await {
            info!(target = %t, released, "disconnected on shutdown");
        }
    }
}

fn mouse(flags: PointerFlags, x: u16, y: u16) -> FastPathInputEvent {
    FastPathInputEvent::MouseEvent(MousePdu {
        flags,
        number_of_wheel_rotation_units: 0,
        x_position: x,
        y_position: y,
    })
}

fn ok(text: String) -> Reply {
    Reply::Ok { text }
}

fn err(code: ErrorCode, message: impl Into<String>) -> Reply {
    Reply::Error {
        code,
        message: message.into(),
        completed_steps: None,
    }
}

fn bad_args(m: impl Into<String>) -> Reply {
    err(ErrorCode::BadArgs, m)
}

fn session_err(m: impl Into<String>) -> Reply {
    err(ErrorCode::Session, m)
}

fn not_connected() -> Reply {
    err(ErrorCode::NotConnected, "not connected; call connect first")
}

fn session_ended() -> Reply {
    err(
        ErrorCode::NotConnected,
        "the session has ended; connect again",
    )
}

fn no_image() -> Reply {
    session_err("no desktop image yet; use wait (change=true) and try again")
}

/// A failed start: the reply, and whether the till rejected the logon.
struct StartError {
    reply: Reply,
    logon_rejected: bool,
}

fn map_connect_error(target: &str, user: &str, e: &CoreError) -> StartError {
    match e {
        CoreError::ExistingSession => StartError {
            reply: err(ErrorCode::ExistingSession, MSG_EXISTING_SESSION),
            logon_rejected: false,
        },
        CoreError::LogonRejected(_) | CoreError::Sspi(_) => StartError {
            reply: session_err(format!(
                "logon as {user} failed: {e}. Not retried (account lockout risk): fix the password (viewer Keychain or SCCM_PASS) and restart the MCP server"
            )),
            logon_rejected: true,
        },
        e => StartError {
            reply: session_err(format!("connecting to {target} failed: {e}")),
            logon_rejected: false,
        },
    }
}

/// Wait for the first desktop image. A "host in use" report before it means a
/// logged-on user; without force the session is stopped and released.
pub(crate) async fn await_first_frame(
    mut session: Session,
    host_in_use: &AtomicBool,
    force: bool,
    timeout: Duration,
) -> Result<Session, Reply> {
    let deadline = Instant::now() + timeout;
    loop {
        if !force && host_in_use.load(Ordering::SeqCst) {
            session.stop_and_wait().await;
            return Err(err(ErrorCode::InUse, MSG_IN_USE));
        }
        if session.has_frame() {
            return Ok(session);
        }
        if let Some(why) = session.shared.ended() {
            session.stop_and_wait().await;
            return Err(session_err(format!(
                "the session ended before the first image: {why}"
            )));
        }
        if Instant::now() >= deadline {
            // Connected, just slow to paint; connect says so.
            return Ok(session);
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn real_connect(
    target: String,
    force: bool,
    rejected: Arc<Mutex<Option<String>>>,
) -> Result<Session, Reply> {
    if let Some(user) = lock(&rejected).clone() {
        return Err(session_err(format!(
            "an earlier logon as {user} was rejected; not retrying (account lockout risk). Fix the password and restart the MCP server"
        )));
    }
    // The Keychain lookup can block on a system dialog.
    let creds = tokio::task::spawn_blocking(|| {
        creds::resolve(
            |k| std::env::var(k).ok(),
            creds::last_user(),
            sccm_rc_core::keychain::load,
        )
    })
    .await
    .map_err(|e| session_err(format!("credential lookup failed: {e}")))?
    .map_err(|e| session_err(e.to_string()))?;
    let user = creds.display_name();

    let shared = Arc::new(Shared::default());
    let opts = RdpOptions {
        takeover: force,
        host_in_use: Arc::new(AtomicBool::new(false)),
    };
    let host_in_use = opts.host_in_use.clone();
    let (input_tx, input_rx) = tokio::sync::mpsc::channel(256);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<bool, StartError>>();
    let (done_tx, done_rx) = oneshot::channel::<()>();
    let thread_shared = shared.clone();
    let thread_target = target.clone();
    std::thread::Builder::new()
        .name("sccm-rc-session".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(session_thread(
                    thread_target,
                    creds,
                    opts,
                    thread_shared,
                    input_rx,
                    ready_tx,
                )),
                Err(e) => {
                    let _ = ready_tx.send(Err(StartError {
                        reply: session_err(format!("runtime: {e}")),
                        logon_rejected: false,
                    }));
                }
            }
            let _ = done_tx.send(());
        })
        .map_err(|e| session_err(format!("could not start the session thread: {e}")))?;

    let view_only = match tokio::time::timeout(STARTUP_TIMEOUT, ready_rx).await {
        Ok(Ok(Ok(view_only))) => view_only,
        Ok(Ok(Err(e))) => {
            if e.logon_rejected {
                *lock(&rejected) = Some(user);
            }
            return Err(e.reply);
        }
        Ok(Err(_)) => return Err(session_err("the session thread stopped unexpectedly")),
        Err(_) => {
            // Dropping the sender lets the thread wind down and disconnect.
            drop(input_tx);
            return Err(session_err(format!(
                "connecting to {target} timed out after {} s",
                STARTUP_TIMEOUT.as_secs()
            )));
        }
    };
    let session = Session::new(input_tx, shared, Some(done_rx), view_only);
    await_first_frame(session, &host_in_use, force, FIRST_FRAME_WAIT).await
}

/// Runs on the session thread. Reports start-up via `ready` (Ok(view_only)),
/// then runs until the input channel closes or the till ends the session,
/// and always disconnects once the SCCM session exists.
async fn session_thread(
    target: String,
    creds: LogonCredentials,
    opts: RdpOptions,
    shared: Arc<Shared>,
    mut input_rx: InputReceiver,
    ready: oneshot::Sender<Result<bool, StartError>>,
) {
    let user = creds.display_name();
    let connected = tokio::time::timeout(
        CONNECT_TIMEOUT,
        SccmSession::connect_with(&target, Some(&creds)),
    )
    .await;
    drop(creds);
    let mut session = match connected {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            warn!(error = %e, "SCCM connect failed");
            shared.end(e.to_string());
            let _ = ready.send(Err(map_connect_error(&target, &user, &e)));
            return;
        }
        Err(_) => {
            shared.end("connect timed out");
            let _ = ready.send(Err(StartError {
                reply: session_err(format!(
                    "connecting to {target} timed out after {} s",
                    CONNECT_TIMEOUT.as_secs()
                )),
                logon_rejected: false,
            }));
            return;
        }
    };
    let grant = session.grant();
    let grant_s = format!("{grant:?}");
    info!(grant = %grant_s, "session established");
    audit::log_event(&target, &grant_s, "connect", None);
    let started = Instant::now();
    let counter = Arc::new(AtomicU64::new(0));
    let mut ready = Some(ready);
    let res = async {
        let (result, buf, share_id) =
            rdp::connect_rdp_with(&mut session, DESK_W, DESK_H, &[], counter.clone(), &opts)
                .await?;
        info!("RDP active");
        if let Some(r) = ready.take() {
            let _ = r.send(Ok(grant == sccm_rc_core::Grant::ViewOnly));
        }
        let mut sink = Sink {
            shared: shared.clone(),
        };
        rdp::run_active_session_with(
            &mut session,
            result,
            buf,
            share_id,
            &mut sink,
            &mut input_rx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            counter,
            &opts,
        )
        .await
    }
    .await;
    session.disconnect().await;
    audit::log_event(
        &target,
        &grant_s,
        "disconnect",
        Some(started.elapsed().as_secs()),
    );
    match &res {
        Ok(()) => shared.end("the session was closed"),
        Err(e) => {
            warn!(error = %e, "session error");
            shared.end(format!("session error: {e}"));
        }
    }
    if let Some(r) = ready.take() {
        let msg = match res {
            Err(e) => format!("setting up the display failed: {e}"),
            Ok(()) => "setting up the display failed".into(),
        };
        let _ = r.send(Err(StartError {
            reply: session_err(msg),
            logon_rejected: false,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    type Audited = Arc<Mutex<Vec<String>>>;

    /// A stub session with a `w`x`h` frame (0 = no frame yet).
    fn stub(w: u16, h: u16) -> (Session, InputReceiver, Arc<Shared>) {
        let shared = Arc::new(Shared::default());
        if w > 0 {
            let mut f = lock(&shared.frame);
            f.rgba = vec![0x80; w as usize * h as usize * 4];
            f.w = w;
            f.h = h;
        }
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        (Session::new(tx, shared.clone(), None, false), rx, shared)
    }

    /// A backend whose connector hands out stub sessions; the receivers
    /// are kept so the sessions stay alive.
    fn backend(
        w: u16,
        h: u16,
    ) -> (
        Headless,
        Arc<Mutex<Vec<InputReceiver>>>,
        Audited,
        Arc<AtomicUsize>,
    ) {
        let rxs: Arc<Mutex<Vec<InputReceiver>>> = Arc::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let audited: Audited = Arc::default();
        let (r2, c2, a2) = (rxs.clone(), calls.clone(), audited.clone());
        let hl = Headless::with_connector(move |_t, _f| {
            c2.fetch_add(1, Ordering::SeqCst);
            let (s, rx, _) = stub(w, h);
            lock(&r2).push(rx);
            Box::pin(async move { Ok(s) })
        })
        .with_audit(move |t, a| lock(&a2).push(format!("{t}: {a}")));
        (hl, rxs, audited, calls)
    }

    fn drain(rxs: &Arc<Mutex<Vec<InputReceiver>>>) -> Vec<FastPathInputEvent> {
        let mut out = Vec::new();
        for rx in lock(rxs).iter_mut() {
            while let Ok(v) = rx.try_recv() {
                out.extend(v);
            }
        }
        out
    }

    fn mouse_pos(ev: &[FastPathInputEvent]) -> Vec<(PointerFlags, u16, u16)> {
        ev.iter()
            .filter_map(|e| match e {
                FastPathInputEvent::MouseEvent(m) => Some((m.flags, m.x_position, m.y_position)),
                _ => None,
            })
            .collect()
    }

    fn is_ok(r: &Reply) -> bool {
        matches!(r, Reply::Ok { .. })
    }

    fn code(r: &Reply) -> Option<ErrorCode> {
        match r {
            Reply::Error { code, .. } => Some(*code),
            _ => None,
        }
    }

    fn click(x: f64, y: f64, relative: bool) -> Command {
        Command::Click {
            x,
            y,
            relative,
            button: Button::Left,
            double: false,
        }
    }

    #[tokio::test]
    async fn second_connect_rejected() {
        let (mut hl, _rx, _a, calls) = backend(100, 100);
        assert!(is_ok(&hl.connect("till-a", false).await));
        let r = hl.connect("till-b", false).await;
        assert_eq!(
            r,
            Reply::Error {
                code: ErrorCode::BadArgs,
                message: "already connected to till-a".into(),
                completed_steps: None
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // After a disconnect a new connect works.
        assert!(is_ok(&hl.run(Command::Disconnect).await));
        assert!(is_ok(&hl.connect("till-b", false).await));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn commands_need_a_connection() {
        let (mut hl, _rx, _a, _c) = backend(100, 100);
        assert_eq!(
            code(&hl.run(click(1.0, 1.0, false)).await),
            Some(ErrorCode::NotConnected)
        );
        match hl.run(Command::Status).await {
            Reply::Status {
                connected, mode, ..
            } => {
                assert!(!connected);
                assert_eq!(mode, "headless");
            }
            r => panic!("{r:?}"),
        }
    }

    #[tokio::test]
    async fn click_before_screenshot_uses_desktop_pixels() {
        let (mut hl, rxs, _a, _c) = backend(1920, 1080);
        hl.connect("t", false).await;
        assert!(is_ok(&hl.run(click(960.0, 540.0, false)).await));
        let m = mouse_pos(&drain(&rxs));
        assert_eq!(
            m,
            vec![
                (PointerFlags::MOVE, 960, 540),
                (PointerFlags::LEFT_BUTTON | PointerFlags::DOWN, 960, 540),
                (PointerFlags::LEFT_BUTTON, 960, 540),
            ]
        );
        assert_eq!(
            code(&hl.run(click(1920.0, 10.0, false)).await),
            Some(ErrorCode::BadArgs)
        );
    }

    #[tokio::test]
    async fn click_maps_through_screenshot_space() {
        let (mut hl, rxs, _a, _c) = backend(1920, 1080);
        hl.connect("t", false).await;
        match hl
            .run(Command::Screenshot {
                region: None,
                max_width: None,
            })
            .await
        {
            Reply::Image {
                desk_w,
                desk_h,
                scale,
                png_base64,
            } => {
                assert_eq!((desk_w, desk_h), (1920, 1080));
                assert!((scale - 1.5).abs() < 1e-9);
                assert!(!png_base64.is_empty());
            }
            r => panic!("{r:?}"),
        }
        assert!(is_ok(&hl.run(click(640.0, 360.0, false)).await));
        assert_eq!(mouse_pos(&drain(&rxs))[0], (PointerFlags::MOVE, 960, 540));
        // Relative ignores the screenshot space.
        assert!(is_ok(
            &hl.run(Command::Move {
                x: 1.0,
                y: 1.0,
                relative: true
            })
            .await
        ));
        assert_eq!(
            mouse_pos(&drain(&rxs)),
            vec![(PointerFlags::MOVE, 1919, 1079)]
        );
        // Outside the 1280x720 image.
        assert_eq!(
            code(&hl.run(click(1281.0, 10.0, false)).await),
            Some(ErrorCode::BadArgs)
        );
    }

    #[tokio::test]
    async fn stale_screenshot_rejected_after_resize() {
        let (s, _rx, shared) = stub(1920, 1080);
        let slot = Arc::new(Mutex::new(Some(s)));
        let mut hl = Headless::with_connector(move |_t, _f| {
            let s = lock(&slot).take().unwrap();
            Box::pin(async move { Ok(s) })
        })
        .with_audit(|_, _| {});
        hl.connect("t", false).await;
        hl.run(Command::Screenshot {
            region: None,
            max_width: None,
        })
        .await;
        {
            let mut f = lock(&shared.frame);
            f.w = 1024;
            f.h = 768;
            f.rgba = vec![0; 1024 * 768 * 4];
        }
        match hl.run(click(10.0, 10.0, false)).await {
            Reply::Error { code, message, .. } => {
                assert_eq!(code, ErrorCode::BadArgs);
                assert!(message.contains("new screenshot"), "{message}");
            }
            r => panic!("{r:?}"),
        }
    }

    #[tokio::test]
    async fn no_frame_yet_is_an_error() {
        let (mut hl, _rx, _a, _c) = backend(0, 0);
        match hl.connect("t", false).await {
            Reply::Ok { text } => assert!(text.contains("no desktop image yet"), "{text}"),
            r => panic!("{r:?}"),
        }
        assert_eq!(
            code(&hl.run(click(1.0, 1.0, true)).await),
            Some(ErrorCode::Session)
        );
    }

    #[tokio::test]
    async fn scroll_sends_vertical_wheel_notches() {
        let (mut hl, rxs, _a, _c) = backend(100, 100);
        hl.connect("t", false).await;
        assert!(is_ok(
            &hl.run(Command::Scroll {
                x: 10.0,
                y: 20.0,
                amount: -2,
                relative: false
            })
            .await
        ));
        let ev = drain(&rxs);
        assert_eq!(ev.len(), 3);
        for e in &ev[1..] {
            match e {
                FastPathInputEvent::MouseEvent(m) => {
                    assert_eq!(m.flags, PointerFlags::VERTICAL_WHEEL);
                    assert_eq!(m.number_of_wheel_rotation_units, -120);
                }
                e => panic!("{e:?}"),
            }
        }
    }

    #[tokio::test]
    async fn secret_text_audited_without_text() {
        let (mut hl, rxs, audited, _c) = backend(100, 100);
        hl.connect("till-01", false).await;
        let r = hl
            .run(Command::TypeText {
                text: "hunter2ä".into(),
                secret: true,
            })
            .await;
        match &r {
            Reply::Ok { text } => assert!(!text.contains("hunter2"), "{text}"),
            r => panic!("{r:?}"),
        }
        assert_eq!(drain(&rxs).len(), 16);
        let a = lock(&audited).join("\n");
        assert!(!a.contains("hunter2"), "{a}");
        assert!(a.contains("till-01: type_text (secret, 8 chars)"), "{a}");
    }

    #[tokio::test]
    async fn type_text_reports_skipped_chars() {
        let (mut hl, _rx, _a, _c) = backend(100, 100);
        hl.connect("t", false).await;
        match hl
            .run(Command::TypeText {
                text: "a😀b".into(),
                secret: false,
            })
            .await
        {
            Reply::Ok { text } => {
                assert!(text.contains("typed 2 characters"), "{text}");
                assert!(text.contains("skipped 1"), "{text}");
            }
            r => panic!("{r:?}"),
        }
    }

    #[tokio::test]
    async fn batch_stops_at_first_error() {
        let (mut hl, rxs, _a, _c) = backend(100, 100);
        hl.connect("t", false).await;
        let r = hl
            .run(Command::Batch {
                steps: vec![
                    Command::Key {
                        combo: "Enter".into(),
                    },
                    Command::Key {
                        combo: "NoSuchKey".into(),
                    },
                    Command::Key {
                        combo: "Tab".into(),
                    },
                ],
            })
            .await;
        match r {
            Reply::Error {
                code,
                completed_steps,
                ..
            } => {
                assert_eq!(code, ErrorCode::BadArgs);
                assert_eq!(completed_steps, Some(1));
            }
            r => panic!("{r:?}"),
        }
        assert_eq!(drain(&rxs).len(), 2, "only Enter down/up was sent");
    }

    #[tokio::test]
    async fn invalid_command_rejected() {
        let (mut hl, _rx, _a, _c) = backend(100, 100);
        hl.connect("t", false).await;
        let r = hl
            .run(Command::Wait {
                change: false,
                stable_ms: None,
                timeout_ms: 10,
            })
            .await;
        assert_eq!(code(&r), Some(ErrorCode::BadArgs));
    }

    #[tokio::test]
    async fn wait_sees_change() {
        let (s, _rx, shared) = stub(100, 100);
        let slot = Arc::new(Mutex::new(Some(s)));
        let mut hl = Headless::with_connector(move |_t, _f| {
            let s = lock(&slot).take().unwrap();
            Box::pin(async move { Ok(s) })
        })
        .with_audit(|_, _| {});
        hl.connect("t", false).await;
        let sh = shared.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            lock(&sh.watch).on_update(Instant::now());
        });
        let r = hl
            .run(Command::Wait {
                change: true,
                stable_ms: None,
                timeout_ms: 5000,
            })
            .await;
        assert_eq!(r, ok("changed".into()));
        let r = hl
            .run(Command::Wait {
                change: true,
                stable_ms: None,
                timeout_ms: 100,
            })
            .await;
        assert_eq!(r, ok("timeout".into()));
    }

    #[tokio::test]
    async fn ended_session_reports_not_connected() {
        let (s, _rx, shared) = stub(100, 100);
        let slot = Arc::new(Mutex::new(Some(s)));
        let mut hl = Headless::with_connector(move |_t, _f| {
            let s = lock(&slot).take().unwrap();
            Box::pin(async move { Ok(s) })
        })
        .with_audit(|_, _| {});
        hl.connect("t", false).await;
        shared.end("the till ended the session: bye");
        match hl.run(click(1.0, 1.0, false)).await {
            Reply::Error { code, message, .. } => {
                assert_eq!(code, ErrorCode::NotConnected);
                assert!(message.contains("bye"), "{message}");
            }
            r => panic!("{r:?}"),
        }
    }

    #[tokio::test]
    async fn idle_timeout_stops_session() {
        let (hl, _rx, audited, _c) = backend(100, 100);
        let mut hl = hl.with_idle(Duration::from_millis(100));
        hl.connect("t", false).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(hl.conn.as_ref().unwrap().session.stopped());
        assert!(lock(&audited).iter().any(|a| a.contains("idle timeout")));
        assert_eq!(
            code(&hl.run(click(1.0, 1.0, false)).await),
            Some(ErrorCode::NotConnected)
        );
    }

    #[tokio::test]
    async fn shutdown_stops_session() {
        let (mut hl, _rx, audited, _c) = backend(100, 100);
        hl.connect("t", false).await;
        let input = hl.conn.as_ref().unwrap().session.input.clone();
        hl.shutdown().await;
        assert!(lock(&input).is_none());
        assert!(hl.conn.is_none());
        assert!(lock(&audited).iter().any(|a| a.contains("shutdown")));
        hl.shutdown().await; // idempotent
    }

    #[tokio::test]
    async fn host_in_use_without_force_stops_session() {
        let (s, _rx, _shared) = stub(0, 0);
        let input = s.input.clone();
        let flag = AtomicBool::new(true);
        let r = await_first_frame(s, &flag, false, Duration::from_secs(2)).await;
        match r {
            Err(Reply::Error { code, message, .. }) => {
                assert_eq!(code, ErrorCode::InUse);
                assert_eq!(message, MSG_IN_USE);
            }
            _ => panic!("expected InUse"),
        }
        assert!(lock(&input).is_none(), "session must be stopped");
    }

    #[tokio::test]
    async fn host_in_use_ignored_with_force() {
        let (s, _rx, _shared) = stub(100, 100);
        let flag = AtomicBool::new(true);
        assert!(await_first_frame(s, &flag, true, Duration::from_secs(2))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn first_frame_timeout_still_connects() {
        let (s, _rx, _shared) = stub(0, 0);
        let flag = AtomicBool::new(false);
        let s = await_first_frame(s, &flag, false, Duration::from_millis(100))
            .await
            .ok()
            .unwrap();
        assert!(!s.has_frame());
    }

    #[test]
    fn connect_errors_map_to_codes() {
        let e = map_connect_error("t", "D\\u", &CoreError::ExistingSession);
        assert_eq!(code(&e.reply), Some(ErrorCode::ExistingSession));
        assert!(!e.logon_rejected);
        let e = map_connect_error("t", "D\\u", &CoreError::LogonRejected("denied".into()));
        assert_eq!(code(&e.reply), Some(ErrorCode::Session));
        assert!(e.logon_rejected);
        let e = map_connect_error("t", "D\\u", &CoreError::Refused);
        assert!(!e.logon_rejected);
    }

    #[test]
    fn sink_copies_dirty_region() {
        struct F(Vec<u8>);
        impl FrameView for F {
            fn data(&self) -> &[u8] {
                &self.0
            }
            fn width(&self) -> u16 {
                4
            }
            fn height(&self) -> u16 {
                2
            }
        }
        let shared = Arc::new(Shared::default());
        let mut sink = Sink {
            shared: shared.clone(),
        };
        let full = UpdateRegion {
            left: 0,
            top: 0,
            right: 3,
            bottom: 1,
        };
        sink.on_graphics_update(&F(vec![1; 32]), full);
        let one = UpdateRegion {
            left: 1,
            top: 1,
            right: 1,
            bottom: 1,
        };
        sink.on_graphics_update(&F(vec![9; 32]), one);
        let f = lock(&shared.frame);
        assert_eq!((f.w, f.h), (4, 2));
        let px = |x: usize, y: usize| f.rgba[(y * 4 + x) * 4];
        assert_eq!((px(1, 1), px(0, 1), px(1, 0)), (9, 1, 1));
        assert_eq!(lock(&shared.watch).mark(), 2);
    }
}
