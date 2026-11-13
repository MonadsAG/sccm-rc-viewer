//! Native logon sheet: `DOMAIN\user`, a secure password field, the optional
//! "save password in Keychain" checkbox, the NTLM note and an error line.
//!
//! Same rules as the drawn overlay (`login_prompt.rs`): a saved password is
//! filled in for a known account and ticks the box; editing the user drops a
//! filled-in password (and an automatic tick); leaving the user field looks
//! the new account up; editing the password means it no longer comes from the
//! Keychain. The result goes out as `NativeEvent::Logon(LoginOutcome)`.

use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSButton, NSControlStateValueOff, NSControlStateValueOn, NSSecureTextField,
    NSTextContentTypePassword, NSTextContentTypeUsername, NSTextField, NSWindow,
};
use objc2_foundation::NSString;
use rust_i18n::t;
use sccm_rc_core::LogonCredentials;

use super::sheet::{self, rect, SheetTarget};
use crate::login_prompt::{account_key, LoginOutcome};
use crate::{keychain, NativeEvent};

const W: f64 = 440.0;
const H: f64 = 300.0;

pub struct LogonSheet {
    pub window: Retained<NSWindow>,
    pub target: Retained<SheetTarget>,
    /// Field that gets the keyboard focus when the sheet opens.
    pub focus: Retained<NSTextField>,
}

#[derive(Default)]
struct State {
    /// Account whose saved password is in the password field, unchanged.
    filled: Option<String>,
    /// The checkbox reflects the user's own choice (not an automatic tick).
    explicit: bool,
}

struct Controls {
    user: Retained<NSTextField>,
    pass: Retained<NSTextField>,
    remember: Retained<NSButton>,
    error: Retained<NSTextField>,
    state: RefCell<State>,
}

impl Controls {
    fn ticked(&self) -> bool {
        self.remember.state() == NSControlStateValueOn
    }

