//! In-window logon overlay: `DOMAIN\user` + password, shown after a host is
//! picked when the viewer has no single sign-on (macOS/Linux, NTLM with
//! explicit credentials).
//!
//! Same conventions as `host_prompt`: state only, `draw()` rasterises into
//! the ARGB u32 buffer, the App polls `take_outcome()` in `about_to_wait`.
//!
//! Keyboard: Tab / ↑ / ↓ switch field, Enter in the user field moves to the
//! password, Enter in the password field logs on, Esc goes back to the host
//! picker. The password is never drawn, only masked, and is wiped on drop.

use crate::host_prompt::{draw_left, fill_rect, stroke_rect};
use crate::text::TextRenderer;
use crate::toolbar;
use rust_i18n::t;
use sccm_rc_core::LogonCredentials;

const CARD_W: u32 = 480;
const CARD_H: u32 = 330;
const FIELD_H: u32 = 36;

pub enum LoginOutcome {
    Confirmed {
        target: String,
        creds: LogonCredentials,
    },
    /// Back to the host picker.
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    User,
    Password,
}

pub struct LoginPromptOverlay {
    target: String,
    user: String,
    password: String,
    focus: Field,
    /// Shown in red under the fields (e.g. the last logon was rejected).
    error: Option<String>,
    outcome: Option<LoginOutcome>,
    caret_anchor: std::time::Instant,
}

impl Drop for LoginPromptOverlay {
    fn drop(&mut self) {
        wipe(&mut self.password);
    }
}

fn wipe(s: &mut String) {
    // SAFETY: zero bytes keep the String valid UTF-8.
    unsafe {
        for b in s.as_bytes_mut() {
            std::ptr::write_volatile(b, 0);
        }
    }
    s.clear();
}

/// Room for any realistic password, reserved up front so typing never
/// reallocates and leaves an unwiped copy behind in freed memory.
const PASSWORD_CAP: usize = 256;

/// `String::pop`, but the removed character's bytes are zeroed first, so a
/// backspaced password character does not linger in the spare capacity.
fn pop_wiped(s: &mut String) {
    let Some((idx, _)) = s.char_indices().next_back() else {
        return;
    };
    // SAFETY: `idx` is a char boundary; the zeroed tail is cut off before the
    // String is used again, so it stays valid UTF-8.
    unsafe {
        let v = s.as_mut_vec();
        for b in &mut v[idx..] {
            std::ptr::write_volatile(b, 0);
        }
        v.truncate(idx);
    }
}

impl LoginPromptOverlay {
    /// `user` pre-fills the user field (last used name); the password field
    /// gets focus when a user is already known.
    pub fn new(target: String, user: Option<String>, error: Option<String>) -> Self {
        let user = user.unwrap_or_default();
        let focus = if user.is_empty() {
            Field::User
        } else {
            Field::Password
        };
        Self {
            target,
            user,
            password: String::with_capacity(PASSWORD_CAP),
            focus,
            error,
            outcome: None,
            caret_anchor: std::time::Instant::now(),
        }
    }

    fn field_mut(&mut self) -> &mut String {
        match self.focus {
            Field::User => &mut self.user,
            Field::Password => &mut self.password,
        }
    }

    fn bump(&mut self) {
        self.caret_anchor = std::time::Instant::now();
    }

    pub fn on_text(&mut self, s: &str) {
        let field = self.field_mut();
        for c in s.chars().filter(|c| !c.is_control()) {
            field.push(c);
        }
        self.bump();
    }

    pub fn on_backspace(&mut self) {
        match self.focus {
            Field::User => {
                self.user.pop();
            }
            Field::Password => pop_wiped(&mut self.password),
        }
        self.bump();
    }

    pub fn on_clear(&mut self) {
        match self.focus {
            Field::User => self.user.clear(),
            Field::Password => wipe(&mut self.password),
        }
        self.bump();
    }

    pub fn on_tab(&mut self) {
        self.focus = match self.focus {
            Field::User => Field::Password,
            Field::Password => Field::User,
        };
        self.bump();
    }

