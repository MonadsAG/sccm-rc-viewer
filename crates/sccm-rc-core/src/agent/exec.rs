//! Command semantics shared by the headless backend and the viewer's control
//! socket: point mapping, the input each command sends (as a plan the caller
//! paces itself) and the replies around it.

use std::time::Duration;

use crate::agent::proto::{Button, Command, ErrorCode, Reply};
use crate::agent::shot::{self, ShotError, ShotSpace};
use crate::agent::wait::WaitResult;
use crate::agent::{keys, text};
use crate::rdp::{FastPathInputEvent, KeyboardFlags, MousePdu, PointerFlags};

/// Pause between the steps of a batch.
pub const STEP_GAP: Duration = Duration::from_millis(100);
pub const DEFAULT_MAX_WIDTH: u32 = 1280;
/// Longest `wait` a client may ask for.
pub const MAX_WAIT_MS: u64 = 5 * 60 * 1000;
pub const MAX_SCROLL: i16 = 50;
/// One wheel notch in RDP rotation units.
const WHEEL_NOTCH: i16 = 120;
/// Text events per input message.
const TEXT_CHUNK: usize = 64;

/// One piece of an input plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Input(Vec<FastPathInputEvent>),
    Pause(Duration),
    /// A point where a live executor lets the user's own input in (between
    /// text chunks); the headless backend goes on at once.
    Yield,
}

/// What an input command sends, and what to log and reply.
#[derive(Debug, Clone, PartialEq)]
pub struct InputPlan {
    /// Audit line; never contains secret text.
    pub audit: String,
    pub chunks: Vec<Chunk>,
    /// Reply text once every chunk was sent; never contains secret text.
    pub done: String,
}

pub fn ok(text: impl Into<String>) -> Reply {
    Reply::Ok { text: text.into() }
}

pub fn err(code: ErrorCode, message: impl Into<String>) -> Reply {
    Reply::Error {
        code,
        message: message.into(),
        completed_steps: None,
    }
}

pub fn bad_args(m: impl Into<String>) -> Reply {
    err(ErrorCode::BadArgs, m)
}

pub fn session_err(m: impl Into<String>) -> Reply {
    err(ErrorCode::Session, m)
}

/// Input refused in a view-only session (the viewer's view-only lock, or a
/// till that grants no control). Same reply in live and headless mode.
pub const MSG_VIEW_ONLY: &str = "the viewer is in view-only mode";

pub fn view_only() -> Reply {
    session_err(MSG_VIEW_ONLY)
}

/// Whether `cmd` sends input to the till (an input command, or a batch,
/// which may hold any): refused in view-only sessions. `wait` sends nothing.
pub fn sends_input(cmd: &Command) -> bool {
    !matches!(
        cmd,
        Command::Status | Command::Screenshot { .. } | Command::Wait { .. } | Command::Disconnect
    )
}

pub fn no_image() -> Reply {
    session_err("no desktop image yet; use wait (change=true) and try again")
}

pub fn mouse(flags: PointerFlags, x: u16, y: u16) -> FastPathInputEvent {
    FastPathInputEvent::MouseEvent(MousePdu {
        flags,
        number_of_wheel_rotation_units: 0,
        x_position: x,
        y_position: y,
    })
}

/// Map a point from the client's coordinate space to desktop pixels: the last
/// screenshot's image space, fractions of the desktop when `relative`, or
/// desktop pixels before any screenshot.
pub fn map_point(
    x: f64,
    y: f64,
    relative: bool,
    shot: Option<&ShotSpace>,
    w: u16,
    h: u16,
) -> Result<(u16, u16), Reply> {
    if w == 0 || h == 0 {
        return Err(no_image());
    }
    let r = if relative {
        shot::relative_to_desktop(x, y, w, h)
    } else if let Some(s) = shot {
        s.to_desktop_checked(x, y, w, h)
    } else if !(x.is_finite() && y.is_finite())
        || x < 0.0
        || y < 0.0
        || x > (w - 1) as f64
        || y > (h - 1) as f64
    {
        Err(ShotError::OutOfBounds)
    } else {
        Ok((x.round() as u16, y.round() as u16))
    };
    r.map_err(|e| match e {
        ShotError::StaleShot => bad_args(format!("{e}; take a new screenshot")),
        e => bad_args(e.to_string()),
    })
}

/// Checks before a command runs: `validate`, and the steps a batch may hold.
pub fn check(cmd: &Command) -> Result<(), Reply> {
    cmd.validate().map_err(bad_args)?;
    if let Command::Batch { steps } = cmd {
        if steps.iter().any(|s| {
            matches!(
                s,
                Command::Screenshot { .. } | Command::Status | Command::Disconnect
            )
        }) {
            return Err(bad_args(
                "screenshot/status/disconnect are not allowed in batch",
            ));
        }
    }
    Ok(())
}