    fn tick(&self, on: bool) {
        self.remember.setState(if on {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
    }

    /// Fill in the saved password for the account in the user field.
    fn fill(&self) {
        if !sheet::text_of(&self.pass).is_empty() {
            return;
        }
        let Some(account) = account_key(&sheet::text_of(&self.user)) else {
            return;
        };
        if let Some(saved) = keychain::load(&account) {
            sheet::set_text(&self.pass, &saved);
            let explicit = {
                let mut st = self.state.borrow_mut();
                st.filled = Some(account);
                st.explicit
            };
            if !explicit {
                self.tick(true);
            }
        }
    }

    fn user_edited(&self) {
        let (was_filled, explicit) = {
            let mut st = self.state.borrow_mut();
            (st.filled.take().is_some(), st.explicit)
        };
        if was_filled {
            sheet::set_text(&self.pass, "");
        }
        if !explicit {
            self.tick(false);
        }
    }

    fn show_error(&self, msg: &str) {
        sheet::set_text(&self.error, msg);
    }

    /// Validate and build the outcome, or show what is missing.
    fn submit(&self, target: &str) -> Option<LoginOutcome> {
        let user = sheet::text_of(&self.user);
        let password = sheet::text_of(&self.pass);
        if user.trim().is_empty() {
            self.show_error(&t!("login.need_user"));
            return None;
        }
        if password.is_empty() {
            self.show_error(&t!("login.need_password"));
            return None;
        }
        let creds = match LogonCredentials::parse(&user, None, &password) {
            Ok(c) => c,
            Err(e) => {
                self.show_error(&e.to_string());
                return None;
            }
        };
        let from_keychain = {
            let st = self.state.borrow();
            st.filled.is_some() && st.filled == account_key(&user)
        };
        sheet::set_text(&self.pass, "");
        self.state.borrow_mut().filled = None;
        Some(LoginOutcome::Confirmed {
            target: target.to_string(),
            creds,
            remember: keychain::AVAILABLE && self.ticked(),
            from_keychain,
        })
    }
}

/// Build the sheet. `fill`: fill in a saved password for `user`;
/// `remember`: the user's earlier checkbox choice (kept as is).
pub fn build(
    mtm: MainThreadMarker,
    target: &str,
    user: Option<String>,
    error: Option<String>,
    fill: bool,
    remember: Option<bool>,
    emit: Rc<dyn Fn(NativeEvent)>,
) -> LogonSheet {
    let window = sheet::sheet_window(mtm, W, H);
    let view = window.contentView().expect("sheet content view");
    let m = 20.0;
    let fw = W - 2.0 * m;

    let title = sheet::title_label(
        mtm,
        &t!("login.title", target => target),
        rect(m, H - 42.0, fw, 22.0),
    );
    let note = sheet::note_label(mtm, &t!("login.ntlm_note"), rect(m, H - 64.0, fw, 18.0));
    let user_label = sheet::label(mtm, &t!("login.user"), rect(m, H - 92.0, fw, 17.0));
    let user_field = sheet::text_field(mtm, rect(m, H - 120.0, fw, 24.0));
    user_field.setPlaceholderString(Some(&NSString::from_str(&t!("login.user_placeholder"))));
    // SAFETY: AppKit constants.
    sheet::set_content_type(&user_field, unsafe { NSTextContentTypeUsername });
    let pass_label = sheet::label(mtm, &t!("login.password"), rect(m, H - 146.0, fw, 17.0));
    let pass_field = NSSecureTextField::initWithFrame(
        NSSecureTextField::alloc(mtm),
        rect(m, H - 174.0, fw, 24.0),
    );
    // SAFETY: AppKit constant.
    sheet::set_content_type(&pass_field, unsafe { NSTextContentTypePassword });
    // The secure field is used through its NSTextField superclass.
    let pass_as_field: Retained<NSTextField> = Retained::into_super(pass_field);
    let remember_box = sheet::checkbox(
        mtm,
        &t!("login.save_password"),
        rect(m, H - 204.0, fw, 20.0),
    );
    let error_label = sheet::error_label(mtm, rect(m, 56.0, fw, 34.0));
    let cancel = sheet::push_button(
        mtm,
        &t!("prompt.cancel"),
        rect(W - m - 230.0, 14.0, 110.0, 32.0),
        "\u{1b}",
    );
    let ok = sheet::push_button(
        mtm,
        &t!("login.log_on"),
        rect(W - m - 110.0, 14.0, 110.0, 32.0),
        "\r",
    );

    for v in [
        &*title as &objc2_app_kit::NSView,
        &note,
        &user_label,
        &user_field,
        &pass_label,
        &pass_as_field,
        &error_label,
        &cancel,
        &ok,
    ] {
        sheet::add(&view, v);
    }
    if keychain::AVAILABLE {
        sheet::add(&view, &remember_box);
    }

    let controls = Rc::new(Controls {
        user: user_field.clone(),
        pass: pass_as_field.clone(),
        remember: remember_box.clone(),
        error: error_label.clone(),
        state: RefCell::new(State::default()),
    });

    if let Some(u) = user {
        sheet::set_text(&user_field, &u);
    }
    if let Some(e) = error {
        controls.show_error(&e);
    }
    if let Some(r) = remember {
        controls.tick(r);
        controls.state.borrow_mut().explicit = true;
    }
    if fill && keychain::AVAILABLE {
        controls.fill();
    }

    let target_obj = SheetTarget::new(mtm);
    {
        let c = controls.clone();
        target_obj.on_click(
            &remember_box,
            Box::new(move || c.state.borrow_mut().explicit = true),
        );
    }
    {
        let c = controls.clone();
        let host = target.to_string();
        let emit = emit.clone();
        target_obj.on_click(
            &ok,
            Box::new(move || {
                if let Some(outcome) = c.submit(&host) {
                    emit(NativeEvent::Logon(outcome));
                }
            }),
        );
    }
    {
        let emit = emit.clone();
        target_obj.on_click(
            &cancel,
            Box::new(move || emit(NativeEvent::Logon(LoginOutcome::Cancelled))),
        );
    }
    {
        let c = controls.clone();
        target_obj.on_text(Box::new(move |obj, ended| {
            if sheet::is(obj, &c.user) {
                if ended {
                    c.fill();
                } else {
                    c.user_edited();
                }
            } else if sheet::is(obj, &c.pass) && !ended {
                // Typed by hand: no longer the saved password.
                c.state.borrow_mut().filled = None;
            }
        }));
    }
    target_obj.watch(&user_field);
    target_obj.watch(&pass_as_field);

    let focus = if sheet::text_of(&user_field).trim().is_empty() {
        user_field
    } else {
        pass_as_field
    };
    LogonSheet {
        window,
        target: target_obj,
        focus,
    }
}