    pub fn on_enter(&mut self) {
        if self.focus == Field::User {
            if !self.user.trim().is_empty() {
                self.focus = Field::Password;
                self.bump();
            }
            return;
        }
        if self.user.trim().is_empty() {
            self.focus = Field::User;
            self.error = Some(t!("login.need_user").to_string());
            return;
        }
        if self.password.is_empty() {
            self.error = Some(t!("login.need_password").to_string());
            return;
        }
        match LogonCredentials::parse(&self.user, None, &self.password) {
            Ok(creds) => {
                wipe(&mut self.password);
                self.outcome = Some(LoginOutcome::Confirmed {
                    target: self.target.clone(),
                    creds,
                });
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    pub fn on_esc(&mut self) {
        self.outcome = Some(LoginOutcome::Cancelled);
    }

    /// Left-click focuses the field under the cursor.
    pub fn on_click(&mut self, x: f64, y: f64, win_w: u32, win_h: u32) {
        let l = layout(win_w, win_h);
        if l.user.contains(x, y) {
            self.focus = Field::User;
            self.bump();
        } else if l.password.contains(x, y) {
            self.focus = Field::Password;
            self.bump();
        }
    }

    pub fn take_outcome(&mut self) -> Option<LoginOutcome> {
        self.outcome.take()
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < self.x as f64 + self.w as f64
            && y < self.y as f64 + self.h as f64
    }
}

struct Layout {
    card_x: i32,
    card_y: i32,
    user: Rect,
    password: Rect,
}

fn layout(win_w: u32, win_h: u32) -> Layout {
    let card_x = (win_w / 2) as i32 - (CARD_W as i32) / 2;
    let card_y = (win_h / 2) as i32 - (CARD_H as i32) / 2;
    let fx = card_x + 24;
    let fw = CARD_W - 48;
    Layout {
        card_x,
        card_y,
        user: Rect {
            x: fx,
            y: card_y + 96,
            w: fw,
            h: FIELD_H,
        },
        password: Rect {
            x: fx,
            y: card_y + 176,
            w: fw,
            h: FIELD_H,
        },
    }
}

pub fn draw(buf: &mut [u32], w: u32, h: u32, font: Option<&TextRenderer>, o: &LoginPromptOverlay) {
    for px in buf.iter_mut() {
        *px = 0x0020_2020;
    }
    let l = layout(w, h);
    fill_rect(buf, w, h, l.card_x, l.card_y, CARD_W, CARD_H, 0x002E_343C);
    stroke_rect(buf, w, h, l.card_x, l.card_y, CARD_W, CARD_H, 0x0050_5860);

    let title = t!("login.title", target => o.target);
    match font {
        Some(f) => f.draw_centered(buf, w, h, (l.card_y + 40) as f32, &title, 0x00E4_EAF0, 22.0),
        None => {
            toolbar::draw_text_centered(buf, w, h, (l.card_y + 32) as u32, &title, 0x00E4_EAF0, 2)
        }
    }

    let label_col = 0x00A0_A8B0;
    let user_label = t!("login.user");
    let pass_label = t!("login.password");
    draw_left(
        buf,
        w,
        h,
        l.user.x as f32,
        (l.user.y - 26) as f32,
        22,
        &user_label,
        label_col,
        15.0,
        font,
    );
    draw_left(
        buf,
        w,
        h,
        l.password.x as f32,
        (l.password.y - 26) as f32,
        22,
        &pass_label,
        label_col,
        15.0,
        font,
    );

    let masked: String = "*".repeat(o.password.chars().count());
    for (rect, text, field) in [
        (l.user, o.user.as_str(), Field::User),
        (l.password, masked.as_str(), Field::Password),
    ] {
        fill_rect(buf, w, h, rect.x, rect.y, rect.w, rect.h, 0x001B_1E23);
        let border = if o.focus == field {
            0x0060_88B8
        } else {
            0x0045_4C55
        };
        stroke_rect(buf, w, h, rect.x, rect.y, rect.w, rect.h, border);
        let tx = (rect.x + 10) as f32;
        if text.is_empty() && field == Field::User && o.focus != Field::User {
            let ph = t!("login.user_placeholder");
            draw_left(
                buf,
                w,
                h,
                tx,
                (rect.y + 6) as f32,
                rect.h - 12,
                &ph,
                0x0070_7880,
                18.0,
                font,
            );
        } else {
            draw_left(
                buf,
                w,
                h,
                tx,
                (rect.y + 6) as f32,
                rect.h - 12,
                text,
                0x00E4_EAF0,
                18.0,
                font,
            );
        }
        if o.focus == field && (o.caret_anchor.elapsed().as_millis() / 500).is_multiple_of(2) {
            let tw = if text.is_empty() {
                0.0
            } else {
                font.map(|f| f.width(text, 18.0))
                    .unwrap_or(text.chars().count() as f32 * 8.0)
            };
            fill_rect(
                buf,
                w,
                h,
                (tx + tw) as i32,
                rect.y + 8,
                2,
                (rect.h - 16).max(1),
                0x00E4_EAF0,
            );
        }
    }

    if let Some(err) = &o.error {
        draw_left(
            buf,
            w,
            h,
            l.password.x as f32,
            (l.password.y + FIELD_H as i32 + 10) as f32,
            22,
            err,
            0x00FF_6B6B,
            15.0,
            font,
        );
    }

    let note = t!("login.ntlm_note");
    let hint = t!("login.hint");
    let bottom = l.card_y + CARD_H as i32;
    match font {
        Some(f) => {
            f.draw_centered(buf, w, h, (bottom - 44) as f32, &note, 0x0070_7880, 13.0);
            f.draw_centered(buf, w, h, (bottom - 20) as f32, &hint, 0x0070_7880, 14.0);
        }
        None => {
            toolbar::draw_text_centered(buf, w, h, (bottom - 50) as u32, &note, 0x0070_7880, 1);
            toolbar::draw_text_centered(buf, w, h, (bottom - 28) as u32, &hint, 0x0070_7880, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirmed(o: &mut LoginPromptOverlay) -> Option<(String, LogonCredentials)> {
        match o.take_outcome() {
            Some(LoginOutcome::Confirmed { target, creds }) => Some((target, creds)),
            _ => None,
        }
    }

    #[test]
    fn enter_flow_user_then_password() {
        let mut o = LoginPromptOverlay::new("192.0.2.10".into(), None, None);
        o.on_text("CORP\\alice");
        o.on_enter(); // moves to password
        assert!(o.take_outcome().is_none());
        o.on_text("pw");
        o.on_enter();
        let (target, c) = confirmed(&mut o).expect("confirmed");
        assert_eq!(target, "192.0.2.10");
        assert_eq!(
            (c.domain.as_str(), c.user.as_str(), c.password.as_str()),
            ("CORP", "alice", "pw")
        );
        assert!(
            o.password.is_empty(),
            "password must be wiped after confirm"
        );
    }

    #[test]
    fn prefilled_user_focuses_password() {
        let mut o = LoginPromptOverlay::new("h".into(), Some("D\\u".into()), None);
        o.on_text("secret");
        o.on_enter();
        assert!(confirmed(&mut o).is_some());
    }

    #[test]
    fn empty_password_is_not_submitted() {
        let mut o = LoginPromptOverlay::new("h".into(), Some("D\\u".into()), None);
        o.on_enter();
        assert!(o.take_outcome().is_none());
        assert!(o.error.is_some());
    }

    #[test]
    fn backspace_zeroes_the_removed_password_bytes() {
        let mut o = LoginPromptOverlay::new("h".into(), Some("D\\u".into()), None);
        o.on_text("ab\u{e9}");
        o.on_backspace();
        assert_eq!(o.password, "ab");
        // The removed 'é' (2 bytes) sits in spare capacity: it must be zeroed.
        let spare = unsafe { std::slice::from_raw_parts(o.password.as_ptr().add(2), 2) };
        assert_eq!(spare, &[0, 0]);
    }

    #[test]
    fn tab_switches_fields_and_esc_cancels() {
        let mut o = LoginPromptOverlay::new("h".into(), None, None);
        o.on_tab();
        o.on_text("x");
        assert_eq!(o.password, "x");
        assert!(o.user.is_empty());
        o.on_esc();
        assert!(matches!(o.take_outcome(), Some(LoginOutcome::Cancelled)));
    }
}