pub fn check_wait(timeout_ms: u64) -> Result<(), Reply> {
    if timeout_ms > MAX_WAIT_MS {
        return Err(bad_args(format!(
            "timeout_ms must be at most {MAX_WAIT_MS}"
        )));
    }
    Ok(())
}

pub fn wait_reply(r: WaitResult) -> Reply {
    ok(match r {
        WaitResult::Changed => "changed",
        WaitResult::Stable => "stable",
        WaitResult::Timeout => "timeout",
    })
}

pub fn screenshot_audit(region: Option<shot::Rect>) -> String {
    match region {
        Some(r) => format!("screenshot region {},{} {}x{}", r.x, r.y, r.w, r.h),
        None => "screenshot".into(),
    }
}

/// A batch step failed: its error, numbered, with the steps done before it.
pub fn step_failed(i: usize, r: Reply) -> Reply {
    match r {
        Reply::Error { code, message, .. } => Reply::Error {
            code,
            message: format!("step {i}: {message}"),
            completed_steps: Some(i),
        },
        r => r,
    }
}

pub fn batch_done(n: usize, notes: &[String]) -> Reply {
    ok(format!("batch done: {n} steps ({})", notes.join("; ")))
}

/// The input plan of click, move, scroll, type_text and key; `None` for the
/// other commands. `map` turns client coordinates into desktop pixels.
pub fn plan_input(
    cmd: &Command,
    map: impl FnOnce(f64, f64, bool) -> Result<(u16, u16), Reply>,
) -> Result<Option<InputPlan>, Reply> {
    Ok(Some(match cmd {
        Command::Click {
            x,
            y,
            relative,
            button,
            double,
        } => {
            let (dx, dy) = map(*x, *y, *relative)?;
            let b = match button {
                Button::Left => PointerFlags::LEFT_BUTTON,
                Button::Right => PointerFlags::RIGHT_BUTTON,
                Button::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
            };
            let name = format!("{button:?}").to_lowercase();
            let kind = if *double { "double-click" } else { "click" };
            let mut chunks = vec![Chunk::Input(vec![mouse(PointerFlags::MOVE, dx, dy)])];
            for i in 0..if *double { 2 } else { 1 } {
                if i > 0 {
                    chunks.push(Chunk::Pause(Duration::from_millis(50)));
                }
                chunks.push(Chunk::Input(vec![mouse(b | PointerFlags::DOWN, dx, dy)]));
                chunks.push(Chunk::Pause(Duration::from_millis(20)));
                chunks.push(Chunk::Input(vec![mouse(b, dx, dy)]));
            }
            InputPlan {
                audit: format!("{kind} {name} at {dx},{dy}"),
                chunks,
                done: format!("{kind} at desktop {dx},{dy}"),
            }
        }
        Command::Move { x, y, relative } => {
            let (dx, dy) = map(*x, *y, *relative)?;
            InputPlan {
                audit: format!("move to {dx},{dy}"),
                chunks: vec![Chunk::Input(vec![mouse(PointerFlags::MOVE, dx, dy)])],
                done: format!("moved to desktop {dx},{dy}"),
            }
        }
        Command::Scroll {
            x,
            y,
            amount,
            relative,
        } => {
            let amount = *amount;
            if amount == 0 || amount.abs() > MAX_SCROLL {
                return Err(bad_args(format!(
                    "amount must be between -{MAX_SCROLL} and {MAX_SCROLL} and not 0"
                )));
            }
            let (dx, dy) = map(*x, *y, *relative)?;
            let units = if amount > 0 {
                WHEEL_NOTCH
            } else {
                -WHEEL_NOTCH
            };
            let mut ev = vec![mouse(PointerFlags::MOVE, dx, dy)];
            ev.extend((0..amount.unsigned_abs()).map(|_| {
                FastPathInputEvent::MouseEvent(MousePdu {
                    flags: PointerFlags::VERTICAL_WHEEL,
                    number_of_wheel_rotation_units: units,
                    x_position: dx,
                    y_position: dy,
                })
            }));
            InputPlan {
                audit: format!("scroll {amount} at {dx},{dy}"),
                chunks: vec![Chunk::Input(ev)],
                done: format!("scrolled {amount} at desktop {dx},{dy}"),
            }
        }
        Command::TypeText { text, secret } => {
            let n = text.chars().count();
            let (events, skipped) = text::encode_text(text);
            let mut chunks = Vec::new();
            for (i, c) in events.chunks(TEXT_CHUNK).enumerate() {
                if i > 0 {
                    chunks.push(Chunk::Yield);
                }
                chunks.push(Chunk::Input(c.to_vec()));
            }
            let mut done = format!("typed {} characters", n - skipped.len());
            if !skipped.is_empty() {
                if *secret {
                    done.push_str(&format!("; skipped {} that cannot be sent", skipped.len()));
                } else {
                    done.push_str(&format!(
                        "; skipped {} that cannot be sent: {skipped:?}",
                        skipped.len()
                    ));
                }
            }
            InputPlan {
                audit: if *secret {
                    format!("type_text (secret, {n} chars)")
                } else {
                    format!("type_text {text:?}")
                },
                chunks,
                done,
            }
        }
        Command::Key { combo } => {
            let k = keys::parse_combo(combo).map_err(|e| bad_args(e.to_string()))?;
            InputPlan {
                audit: format!("key {combo}"),
                chunks: vec![Chunk::Input(keys::press_combo(&k))],
                done: format!("pressed {combo}"),
            }
        }
        _ => return Ok(None),
    }))
}

