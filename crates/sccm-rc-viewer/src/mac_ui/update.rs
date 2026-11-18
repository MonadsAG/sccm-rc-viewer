//! Update sheet: offers a newer build (Install / Later / Skip, release
//! notes) or reports the result of a manual check (OK). While the download
//! runs the buttons are disabled and a status line shows progress or the
//! error. Every click goes out as `NativeEvent::Update(UpdateCmd)`.

use std::rc::Rc;

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSButton, NSTextField, NSView, NSWindow};
use objc2_foundation::NSString;
use rust_i18n::t;

use super::sheet::{self, rect, SheetTarget};
use crate::{NativeEvent, UpdateCmd, UpdateSheet};

const W: f64 = 460.0;
const H: f64 = 220.0;

pub struct Built {
    pub window: Retained<NSWindow>,
    pub target: Retained<SheetTarget>,
    pub ctl: Rc<Ctl>,
}

/// Live controls the App updates while the download runs.
pub struct Ctl {
    status: Retained<NSTextField>,
    buttons: Vec<Retained<NSButton>>,
}

impl Ctl {
    /// Busy: buttons off, `msg` as status.
    pub fn busy(&self, msg: &str) {
        sheet::set_text(&self.status, msg);
        for b in &self.buttons {
            b.setEnabled(false);
        }
    }

    /// Failed: buttons back on, `msg` as status.
    pub fn failed(&self, msg: &str) {
        sheet::set_text(&self.status, msg);
        for b in &self.buttons {
            b.setEnabled(true);
        }
    }
}

pub fn build(mtm: MainThreadMarker, what: &UpdateSheet, emit: Rc<dyn Fn(NativeEvent)>) -> Built {
    let window = sheet::sheet_window(mtm, W, H);
    let view = window.contentView().expect("sheet content view");
    let target = SheetTarget::new(mtm);
    let m = 20.0;
    let fw = W - 2.0 * m;

    let (title, body) = match what {
        UpdateSheet::Offer {
            label,
            installed,
            session,
            ..
        } => {
            let mut body = t!("update.offer_body", installed => installed).to_string();
            if *session {
                body.push(' ');
                body.push_str(&t!("update.session_note"));
            }
            (t!("update.offer_title", label => label).to_string(), body)
        }
        UpdateSheet::Info { title, body } => (title.clone(), body.clone()),
    };
    let title = sheet::title_label(mtm, &title, rect(m, H - 42.0, fw, 22.0));
    let body = NSTextField::wrappingLabelWithString(&NSString::from_str(&body), mtm);
    body.setFrame(rect(m, H - 110.0, fw, 60.0));
    let status = sheet::note_label(mtm, "", rect(m, 50.0, fw, 22.0));
    for v in [&*title as &NSView, &body, &status] {
        sheet::add(&view, v);
    }

    let send = |cmd: UpdateCmd| {
        let emit = emit.clone();
        Box::new(move || emit(NativeEvent::Update(cmd.clone()))) as Box<dyn Fn()>
    };
    let mut buttons = Vec::new();
    match what {
        UpdateSheet::Offer { notes, .. } => {
            let install = sheet::push_button(
                mtm,
                &t!("update.install"),
                rect(W - m - 190.0, 14.0, 190.0, 32.0),
                "\r",
            );
            let later = sheet::push_button(
                mtm,
                &t!("update.later"),
                rect(W - m - 290.0, 14.0, 96.0, 32.0),
                "\u{1b}",
            );
            let skip = sheet::push_button(
                mtm,
                &t!("update.skip"),
                rect(m - 6.0, 14.0, 130.0, 32.0),
                "",
            );
            target.on_click(&install, send(UpdateCmd::Install));
            target.on_click(&later, send(UpdateCmd::Later));
            target.on_click(&skip, send(UpdateCmd::Skip));
            for b in [&install, &later, &skip] {
                sheet::add(&view, b);
            }
            buttons.extend([install, later, skip]);
            if notes.is_some() {
                let n = sheet::push_button(
                    mtm,
                    &t!("update.notes"),
                    rect(m - 6.0, 78.0, 180.0, 28.0),
                    "",
                );
                target.on_click(&n, send(UpdateCmd::Notes));
                sheet::add(&view, &n);
                buttons.push(n);
            }
        }
        UpdateSheet::Info { .. } => {
            let ok = sheet::push_button(
                mtm,
                &t!("update.ok"),
                rect(W - m - 110.0, 14.0, 110.0, 32.0),
                "\r",
            );
            target.on_click(&ok, send(UpdateCmd::Dismiss));
            sheet::add(&view, &ok);
            buttons.push(ok);
        }
    }

    Built {
        window,
        target,
        ctl: Rc::new(Ctl { status, buttons }),
    }
}
