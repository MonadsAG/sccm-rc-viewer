//! "Allow control by Claude": the viewer's control socket (watch-along mode).
//!
//! While the toggle is on, the viewer listens on `control.sock` in its data
//! folder (0600). Every request line is one [`Command`]; it is handed to the
//! App thread, which runs it on the viewer's own session as a [`Job`] (paced
//! by a [`Ticker`], never blocking the event loop) and answers one [`Reply`]
//! line. The user's own keyboard and mouse win: the [`Arbiter`] aborts a
//! running command on local input and turns commands away for two seconds.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use sccm_rc_core::agent::exec::{self, bad_args, Chunk, Held, STEP_GAP};
use sccm_rc_core::agent::proto::{Command, ErrorCode, Reply};
use sccm_rc_core::agent::shot::ShotSpace;
use sccm_rc_core::agent::wait::FrameWatch;
use sccm_rc_core::rdp::{FastPathInputEvent, PointerFlags};

/// Commands are turned away this long after the user's last input.
pub const COOLDOWN: Duration = Duration::from_secs(2);
/// Pause between text chunks, so the user's input can get in between.
const TEXT_GAP: Duration = STEP_GAP;
/// How often a running `wait` looks at the frame counter.
const WAIT_POLL: Duration = Duration::from_millis(50);
pub const SOCKET_NAME: &str = "control.sock";
pub const MSG_TOOK_OVER: &str =
    "the user took over (local keyboard or mouse input); command aborted";
pub const MSG_COOLDOWN: &str =
    "the user is working in the viewer; try again when they have stopped for 2 seconds";

/// Who has the remote session's input: pure state, all `Instant`s injected.
#[derive(Debug, Default)]
pub struct Arbiter {
    last_input: Option<Instant>,
    running: bool,
    interrupted: bool,
}

impl Arbiter {
    /// The user sent keyboard or mouse input to the remote.
    pub fn on_user_input(&mut self, now: Instant) {
        self.last_input = Some(self.last_input.map_or(now, |t| t.max(now)));
        if self.running {
            self.interrupted = true;
        }
    }

    pub fn begin_command(&mut self) {
        self.running = true;
        self.interrupted = false;
    }

    pub fn end_command(&mut self) {
        self.running = false;
        self.interrupted = false;
    }

    /// The running command must stop: the user has touched the keyboard or
    /// mouse since it began.
    pub fn should_abort(&self, _now: Instant) -> bool {
        self.running && self.interrupted
    }

    /// False for [`COOLDOWN`] after the user's last input.
    pub fn accepts_commands(&self, now: Instant) -> bool {
        self.last_input
            .is_none_or(|t| now.saturating_duration_since(t) >= COOLDOWN)
    }
}

/// What a [`Job`] needs from the viewer session.
pub trait Target {
    /// Desktop size; 0x0 while no image has arrived.
    fn desk_size(&self) -> (u16, u16);
    /// Geometry of the last screenshot Claude got.
    fn shot(&self) -> Option<ShotSpace>;
    /// `Err` when there is no session (any more).
    fn alive(&self) -> Result<(), Reply>;
    fn send(&mut self, events: Vec<FastPathInputEvent>) -> Result<(), Reply>;
    /// Let go of keys and buttons. Needs only the input channel: never
    /// refused for view-only, or a button could stay pressed on the till.
    fn release(&mut self, events: Vec<FastPathInputEvent>);
    fn watch(&self) -> FrameWatch;
    /// Already redacted text.
    fn audit(&mut self, action: &str);
}

pub enum Poll {
    /// Poll again at this time.
    Pending(Instant),
    Done(Reply),
}

enum Op {
    Chunk(Chunk),
    Wait {
        mark: u64,
        change: bool,
        stable_ms: Option<u64>,
        started: Instant,
        timeout: Duration,
    },
}

struct Step {
    ops: VecDeque<Op>,
    done: Reply,
}

/// One command (a batch or a single step) running on the App thread, one
/// piece per [`Job::poll`].
pub struct Job {
    steps: VecDeque<Command>,
    total: usize,
    batch: bool,
    completed: usize,
    notes: Vec<String>,
    current: Option<Step>,
    held: Held,
    next_at: Option<Instant>,
}

