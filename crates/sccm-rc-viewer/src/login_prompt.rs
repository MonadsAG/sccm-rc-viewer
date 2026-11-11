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
//!
//! With [`LoginPromptOverlay::with_keychain`] (macOS) the card shows a "save
//! password" checkbox (Space or click toggles it) and fills in a saved
//! password when the user field holds a known account. Editing the user
//! field drops a filled-in password; typing in the password field replaces it.

use crate::host_prompt::{draw_left, fill_rect, stroke_rect};
use crate::keychain;
use crate::text::TextRenderer;
use crate::toolbar;
use rust_i18n::t;
use sccm_rc_core::LogonCredentials;

const CARD_W: u32 = 480;
const CARD_H: u32 = 330;
/// Extra card height for the "save password" row.
const SAVE_ROW_H: u32 = 40;
const FIELD_H: u32 = 36;
const CHECK: u32 = 18;

pub enum LoginOutcome {
    Confirmed {
        target: String,
        creds: LogonCredentials,
        /// The "save password" box was ticked.
        remember: bool,
        /// The password is the one filled in from the Keychain, unchanged.
        from_keychain: bool,
    },
    /// Back to the host picker.
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    User,
    Password,
    /// The "save password" checkbox (only with a Keychain).
    Save,
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
    /// Keychain lookup; `None` hides the checkbox.
    lookup: Option<keychain::Lookup>,
    remember: bool,
    /// `password` holds the saved password from the Keychain.
    from_keychain: bool,
    /// User name the Keychain was last asked about (asked once per name).
    looked_up: String,
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
            lookup: None,
            remember: false,
            from_keychain: false,
            looked_up: String::new(),
        }
    }

    /// Offer the "save password" checkbox, backed by `lookup`. With `fill`,
    /// a saved password for the pre-filled user is filled in right away
    /// (off after a rejected logon, so the user types it again). `remember`
    /// is the initial state of the checkbox.
    pub fn with_keychain(mut self, lookup: keychain::Lookup, fill: bool, remember: bool) -> Self {
        self.lookup = Some(lookup);
        self.remember = remember;
        if fill {
            self.fill_from_keychain();
        } else {
            // Don't ask again for this user until the name changes.
            self.looked_up = self.user.trim().to_string();
        }
        self
    }

    /// Fill in the saved password for the current user, if the password
    /// field is empty and the Keychain was not asked about this name yet.
    fn fill_from_keychain(&mut self) {
        let Some(lookup) = self.lookup else { return };
        let user = self.user.trim().to_string();
        if user.is_empty() || !self.password.is_empty() || user == self.looked_up {
            return;
        }
        self.looked_up = user;
        if let Some(mut saved) = lookup(&self.looked_up) {
            self.password.push_str(&saved);
            wipe(&mut saved);
            self.from_keychain = true;
            self.remember = true;
        }
    }

    /// The user name changed: a filled-in password belongs to the old name,
    /// and the new name is worth asking the Keychain about.
    fn user_edited(&mut self) {
        self.looked_up.clear();
        if self.from_keychain {
            wipe(&mut self.password);
            self.from_keychain = false;
        }
    }

    /// The user starts editing a filled-in password: replace it, don't append.
    fn password_edited(&mut self) {
        if self.from_keychain {
            wipe(&mut self.password);
            self.from_keychain = false;
        }
    }

    fn has_save(&self) -> bool {
        self.lookup.is_some()
    }

    fn bump(&mut self) {
        self.caret_anchor = std::time::Instant::now();
    }

    pub fn on_text(&mut self, s: &str) {
        let chars = s.chars().filter(|c| !c.is_control());
        match self.focus {
            Field::User => {
                self.user_edited();
                self.user.extend(chars);
            }
            Field::Password => {
                self.password_edited();
                for c in chars {
                    self.password.push(c);
                }
            }
            Field::Save => {
                if s == " " {
                    self.remember = !self.remember;
                }
            }
        }
        self.bump();
    }

    pub fn on_backspace(&mut self) {
        match self.focus {
            Field::User => {
                self.user_edited();
                self.user.pop();
            }
            // Backspace on a filled-in password clears it as a whole.
            Field::Password if self.from_keychain => self.password_edited(),
            Field::Password => pop_wiped(&mut self.password),
            Field::Save => {}
        }
        self.bump();
    }

    pub fn on_clear(&mut self) {
        match self.focus {
            Field::User => {
                self.user_edited();
                self.user.clear();
            }
            Field::Password => {
                self.from_keychain = false;
                wipe(&mut self.password);
            }
            Field::Save => {}
        }
        self.bump();
    }

    fn set_focus(&mut self, f: Field) {
        if self.focus == Field::User && f != Field::User {
            self.fill_from_keychain();
        }
        self.focus = f;
        self.bump();
    }

    pub fn on_tab(&mut self) {
        let next = match self.focus {
            Field::User => Field::Password,
            Field::Password if self.has_save() => Field::Save,
            Field::Password | Field::Save => Field::User,
        };
        self.set_focus(next);
    }

    pub fn on_enter(&mut self) {
        if self.focus == Field::User {
            if !self.user.trim().is_empty() {
                self.set_focus(Field::Password);
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
                    remember: self.has_save() && self.remember,
                    from_keychain: self.from_keychain,
                });
                self.from_keychain = false;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    pub fn on_esc(&mut self) {
        self.outcome = Some(LoginOutcome::Cancelled);
    }

    /// Left-click focuses the field under the cursor; a click on the
    /// checkbox row toggles it.
    pub fn on_click(&mut self, x: f64, y: f64, win_w: u32, win_h: u32) {
        let l = layout(win_w, win_h, self.has_save());
        if l.user.contains(x, y) {
            self.set_focus(Field::User);
        } else if l.password.contains(x, y) {
            self.set_focus(Field::Password);
        } else if self.has_save() && l.save.contains(x, y) {
            self.set_focus(Field::Save);
            self.remember = !self.remember;
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
    card_h: u32,
    user: Rect,
    password: Rect,
    /// The checkbox row (box + label); clickable as a whole.
    save: Rect,
    /// Top of the error line.
    error_y: i32,
}

fn layout(win_w: u32, win_h: u32, with_save: bool) -> Layout {
    let card_h = CARD_H + if with_save { SAVE_ROW_H } else { 0 };
    let card_x = (win_w / 2) as i32 - (CARD_W as i32) / 2;
    let card_y = (win_h / 2) as i32 - (card_h as i32) / 2;
    let fx = card_x + 24;
    let fw = CARD_W - 48;
    let below_password = card_y + 176 + FIELD_H as i32 + 10;
    Layout {
        card_x,
        card_y,
        card_h,
        save: Rect {
            x: fx,
            y: below_password,
            w: fw,
            h: CHECK + 8,
        },
        error_y: below_password + if with_save { SAVE_ROW_H as i32 } else { 0 },
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
    let l = layout(w, h, o.has_save());
    fill_rect(buf, w, h, l.card_x, l.card_y, CARD_W, l.card_h, 0x002E_343C);
    stroke_rect(buf, w, h, l.card_x, l.card_y, CARD_W, l.card_h, 0x0050_5860);

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

    if o.has_save() {
        let r = l.save;
        let by = r.y + 4;
        let border = if o.focus == Field::Save {
            0x0060_88B8
        } else {
            0x0070_7880
        };
        fill_rect(buf, w, h, r.x, by, CHECK, CHECK, 0x001B_1E23);
        stroke_rect(buf, w, h, r.x, by, CHECK, CHECK, border);
        if o.remember {
            fill_rect(
                buf,
                w,
                h,
                r.x + 4,
                by + 4,
                CHECK - 8,
                CHECK - 8,
                0x0060_88B8,
            );
        }
        let label = t!("login.save_password");
        draw_left(
            buf,
            w,
            h,
            (r.x + CHECK as i32 + 10) as f32,
            r.y as f32,
            22,
            &label,
            0x00C8_D0D8,
            15.0,
            font,
        );
    }

    if let Some(err) = &o.error {
        draw_left(
            buf,
            w,
            h,
            l.password.x as f32,
            l.error_y as f32,
            22,
            err,
            0x00FF_6B6B,
            15.0,
            font,
        );
    }

    let note = t!("login.ntlm_note");
    let hint = t!("login.hint");
    let bottom = l.card_y + l.card_h as i32;
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
        confirmed_full(o).map(|(t, c, _, _)| (t, c))
    }

    fn confirmed_full(
        o: &mut LoginPromptOverlay,
    ) -> Option<(String, LogonCredentials, bool, bool)> {
        match o.take_outcome() {
            Some(LoginOutcome::Confirmed {
                target,
                creds,
                remember,
                from_keychain,
            }) => Some((target, creds, remember, from_keychain)),
            _ => None,
        }
    }

    /// Fake Keychain: knows one account (any case).
    fn fake_store(account: &str) -> Option<String> {
        (account.to_lowercase() == "corp\\alice").then(|| "saved-pw".to_string())
    }

    fn pw(c: &LogonCredentials) -> &str {
        c.password.as_str()
    }

    #[test]
    fn saved_password_is_filled_in_and_enter_logs_on() {
        let mut o = LoginPromptOverlay::new("h".into(), Some("CORP\\alice".into()), None)
            .with_keychain(fake_store, true, false);
        assert!(o.remember, "a saved password ticks the box");
        o.on_enter();
        let (_, c, remember, from_kc) = confirmed_full(&mut o).expect("confirmed");
        assert_eq!(pw(&c), "saved-pw");
        assert!(remember && from_kc);
    }

    #[test]
    fn no_fill_after_rejection() {
        let mut o = LoginPromptOverlay::new("h".into(), Some("CORP\\alice".into()), None)
            .with_keychain(fake_store, false, true);
        assert!(o.password.is_empty());
        assert!(o.remember, "box keeps the user's earlier choice");
        // Tabbing around must not fill it in behind the user's back either.
        o.on_tab();
        o.on_tab();
        o.on_tab();
        assert!(o.password.is_empty());
    }

    #[test]
    fn typing_replaces_a_filled_in_password() {
        let mut o = LoginPromptOverlay::new("h".into(), Some("CORP\\alice".into()), None)
            .with_keychain(fake_store, true, false);
        o.on_text("new");
        o.on_enter();
        let (_, c, remember, from_kc) = confirmed_full(&mut o).unwrap();
        assert_eq!(pw(&c), "new");
        assert!(remember && !from_kc);
    }

    #[test]
    fn changing_the_user_drops_the_filled_in_password() {
        let mut o = LoginPromptOverlay::new("h".into(), Some("CORP\\alice".into()), None)
            .with_keychain(fake_store, true, false);
        o.on_tab(); // password -> save
        o.on_tab(); // save -> user
        o.on_backspace();
        assert!(o.password.is_empty());
        o.on_text("e");
        o.on_tab(); // back to the password: looks up CORP\alice again
        assert_eq!(o.password, "saved-pw");
    }

    #[test]
    fn user_typed_then_tab_fills_in() {
        let mut o =
            LoginPromptOverlay::new("h".into(), None, None).with_keychain(fake_store, true, false);
        o.on_text("corp\\ALICE");
        o.on_enter();
        assert_eq!(o.password, "saved-pw");
        assert_eq!(o.focus, Field::Password);
    }

    #[test]
    fn space_toggles_the_checkbox_and_backspace_clears_filled_password() {
        let mut o = LoginPromptOverlay::new("h".into(), Some("CORP\\bob".into()), None)
            .with_keychain(fake_store, true, false);
        assert!(!o.remember);
        o.on_text("pw");
        o.on_tab(); // -> save
        o.on_text(" ");
        assert!(o.remember);
        assert_eq!(o.password, "pw", "space on the checkbox is not typed");
        o.on_enter(); // Enter on the checkbox logs on
        let (_, c, remember, from_kc) = confirmed_full(&mut o).unwrap();
        assert_eq!(pw(&c), "pw");
        assert!(remember && !from_kc);

        let mut o = LoginPromptOverlay::new("h".into(), Some("CORP\\alice".into()), None)
            .with_keychain(fake_store, true, false);
        o.on_backspace();
        assert!(
            o.password.is_empty(),
            "one backspace clears the whole saved password"
        );
    }

    #[test]
    fn without_keychain_no_checkbox_and_never_remember() {
        let mut o = LoginPromptOverlay::new("h".into(), Some("D\\u".into()), None);
        o.on_tab(); // password -> user (no save field)
        assert_eq!(o.focus, Field::User);
        o.on_tab();
        o.on_text("x");
        o.on_enter();
        let (_, _, remember, from_kc) = confirmed_full(&mut o).unwrap();
        assert!(!remember && !from_kc);
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
