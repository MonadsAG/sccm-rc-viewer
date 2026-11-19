//! Update sheet: offers a newer build (Install / Later / Skip) with its
//! release notes in a scrolling text view, or reports the result of a manual
//! check (OK). While the download runs the buttons are disabled and a status
//! line shows progress or the error. Every click goes out as
//! `NativeEvent::Update(UpdateCmd)`.

use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBorderType, NSButton, NSColor, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSMutableParagraphStyle, NSParagraphStyleAttributeName,
    NSScrollView, NSTextField, NSTextView, NSView, NSWindow,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableAttributedString, NSRect, NSSize, NSString,
};
use rust_i18n::t;

use super::sheet::{self, rect, SheetTarget};
use crate::update::notes::{self, Line};
use crate::{NativeEvent, UpdateCmd, UpdateSheet};

const W: f64 = 460.0;
/// Height without release notes, and with them.
const H: f64 = 220.0;
const H_NOTES: f64 = 430.0;

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
    let notes = match what {
        UpdateSheet::Offer { notes: Some(n), .. } => Some(notes::render(n)),
        _ => None,
    }
    .filter(|l| !l.is_empty());
    let h = if notes.is_some() { H_NOTES } else { H };
    let window = sheet::sheet_window(mtm, W, h);
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
    let title = sheet::title_label(mtm, &title, rect(m, h - 42.0, fw, 22.0));
    let body = NSTextField::wrappingLabelWithString(&NSString::from_str(&body), mtm);
    body.setFrame(rect(m, h - 110.0, fw, 60.0));
    let status = sheet::note_label(mtm, "", rect(m, 50.0, fw, 22.0));
    for v in [&*title as &NSView, &body, &status] {
        sheet::add(&view, v);
    }
    if let Some(lines) = &notes {
        let label = sheet::label(mtm, &t!("update.notes_label"), rect(m, h - 136.0, fw, 18.0));
        label.setFont(Some(&NSFont::boldSystemFontOfSize(12.0)));
        let top = h - 142.0;
        let bottom = 78.0;
        let scroll = notes_view(mtm, lines, rect(m, bottom, fw, top - bottom));
        sheet::add(&view, &label);
        sheet::add(&view, &scroll);
    }

    let send = |cmd: UpdateCmd| {
        let emit = emit.clone();
        Box::new(move || emit(NativeEvent::Update(cmd.clone()))) as Box<dyn Fn()>
    };
    let mut buttons = Vec::new();
    match what {
        UpdateSheet::Offer { .. } => {
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

/// Read-only, selectable text view with the release notes: headings bold,
/// bullets with a hanging indent.
fn notes_view(mtm: MainThreadMarker, lines: &[Line], frame: NSRect) -> Retained<NSScrollView> {
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
    scroll.setHasVerticalScroller(true);
    scroll.setBorderType(NSBorderType::BezelBorder);
    let size = scroll.contentSize();
    let tv = NSTextView::initWithFrame(
        NSTextView::alloc(mtm),
        rect(0.0, 0.0, size.width, size.height),
    );
    tv.setEditable(false);
    tv.setSelectable(true);
    tv.setVerticallyResizable(true);
    tv.setHorizontallyResizable(false);
    tv.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    tv.setTextContainerInset(NSSize::new(6.0, 8.0));
    // SAFETY: the text view owns its storage and outlives this call.
    if let Some(storage) = unsafe { tv.textStorage() } {
        storage.setAttributedString(&styled(lines));
    }
    scroll.setDocumentView(Some(&tv));
    scroll
}

fn styled(lines: &[Line]) -> Retained<NSMutableAttributedString> {
    let out = NSMutableAttributedString::new();
    let body = NSFont::systemFontOfSize(12.0);
    let bold = NSFont::boldSystemFontOfSize(12.0);
    let color = NSColor::labelColor();
    let plain = NSMutableParagraphStyle::new();
    plain.setParagraphSpacing(4.0);
    let bullet = NSMutableParagraphStyle::new();
    bullet.setParagraphSpacing(4.0);
    bullet.setDefaultTabInterval(14.0);
    bullet.setHeadIndent(14.0);
    let n = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let (text, font, para) = match line {
            Line::Heading(t) => (t.clone(), &bold, &plain),
            Line::Bullet(t) => (format!("\u{2022}\t{t}"), &body, &bullet),
            Line::Text(t) => (t.clone(), &body, &plain),
            Line::Blank => (String::new(), &body, &plain),
        };
        let text = if i + 1 < n { text + "\n" } else { text };
        let font: &AnyObject = font;
        let para: &AnyObject = para;
        let color: &AnyObject = &color;
        // SAFETY: AppKit's attribute-name constants, valid for the process.
        let keys = unsafe {
            [
                NSFontAttributeName,
                NSParagraphStyleAttributeName,
                NSForegroundColorAttributeName,
            ]
        };
        let attrs = NSDictionary::from_slices(&keys, &[font, para, color]);
        // SAFETY: values match their keys (NSFont, NSParagraphStyle, NSColor).
        let piece = unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &NSString::from_str(&text),
                Some(&attrs),
            )
        };
        out.appendAttributedString(&piece);
    }
    out
}