impl Job {
    /// `cmd` has passed [`exec::check`]; status, screenshot and disconnect
    /// are answered by the App directly.
    pub fn new(cmd: Command) -> Self {
        let (steps, batch) = match cmd {
            Command::Batch { steps } => (steps.into(), true),
            c => (VecDeque::from([c]), false),
        };
        Self {
            total: steps.len(),
            steps,
            batch,
            completed: 0,
            notes: Vec::new(),
            current: None,
            held: Held::new(),
            next_at: None,
        }
    }

    /// Run whatever is due at `now`.
    pub fn poll(&mut self, now: Instant, t: &mut dyn Target) -> Poll {
        loop {
            if let Some(at) = self.next_at {
                if now < at {
                    return Poll::Pending(at);
                }
                self.next_at = None;
            }
            if self.current.is_none() {
                let Some(cmd) = self.steps.pop_front() else {
                    return Poll::Done(exec::batch_done(self.total, &self.notes));
                };
                match start_step(cmd, now, t) {
                    Ok(s) => self.current = Some(s),
                    Err(e) => return Poll::Done(self.fail(e, t)),
                }
            }
            if let Err(e) = t.alive() {
                return Poll::Done(self.fail(e, t));
            }
            let step = self.current.as_mut().expect("a step is running");
            match step.ops.pop_front() {
                None => {
                    let r = self.current.take().expect("a step is running").done;
                    self.completed += 1;
                    if !self.batch {
                        return Poll::Done(r);
                    }
                    if let Reply::Ok { text } = r {
                        self.notes.push(text);
                    }
                    if !self.steps.is_empty() {
                        self.next_at = Some(now + STEP_GAP);
                    }
                }
                Some(Op::Chunk(Chunk::Input(ev))) => {
                    if let Err(e) = t.send(ev.clone()) {
                        return Poll::Done(self.fail(e, t));
                    }
                    self.held.track(&ev);
                }
                Some(Op::Chunk(Chunk::Pause(d))) => self.next_at = Some(now + d),
                Some(Op::Chunk(Chunk::Yield)) => self.next_at = Some(now + TEXT_GAP),
                Some(Op::Wait {
                    mark,
                    change,
                    stable_ms,
                    started,
                    timeout,
                }) => match t
                    .watch()
                    .evaluate(mark, change, stable_ms, started, timeout, now)
                {
                    Some(r) => step.done = exec::wait_reply(r),
                    None => {
                        step.ops.push_front(Op::Wait {
                            mark,
                            change,
                            stable_ms,
                            started,
                            timeout,
                        });
                        self.next_at = Some(now + WAIT_POLL);
                    }
                },
            }
        }
    }

    /// Stop now: let go of every key and button this command still holds and
    /// answer `code` with the steps completed so far.
    pub fn abort(&mut self, code: ErrorCode, message: &str, t: &mut dyn Target) -> Reply {
        self.release(t);
        t.audit(&format!("command aborted: {message}"));
        Reply::Error {
            code,
            message: message.into(),
            completed_steps: Some(self.completed),
        }
    }

    fn release(&mut self, t: &mut dyn Target) {
        let ev = self.held.release_all();
        if !ev.is_empty() {
            t.release(ev);
        }
    }

    fn fail(&mut self, e: Reply, t: &mut dyn Target) -> Reply {
        self.release(t);
        if self.batch {
            exec::step_failed(self.completed, e)
        } else {
            e
        }
    }
}

fn start_step(cmd: Command, now: Instant, t: &mut dyn Target) -> Result<Step, Reply> {
    t.alive()?;
    if let Command::Wait {
        change,
        stable_ms,
        timeout_ms,
    } = cmd
    {
        exec::check_wait(timeout_ms)?;
        return Ok(Step {
            ops: VecDeque::from([Op::Wait {
                mark: t.watch().mark(),
                change,
                stable_ms,
                started: now,
                timeout: Duration::from_millis(timeout_ms),
            }]),
            done: exec::ok("timeout"),
        });
    }
    let (w, h) = t.desk_size();
    let shot = t.shot();
    let plan = exec::plan_input(&cmd, |x, y, r| {
        exec::map_point(x, y, r, shot.as_ref(), w, h)
    })?
    .ok_or_else(|| bad_args("this command cannot run as a step"))?;
    drop(cmd);
    t.audit(&plan.audit);
    Ok(Step {
        ops: plan.chunks.into_iter().map(Op::Chunk).collect(),
        done: exec::ok(plan.done),
    })
}