/// Keys and mouse buttons that sent input left pressed, so an aborted
/// command can let go of them.
#[derive(Debug, Default, Clone)]
pub struct Held {
    keys: Vec<(u8, bool)>,
    unicode: Vec<u16>,
    buttons: Vec<PointerFlags>,
    pos: (u16, u16),
}

const BUTTONS: [PointerFlags; 3] = [
    PointerFlags::LEFT_BUTTON,
    PointerFlags::RIGHT_BUTTON,
    PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
];

impl Held {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn track(&mut self, events: &[FastPathInputEvent]) {
        for e in events {
            match e {
                FastPathInputEvent::KeyboardEvent(f, sc) => {
                    let k = (*sc, f.contains(KeyboardFlags::EXTENDED));
                    self.keys.retain(|x| *x != k);
                    if !f.contains(KeyboardFlags::RELEASE) {
                        self.keys.push(k);
                    }
                }
                FastPathInputEvent::UnicodeKeyboardEvent(f, c) => {
                    self.unicode.retain(|x| x != c);
                    if !f.contains(KeyboardFlags::RELEASE) {
                        self.unicode.push(*c);
                    }
                }
                FastPathInputEvent::MouseEvent(m) => {
                    if m.flags
                        .intersects(PointerFlags::VERTICAL_WHEEL | PointerFlags::HORIZONTAL_WHEEL)
                    {
                        continue;
                    }
                    self.pos = (m.x_position, m.y_position);
                    for b in BUTTONS {
                        if m.flags.contains(b) {
                            self.buttons.retain(|x| *x != b);
                            if m.flags.contains(PointerFlags::DOWN) {
                                self.buttons.push(b);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.unicode.is_empty() && self.buttons.is_empty()
    }

    /// Release events for everything still held (last pressed first), and
    /// forget it.
    pub fn release_all(&mut self) -> Vec<FastPathInputEvent> {
        let mut out = Vec::new();
        for b in self.buttons.drain(..).rev() {
            out.push(mouse(b, self.pos.0, self.pos.1));
        }
        for c in self.unicode.drain(..).rev() {
            out.push(FastPathInputEvent::UnicodeKeyboardEvent(
                KeyboardFlags::RELEASE,
                c,
            ));
        }
        for (sc, ext) in self.keys.drain(..).rev() {
            let mut f = KeyboardFlags::RELEASE;
            if ext {
                f |= KeyboardFlags::EXTENDED;
            }
            out.push(FastPathInputEvent::KeyboardEvent(f, sc));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(x: f64, y: f64, _r: bool) -> Result<(u16, u16), Reply> {
        Ok((x as u16, y as u16))
    }

    fn inputs(p: &InputPlan) -> Vec<FastPathInputEvent> {
        p.chunks
            .iter()
            .flat_map(|c| match c {
                Chunk::Input(e) => e.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn click_plan_moves_then_presses_with_pauses() {
        let p = plan_input(
            &Command::Click {
                x: 5.0,
                y: 6.0,
                relative: false,
                button: Button::Right,
                double: false,
            },
            px,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            p.chunks,
            vec![
                Chunk::Input(vec![mouse(PointerFlags::MOVE, 5, 6)]),
                Chunk::Input(vec![mouse(
                    PointerFlags::RIGHT_BUTTON | PointerFlags::DOWN,
                    5,
                    6
                )]),
                Chunk::Pause(Duration::from_millis(20)),
                Chunk::Input(vec![mouse(PointerFlags::RIGHT_BUTTON, 5, 6)]),
            ]
        );
        assert_eq!(p.audit, "click right at 5,6");
        assert_eq!(p.done, "click at desktop 5,6");
    }

    #[test]
    fn double_click_pauses_between_clicks() {
        let p = plan_input(
            &Command::Click {
                x: 1.0,
                y: 1.0,
                relative: false,
                button: Button::Left,
                double: true,
            },
            px,
        )
        .unwrap()
        .unwrap();
        assert_eq!(inputs(&p).len(), 5);
        assert!(p.chunks.contains(&Chunk::Pause(Duration::from_millis(50))));
    }

    #[test]
    fn secret_text_never_in_audit_or_reply() {
        let p = plan_input(
            &Command::TypeText {
                text: "hunter2😀".into(),
                secret: true,
            },
            px,
        )
        .unwrap()
        .unwrap();
        assert_eq!(p.audit, "type_text (secret, 8 chars)");
        assert!(
            !p.done.contains("hunter2") && !p.done.contains('😀'),
            "{}",
            p.done
        );
        assert_eq!(inputs(&p).len(), 14);
    }

    #[test]
    fn long_text_yields_between_chunks() {
        let p = plan_input(
            &Command::TypeText {
                text: "a".repeat(40),
                secret: false,
            },
            px,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            p.chunks.iter().filter(|c| **c == Chunk::Yield).count(),
            1,
            "80 events = 2 chunks"
        );
    }

    #[test]
    fn scroll_amount_checked_before_mapping() {
        let r = plan_input(
            &Command::Scroll {
                x: 0.0,
                y: 0.0,
                amount: 0,
                relative: false,
            },
            |_, _, _| panic!("mapped"),
        );
        assert!(matches!(
            r,
            Err(Reply::Error {
                code: ErrorCode::BadArgs,
                ..
            })
        ));
    }

    #[test]
    fn non_input_commands_have_no_plan() {
        for c in [
            Command::Status,
            Command::Disconnect,
            Command::Wait {
                change: true,
                stable_ms: None,
                timeout_ms: 1,
            },
        ] {
            assert_eq!(plan_input(&c, px).unwrap(), None);
        }
    }

    #[test]
    fn map_point_without_screenshot_uses_desktop_pixels() {
        assert_eq!(map_point(99.0, 49.0, false, None, 100, 50), Ok((99, 49)));
        assert!(map_point(100.0, 0.0, false, None, 100, 50).is_err());
        assert!(map_point(0.0, 0.0, false, None, 0, 0).is_err());
        assert_eq!(map_point(1.0, 1.0, true, None, 101, 51), Ok((100, 50)));
    }

    #[test]
    fn batch_check_rejects_screenshot_status_disconnect() {
        for s in [
            Command::Status,
            Command::Disconnect,
            Command::Screenshot {
                region: None,
                max_width: None,
            },
        ] {
            assert!(check(&Command::Batch { steps: vec![s] }).is_err());
        }
        assert!(check(&Command::Batch { steps: vec![] }).is_err());
        assert!(check(&Command::Batch {
            steps: vec![Command::Key { combo: "a".into() }]
        })
        .is_ok());
    }

    #[test]
    fn view_only_refuses_input_and_batches_only() {
        for c in [
            Command::Status,
            Command::Disconnect,
            Command::Screenshot {
                region: None,
                max_width: None,
            },
            Command::Wait {
                change: true,
                stable_ms: None,
                timeout_ms: 1,
            },
        ] {
            assert!(!sends_input(&c), "{c:?}");
        }
        for c in [
            Command::Key { combo: "a".into() },
            Command::Move {
                x: 1.0,
                y: 1.0,
                relative: false,
            },
            Command::Batch { steps: vec![] },
        ] {
            assert!(sends_input(&c), "{c:?}");
        }
        assert_eq!(
            view_only(),
            err(ErrorCode::Session, "the viewer is in view-only mode")
        );
    }

    #[test]
    fn held_releases_what_is_still_pressed() {
        let mut h = Held::new();
        h.track(&[
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 0x1D),
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED, 0x53),
            FastPathInputEvent::KeyboardEvent(
                KeyboardFlags::EXTENDED | KeyboardFlags::RELEASE,
                0x53,
            ),
            mouse(PointerFlags::LEFT_BUTTON | PointerFlags::DOWN, 7, 8),
            FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), 0x61),
        ]);
        assert!(!h.is_empty());
        assert_eq!(
            h.release_all(),
            vec![
                mouse(PointerFlags::LEFT_BUTTON, 7, 8),
                FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::RELEASE, 0x61),
                FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 0x1D),
            ]
        );
        assert!(h.is_empty());
        assert!(h.release_all().is_empty());
    }

    #[test]
    fn held_is_empty_after_complete_plans() {
        let mut h = Held::new();
        for c in [
            Command::Click {
                x: 1.0,
                y: 1.0,
                relative: false,
                button: Button::Middle,
                double: true,
            },
            Command::Key {
                combo: "ctrl+alt+del".into(),
            },
            Command::TypeText {
                text: "Grüezi\n".into(),
                secret: false,
            },
            Command::Scroll {
                x: 1.0,
                y: 1.0,
                amount: 3,
                relative: false,
            },
        ] {
            h.track(&inputs(&plan_input(&c, px).unwrap().unwrap()));
        }
        assert!(h.is_empty());
    }
}