/// Wakes the event loop at a requested time (one pending deadline, the
/// earliest wins), so a job's next piece runs without blocking the loop.
pub struct Ticker {
    shared: Arc<(Mutex<TickState>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct TickState {
    at: Option<Instant>,
    quit: bool,
}

impl Ticker {
    pub fn new(fire: impl Fn() + Send + 'static) -> Self {
        let shared = Arc::new((Mutex::new(TickState::default()), Condvar::new()));
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("control-ticker".into())
            .spawn(move || {
                let (m, cv) = &*s;
                let mut st = m.lock().unwrap();
                loop {
                    if st.quit {
                        return;
                    }
                    match st.at {
                        None => st = cv.wait(st).unwrap(),
                        Some(at) => {
                            let now = Instant::now();
                            if now >= at {
                                st.at = None;
                                drop(st);
                                fire();
                                st = m.lock().unwrap();
                            } else {
                                st = cv.wait_timeout(st, at - now).unwrap().0;
                            }
                        }
                    }
                }
            })
            .expect("spawn control ticker");
        Self {
            shared,
            thread: Some(thread),
        }
    }

    pub fn schedule(&self, at: Instant) {
        let (m, cv) = &*self.shared;
        let mut st = m.lock().unwrap();
        if st.at.is_none_or(|cur| at < cur) {
            st.at = Some(at);
            cv.notify_one();
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        let (m, cv) = &*self.shared;
        m.lock().unwrap().quit = true;
        cv.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Parse and check one request line. `None` for a blank line.
/// Whether the user's input event takes over from Claude: anything but a
/// plain pointer move.
pub fn counts_as_takeover(ev: &FastPathInputEvent) -> bool {
    !matches!(ev, FastPathInputEvent::MouseEvent(m) if m.flags == PointerFlags::MOVE)
}

/// The command's name, for logs (never its arguments).
pub fn cmd_name(c: &Command) -> &'static str {
    match c {
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

pub fn parse_request(line: &[u8]) -> Option<Result<Command, Reply>> {
    let Ok(text) = std::str::from_utf8(line) else {
        return Some(Err(bad_args("request is not valid UTF-8")));
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(
        serde_json::from_str::<Command>(text)
            // serde's message can quote a value, which may be typed text.
            .map_err(|_| bad_args("invalid command"))
            .and_then(|c| exec::check(&c).map(|()| c)),
    )
}

#[cfg(unix)]
mod unix {
    use std::collections::HashMap;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    use sccm_rc_core::agent::exec::{bad_args, session_err};
    use sccm_rc_core::agent::proto::{Command, Reply};
    use tracing::{debug, info, warn};
    use winit::event_loop::EventLoopProxy;

    use crate::UserEvent;

    /// Longest accepted request line (a long type_text fits easily).
    const MAX_LINE: usize = 1 << 20;
    const ACCEPT_POLL: Duration = Duration::from_millis(50);

    type Dispatch = Arc<dyn Fn(Command, mpsc::Sender<Reply>) -> bool + Send + Sync>;
    type Conns = Arc<Mutex<HashMap<u64, UnixStream>>>;

    /// The listening control socket. Dropping it stops it.
    pub struct Server {
        path: PathBuf,
        stop: Arc<AtomicBool>,
        accept: Option<std::thread::JoinHandle<()>>,
        conns: Conns,
    }

    impl Server {
        /// Listen on `data_dir/control.sock`; each request goes to the App
        /// as `UserEvent::Control`.
        pub fn start(data_dir: &Path, proxy: EventLoopProxy<UserEvent>) -> io::Result<Server> {
            let proxy = Mutex::new(proxy);
            Self::start_with(data_dir, move |cmd, reply| {
                proxy
                    .lock()
                    .unwrap()
                    .send_event(UserEvent::Control(cmd, reply))
                    .is_ok()
            })
        }

        /// `start` with any App side: `dispatch` hands a command and its
        /// reply channel over and returns false if nobody will answer.
        pub fn start_with(
            data_dir: &Path,
            dispatch: impl Fn(Command, mpsc::Sender<Reply>) -> bool + Send + Sync + 'static,
        ) -> io::Result<Server> {
            let path = data_dir.join(super::SOCKET_NAME);
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                if UnixStream::connect(&path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "another viewer already allows control by Claude",
                    ));
                }
                if !meta.file_type().is_socket() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a socket", path.display()),
                    ));
                }
                std::fs::remove_file(&path)?;
            }
            let listener = UnixListener::bind(&path)?;
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
            listener.set_nonblocking(true)?;
            let stop = Arc::new(AtomicBool::new(false));
            let conns: Conns = Arc::default();
            let dispatch: Dispatch = Arc::new(dispatch);
            let accept = {
                let (stop, conns) = (stop.clone(), conns.clone());
                std::thread::Builder::new()
                    .name("control-accept".into())
                    .spawn(move || accept_loop(listener, stop, conns, dispatch))?
            };
            info!(path = %path.display(), "control socket listening");
            Ok(Server {
                path,
                stop,
                accept: Some(accept),
                conns,
            })
        }

        /// Stop listening, delete the socket file and close every connection
        /// (a reply already handed over is still written).
        pub fn stop(&mut self) {
            let Some(accept) = self.accept.take() else {
                return;
            };
            self.stop.store(true, Ordering::SeqCst);
            let _ = accept.join();
            let _ = std::fs::remove_file(&self.path);
            for (_, c) in self.conns.lock().unwrap().drain() {
                let _ = c.shutdown(std::net::Shutdown::Read);
            }
            info!("control socket closed");
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn accept_loop(listener: UnixListener, stop: Arc<AtomicBool>, conns: Conns, d: Dispatch) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((s, _)) => {
                    if s.set_nonblocking(false).is_err() {
                        continue;
                    }
                    let id = NEXT.fetch_add(1, Ordering::Relaxed);
                    let Ok(clone) = s.try_clone() else { continue };
                    conns.lock().unwrap().insert(id, clone);
                    // A connection accepted while stopping is closed below.
                    if stop.load(Ordering::SeqCst) {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                        conns.lock().unwrap().remove(&id);
                        break;
                    }
                    let (conns, d) = (conns.clone(), d.clone());
                    let _ = std::thread::Builder::new()
                        .name("control-conn".into())
                        .spawn(move || {
                            serve(s, &d);
                            conns.lock().unwrap().remove(&id);
                        });
                    debug!(id, "control client connected");
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
                Err(e) => {
                    warn!(error = %e, "control socket accept failed");
                    std::thread::sleep(ACCEPT_POLL);
                }
            }
        }
    }

    fn serve(stream: UnixStream, d: &Dispatch) {
        let Ok(read) = stream.try_clone() else { return };
        let mut reader = BufReader::new(read);
        let mut w = stream;
        loop {
            let mut buf = Vec::new();
            match (&mut reader)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut buf)
            {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let too_long = buf.len() > MAX_LINE;
            let reply = if too_long {
                Some(bad_args("request line too long"))
            } else {
                super::parse_request(&buf).map(|r| match r {
                    Ok(cmd) => {
                        debug!(cmd = super::cmd_name(&cmd), "control command");
                        let (tx, rx) = mpsc::channel();
                        if !d(cmd, tx) {
                            session_err("the viewer is shutting down")
                        } else {
                            rx.recv()
                                .unwrap_or_else(|_| session_err("the viewer dropped the command"))
                        }
                    }
                    Err(e) => e,
                })
            };
            if let Some(r) = reply {
                let mut line = serde_json::to_vec(&r).unwrap_or_default();
                line.push(b'\n');
                if w.write_all(&line).and_then(|()| w.flush()).is_err() {
                    return;
                }
            }
            if too_long {
                return;
            }
        }
    }
}

#[cfg(unix)]
pub use unix::Server;

#[cfg(not(unix))]
pub use other::Server;

#[cfg(not(unix))]
mod other {
    use std::io;
    use std::path::Path;

    use winit::event_loop::EventLoopProxy;

    use crate::UserEvent;

    /// Control by Claude needs a Unix socket; not available here.
    pub struct Server;

    impl Server {
        pub fn start(_data_dir: &Path, _proxy: EventLoopProxy<UserEvent>) -> io::Result<Server> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "control by Claude is only available on macOS and Linux",
            ))
        }

        pub fn stop(&mut self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sccm_rc_core::agent::proto::Button;
    use sccm_rc_core::rdp::{KeyboardFlags, MousePdu};
    use std::sync::mpsc;

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn user_input_aborts_running_command() {
        let t0 = Instant::now();
        let mut a = Arbiter::default();
        a.begin_command();
        assert!(!a.should_abort(t0));
        a.on_user_input(ms(t0, 10));
        assert!(a.should_abort(ms(t0, 10)));
        a.end_command();
        assert!(!a.should_abort(ms(t0, 20)), "nothing runs any more");
    }

    #[test]
    fn input_without_a_command_aborts_nothing_later() {
        let t0 = Instant::now();
        let mut a = Arbiter::default();
        a.on_user_input(t0);
        a.begin_command();
        assert!(!a.should_abort(ms(t0, 3000)));
    }

    #[test]
    fn commands_rejected_for_two_seconds() {
        let t0 = Instant::now();
        let mut a = Arbiter::default();
        assert!(a.accepts_commands(t0));
        a.on_user_input(t0);
        assert!(!a.accepts_commands(t0));
        assert!(!a.accepts_commands(ms(t0, 1999)));
    }

    #[test]
    fn accepts_again_after_cooldown() {
        let t0 = Instant::now();
        let mut a = Arbiter::default();
        a.on_user_input(t0);
        assert!(a.accepts_commands(ms(t0, 2000)));
        a.on_user_input(ms(t0, 2500));
        assert!(!a.accepts_commands(ms(t0, 4000)));
        assert!(a.accepts_commands(ms(t0, 4500)));
    }

    /// A session stub that records what a job sends.
    struct Fake {
        w: u16,
        h: u16,
        alive: bool,
        refuse: bool,
        sent: Vec<FastPathInputEvent>,
        audited: Vec<String>,
        watch: FrameWatch,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                w: 100,
                h: 100,
                alive: true,
                refuse: false,
                sent: Vec::new(),
                audited: Vec::new(),
                watch: FrameWatch::new(),
            }
        }
    }

    impl Target for Fake {
        fn desk_size(&self) -> (u16, u16) {
            (self.w, self.h)
        }
        fn shot(&self) -> Option<ShotSpace> {
            None
        }
        fn alive(&self) -> Result<(), Reply> {
            if self.alive {
                Ok(())
            } else {
                Err(exec::err(ErrorCode::NotConnected, "gone"))
            }
        }
        fn send(&mut self, events: Vec<FastPathInputEvent>) -> Result<(), Reply> {
            if self.refuse {
                return Err(exec::session_err("the viewer is in view-only mode"));
            }
            self.sent.extend(events);
            Ok(())
        }
        fn release(&mut self, events: Vec<FastPathInputEvent>) {
            self.sent.extend(events);
        }
        fn watch(&self) -> FrameWatch {
            self.watch.clone()
        }
        fn audit(&mut self, action: &str) {
            self.audited.push(action.into());
        }
    }

    fn click(x: f64, y: f64) -> Command {
        Command::Click {
            x,
            y,
            relative: false,
            button: Button::Left,
            double: false,
        }
    }

    fn key(c: &str) -> Command {
        Command::Key { combo: c.into() }
    }

    fn mouse_flags(ev: &[FastPathInputEvent]) -> Vec<PointerFlags> {
        ev.iter()
            .filter_map(|e| match e {
                FastPathInputEvent::MouseEvent(MousePdu { flags, .. }) => Some(*flags),
                _ => None,
            })
            .collect()
    }

    fn pending(p: Poll) -> Instant {
        match p {
            Poll::Pending(at) => at,
            Poll::Done(r) => panic!("done early: {r:?}"),
        }
    }

    fn done(p: Poll) -> Reply {
        match p {
            Poll::Done(r) => r,
            Poll::Pending(_) => panic!("still pending"),
        }
    }

    #[test]
    fn click_is_paced_not_blocking() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(click(10.0, 20.0));
        let at = pending(j.poll(t0, &mut f));
        assert_eq!(at, ms(t0, 20));
        assert_eq!(
            mouse_flags(&f.sent),
            vec![
                PointerFlags::MOVE,
                PointerFlags::LEFT_BUTTON | PointerFlags::DOWN
            ]
        );
        // Too early: nothing more happens.
        assert_eq!(pending(j.poll(ms(t0, 5), &mut f)), at);
        assert_eq!(f.sent.len(), 2);
        let r = done(j.poll(at, &mut f));
        assert_eq!(r, exec::ok("click at desktop 10,20"));
        assert_eq!(f.sent.len(), 3);
        assert_eq!(f.audited, vec!["click left at 10,20"]);
    }

    #[test]
    fn batch_steps_are_100ms_apart() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(Command::Batch {
            steps: vec![key("a"), key("b"), key("c")],
        });
        assert_eq!(pending(j.poll(t0, &mut f)), ms(t0, 100));
        assert_eq!(f.sent.len(), 2);
        assert_eq!(pending(j.poll(ms(t0, 100), &mut f)), ms(t0, 200));
        match done(j.poll(ms(t0, 200), &mut f)) {
            Reply::Ok { text } => assert!(text.starts_with("batch done: 3 steps"), "{text}"),
            r => panic!("{r:?}"),
        }
        assert_eq!(f.sent.len(), 6);
    }

    #[test]
    fn abort_releases_the_held_button() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(click(1.0, 2.0));
        pending(j.poll(t0, &mut f));
        let r = j.abort(ErrorCode::UserTookOver, MSG_TOOK_OVER, &mut f);
        assert_eq!(
            r,
            Reply::Error {
                code: ErrorCode::UserTookOver,
                message: MSG_TOOK_OVER.into(),
                completed_steps: Some(0),
            }
        );
        assert_eq!(
            mouse_flags(&f.sent).last(),
            Some(&PointerFlags::LEFT_BUTTON),
            "button released"
        );
    }

    #[test]
    fn abort_counts_completed_batch_steps() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(Command::Batch {
            steps: vec![key("a"), key("b"), click(1.0, 1.0)],
        });
        pending(j.poll(t0, &mut f));
        pending(j.poll(ms(t0, 100), &mut f));
        pending(j.poll(ms(t0, 200), &mut f)); // click pressed, waiting for release
        match j.abort(ErrorCode::UserTookOver, MSG_TOOK_OVER, &mut f) {
            Reply::Error {
                completed_steps, ..
            } => assert_eq!(completed_steps, Some(2)),
            r => panic!("{r:?}"),
        }
        assert_eq!(
            mouse_flags(&f.sent).last(),
            Some(&PointerFlags::LEFT_BUTTON)
        );
        let mut h = Held::new();
        h.track(&f.sent);
        assert!(h.is_empty(), "nothing left pressed");
    }

    #[test]
    fn long_text_is_typed_in_paced_chunks() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(Command::TypeText {
            text: "x".repeat(40),
            secret: true,
        });
        assert_eq!(pending(j.poll(t0, &mut f)), ms(t0, 100));
        assert_eq!(f.sent.len(), 64);
        let r = done(j.poll(ms(t0, 100), &mut f));
        assert_eq!(r, exec::ok("typed 40 characters"));
        assert_eq!(f.sent.len(), 80);
        assert_eq!(f.audited, vec!["type_text (secret, 40 chars)"]);
        assert!(matches!(
            f.sent[0],
            FastPathInputEvent::UnicodeKeyboardEvent(_, 0x78)
        ));
    }

    #[test]
    fn batch_error_names_the_step() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(Command::Batch {
            steps: vec![key("enter"), key("nosuchkey"), key("tab")],
        });
        pending(j.poll(t0, &mut f));
        match done(j.poll(ms(t0, 100), &mut f)) {
            Reply::Error {
                code,
                message,
                completed_steps,
            } => {
                assert_eq!(code, ErrorCode::BadArgs);
                assert!(message.starts_with("step 1:"), "{message}");
                assert_eq!(completed_steps, Some(1));
            }
            r => panic!("{r:?}"),
        }
        assert_eq!(f.sent.len(), 2);
    }

    #[test]
    fn refused_input_is_an_error_and_releases() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(click(1.0, 1.0));
        pending(j.poll(t0, &mut f));
        f.refuse = true;
        match done(j.poll(ms(t0, 20), &mut f)) {
            Reply::Error { code, .. } => assert_eq!(code, ErrorCode::Session),
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn wait_polls_until_the_screen_changes() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(Command::Wait {
            change: true,
            stable_ms: None,
            timeout_ms: 1000,
        });
        assert_eq!(pending(j.poll(t0, &mut f)), ms(t0, 50));
        assert_eq!(pending(j.poll(ms(t0, 50), &mut f)), ms(t0, 100));
        f.watch.on_update(ms(t0, 60));
        assert_eq!(done(j.poll(ms(t0, 100), &mut f)), exec::ok("changed"));
    }

    #[test]
    fn wait_times_out_and_session_end_stops_it() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(Command::Wait {
            change: true,
            stable_ms: None,
            timeout_ms: 100,
        });
        pending(j.poll(t0, &mut f));
        assert_eq!(done(j.poll(ms(t0, 100), &mut f)), exec::ok("timeout"));
        let mut j = Job::new(Command::Wait {
            change: true,
            stable_ms: None,
            timeout_ms: 100,
        });
        pending(j.poll(t0, &mut f));
        f.alive = false;
        match done(j.poll(ms(t0, 50), &mut f)) {
            Reply::Error { code, .. } => assert_eq!(code, ErrorCode::NotConnected),
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn out_of_bounds_click_sends_nothing() {
        let mut f = Fake::new();
        let mut j = Job::new(click(500.0, 1.0));
        assert!(matches!(
            done(j.poll(Instant::now(), &mut f)),
            Reply::Error {
                code: ErrorCode::BadArgs,
                ..
            }
        ));
        assert!(f.sent.is_empty() && f.audited.is_empty());
    }

    #[test]
    fn parse_request_checks_and_hides_typed_text() {
        assert!(parse_request(b"  \n").is_none());
        assert!(matches!(
            parse_request(br#"{"cmd":"status"}"#),
            Some(Ok(Command::Status))
        ));
        let bad = parse_request(br#"{"cmd":"type_text","text":12345678,"secret":true}"#);
        match bad {
            Some(Err(Reply::Error { message, .. })) => {
                assert!(!message.contains("12345678"), "{message}")
            }
            r => panic!("{r:?}"),
        }
        assert!(matches!(
            parse_request(br#"{"cmd":"batch","steps":[{"cmd":"status"}]}"#),
            Some(Err(_))
        ));
        assert!(matches!(parse_request(b"\xff\xfe"), Some(Err(_))));
    }

    #[test]
    fn release_bypasses_view_only() {
        let t0 = Instant::now();
        let mut f = Fake::new();
        let mut j = Job::new(click(3.0, 4.0));
        pending(j.poll(t0, &mut f)); // button down
        f.refuse = true; // the user switched view-only on
        j.abort(ErrorCode::ControlDisabled, "off", &mut f);
        assert_eq!(
            mouse_flags(&f.sent).last(),
            Some(&PointerFlags::LEFT_BUTTON)
        );
        // Same when the next send is refused mid-command.
        let mut f = Fake::new();
        let mut j = Job::new(click(3.0, 4.0));
        pending(j.poll(t0, &mut f));
        f.refuse = true;
        done(j.poll(ms(t0, 20), &mut f));
        assert_eq!(
            mouse_flags(&f.sent).last(),
            Some(&PointerFlags::LEFT_BUTTON)
        );
    }

    #[test]
    fn mouse_move_alone_does_not_abort() {
        let mv = exec::mouse(PointerFlags::MOVE, 5, 5);
        assert!(!counts_as_takeover(&mv));
        for ev in [
            exec::mouse(PointerFlags::LEFT_BUTTON | PointerFlags::DOWN, 5, 5),
            exec::mouse(PointerFlags::LEFT_BUTTON, 5, 5),
            exec::mouse(PointerFlags::RIGHT_BUTTON, 5, 5),
            FastPathInputEvent::MouseEvent(MousePdu {
                flags: PointerFlags::VERTICAL_WHEEL,
                number_of_wheel_rotation_units: 120,
                x_position: 0,
                y_position: 0,
            }),
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 0x1E),
        ] {
            assert!(counts_as_takeover(&ev), "{ev:?}");
        }
        // And a move therefore leaves a running command alone.
        let t0 = Instant::now();
        let mut a = Arbiter::default();
        a.begin_command();
        if counts_as_takeover(&mv) {
            a.on_user_input(t0);
        }
        assert!(!a.should_abort(t0));
    }

    #[test]
    fn parse_errors_are_generic() {
        for line in [
            &br#"{"cmd":"nosuch","x":"hunter2"}"#[..],
            br#"{"cmd":"click","x":"hunter2"}"#,
            b"not json",
        ] {
            match parse_request(line) {
                Some(Err(Reply::Error { message, .. })) => assert_eq!(message, "invalid command"),
                r => panic!("{r:?}"),
            }
        }
    }

    #[test]
    fn cmd_name_has_no_arguments() {
        let c = Command::TypeText {
            text: "hunter2".into(),
            secret: false,
        };
        assert_eq!(cmd_name(&c), "type_text");
        assert_eq!(cmd_name(&Command::Batch { steps: vec![c] }), "batch");
    }

    #[test]
    fn ticker_fires_after_the_deadline() {
        let (tx, rx) = mpsc::channel();
        let t = Ticker::new(move || {
            let _ = tx.send(Instant::now());
        });
        let start = Instant::now();
        t.schedule(start + Duration::from_millis(30));
        let fired = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(fired >= start + Duration::from_millis(30));
        drop(t); // joins the thread
    }

    #[test]
    fn key_release_flags_survive_abort() {
        let mut h = Held::new();
        h.track(&[FastPathInputEvent::KeyboardEvent(
            KeyboardFlags::EXTENDED,
            0x5B,
        )]);
        assert_eq!(
            h.release_all(),
            vec![FastPathInputEvent::KeyboardEvent(
                KeyboardFlags::EXTENDED | KeyboardFlags::RELEASE,
                0x5B
            )]
        );
    }
}

#[cfg(all(test, unix))]
mod socket_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::mpsc;

    /// A fresh, short directory (socket paths are limited to ~104 bytes).
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rcctl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Start a server whose "App side" is a thread answering every command.
    fn fake_app(dir: &Path) -> Server {
        let (tx, rx) = mpsc::channel::<(Command, mpsc::Sender<Reply>)>();
        std::thread::spawn(move || {
            for (cmd, reply) in rx {
                let r = match cmd {
                    Command::Status => Reply::Status {
                        connected: true,
                        target: Some("till".into()),
                        desk_w: 800,
                        desk_h: 600,
                        mode: "live".into(),
                        idle_s: 0,
                    },
                    c => Reply::Ok {
                        text: format!("ran {c:?}"),
                    },
                };
                let _ = reply.send(r);
            }
        });
        let tx = std::sync::Mutex::new(tx);
        Server::start_with(dir, move |c, r| tx.lock().unwrap().send((c, r)).is_ok()).unwrap()
    }

    fn ask(s: &mut UnixStream, line: &str) -> Reply {
        s.write_all(line.as_bytes()).unwrap();
        s.write_all(b"\n").unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut out = String::new();
        r.read_line(&mut out).unwrap();
        assert!(out.ends_with('\n'), "{out:?}");
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn control_socket_round_trip() {
        let dir = temp_dir("rt");
        let mut server = fake_app(&dir);
        let mut s = UnixStream::connect(dir.join("control.sock")).unwrap();
        match ask(&mut s, r#"{"cmd":"status"}"#) {
            Reply::Status { mode, desk_w, .. } => {
                assert_eq!(mode, "live");
                assert_eq!(desk_w, 800);
            }
            r => panic!("{r:?}"),
        }
        // Same connection, second command.
        assert!(matches!(
            ask(&mut s, r#"{"cmd":"key","combo":"enter"}"#),
            Reply::Ok { .. }
        ));
        server.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn socket_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perm");
        let mut server = fake_app(&dir);
        let mode = std::fs::metadata(dir.join("control.sock"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        server.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_removes_socket() {
        let dir = temp_dir("stop");
        let mut server = fake_app(&dir);
        let path = dir.join("control.sock");
        let mut s = UnixStream::connect(&path).unwrap();
        server.stop();
        assert!(!path.exists());
        assert!(UnixStream::connect(&path).is_err());
        // The open connection is closed too.
        let mut buf = String::new();
        let n = BufReader::new(&mut s).read_line(&mut buf).unwrap_or(0);
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_server_refuses_a_live_socket() {
        let dir = temp_dir("dup");
        let mut server = fake_app(&dir);
        assert!(Server::start_with(&dir, |_, _| false).is_err());
        server.stop();
        // A stale socket file (nobody listening) is replaced.
        drop(std::os::unix::net::UnixListener::bind(dir.join(SOCKET_NAME)).unwrap());
        assert!(dir.join(SOCKET_NAME).exists());
        let mut again = fake_app(&dir);
        again.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_request_gets_an_error_line() {
        let dir = temp_dir("bad");
        let mut server = fake_app(&dir);
        let mut s = UnixStream::connect(dir.join(SOCKET_NAME)).unwrap();
        assert!(matches!(
            ask(&mut s, "not json"),
            Reply::Error {
                code: ErrorCode::BadArgs,
                ..
            }
        ));
        server.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
