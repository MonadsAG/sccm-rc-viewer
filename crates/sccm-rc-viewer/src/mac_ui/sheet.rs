//! Building blocks for the native sheets: a target object that turns button
//! clicks and text edits into Rust closures, and small view constructors.
//!
//! Closures run inside AppKit callbacks. They may change the sheet's own
//! controls, but anything that concerns the App goes out as a `NativeEvent`
//! through the event loop, so the sheet is never torn down while AppKit is
//! still inside one of its callbacks.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSColor, NSControl, NSControlTextEditingDelegate, NSFont,
    NSTableView, NSTextField, NSTextFieldDelegate, NSTextView, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

/// `(changed control, editing ended)`.
type TextHandler = Box<dyn Fn(&AnyObject, bool)>;

/// Editing command in a text field (e.g. "moveUp:"); true = handled.
type CommandHandler = Box<dyn Fn(&str) -> bool>;

pub struct SheetIvars {
    buttons: RefCell<Vec<Box<dyn Fn()>>>,
    on_text: RefCell<Option<TextHandler>>,
    on_command: RefCell<Option<CommandHandler>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = SheetIvars]
    pub struct SheetTarget;

    unsafe impl NSObjectProtocol for SheetTarget {}

    impl SheetTarget {
        /// Action of every button/checkbox; the sender's tag picks the closure.
        #[unsafe(method(fire:))]
        fn fire(&self, sender: &AnyObject) {
            // SAFETY: every sender is an NSControl, which answers `tag`.
            let tag: isize = unsafe { msg_send![sender, tag] };
            let buttons = self.ivars().buttons.borrow();
            if let Some(f) = usize::try_from(tag).ok().and_then(|i| buttons.get(i)) {
                f();
            }
        }
    }

    unsafe impl NSControlTextEditingDelegate for SheetTarget {
        #[unsafe(method(controlTextDidChange:))]
        fn text_did_change(&self, n: &NSNotification) {
            self.text_event(n, false);
        }

        #[unsafe(method(controlTextDidEndEditing:))]
        fn text_did_end(&self, n: &NSNotification) {
            self.text_event(n, true);
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn do_command(&self, _control: &NSControl, _tv: &NSTextView, cmd: Sel) -> bool {
            match self.ivars().on_command.borrow().as_ref() {
                Some(f) => f(cmd.name().to_str().unwrap_or("")),
                None => false,
            }
        }
    }

    unsafe impl NSTextFieldDelegate for SheetTarget {}
);

impl SheetTarget {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SheetIvars {
            buttons: RefCell::new(Vec::new()),
            on_text: RefCell::new(None),
            on_command: RefCell::new(None),
        });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }

    fn text_event(&self, n: &NSNotification, ended: bool) {
        // Control notifications carry the control as their object.
        let Some(obj) = n.object() else {
            return;
        };
        if let Some(f) = self.ivars().on_text.borrow().as_ref() {
            f(&obj, ended);
        }
    }

    /// Register `f` and wire `control`'s action (a button click, a search
    /// field's search) to it.
    pub fn on_click(&self, control: &NSControl, f: Box<dyn Fn()>) {
        let mut buttons = self.ivars().buttons.borrow_mut();
        control.setTag(buttons.len() as isize);
        buttons.push(f);
        // SAFETY: the target outlives the control (the sheet keeps both).
        unsafe {
            control.setTarget(Some(self));
            control.setAction(Some(sel!(fire:)));
        }
    }

    /// Double-click on a table row runs `f`.
    pub fn on_double_click(&self, table: &NSTableView, f: Box<dyn Fn()>) {
        let mut buttons = self.ivars().buttons.borrow_mut();
        table.setTag(buttons.len() as isize);
        buttons.push(f);
        // SAFETY: the target outlives the table (the sheet keeps both).
        unsafe {
            table.setTarget(Some(self));
            table.setDoubleAction(Some(sel!(fire:)));
        }
    }

    /// Editing commands (arrow keys, …) in fields whose delegate is this target.
    pub fn on_command(&self, f: CommandHandler) {
        *self.ivars().on_command.borrow_mut() = Some(f);
    }

    /// Text edits of every field whose delegate is this target.
    pub fn on_text(&self, f: TextHandler) {
        *self.ivars().on_text.borrow_mut() = Some(f);
    }

    pub fn watch(&self, field: &NSTextField) {
        // SAFETY: the target outlives the field (the sheet keeps both).
        unsafe { field.setDelegate(Some(ProtocolObject::from_ref(self))) };
    }
}

/// True when `obj` is `field` (same Objective-C object).
pub fn is(obj: &AnyObject, field: &NSTextField) -> bool {
    std::ptr::eq(
        obj as *const AnyObject,
        (field as *const NSTextField).cast(),
    )
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// A sheet window of `w × h` points with an empty content view.
pub fn sheet_window(mtm: MainThreadMarker, w: f64, h: f64) -> Retained<NSWindow> {
    // SAFETY: plain NSWindow initialiser; released manually (see below).
    let win = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, w, h),
            NSWindowStyleMask::Titled,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: we keep a Retained; AppKit must not release it on close.
    unsafe { win.setReleasedWhenClosed(false) };
    win
}

pub fn add(parent: &NSView, view: &NSView) {
    parent.addSubview(view);
}

pub fn label(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFrame(frame);
    l
}

pub fn title_label(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let l = label(mtm, text, frame);
    l.setFont(Some(&NSFont::boldSystemFontOfSize(15.0)));
    l
}

pub fn note_label(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let l = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    l.setFrame(frame);
    l.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    l.setTextColor(Some(&NSColor::secondaryLabelColor()));
    l
}

pub fn error_label(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSTextField> {
    let l = NSTextField::wrappingLabelWithString(&NSString::from_str(""), mtm);
    l.setFrame(frame);
    l.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    l.setTextColor(Some(&NSColor::systemRedColor()));
    l
}

pub fn text_field(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSTextField> {
    let f = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
    f.setFrame(frame);
    f
}

pub fn push_button(
    mtm: MainThreadMarker,
    title: &str,
    frame: NSRect,
    key: &str,
) -> Retained<NSButton> {
    // SAFETY: no target/action yet; `SheetTarget::on_click` sets them.
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
    };
    b.setFrame(frame);
    b.setKeyEquivalent(&NSString::from_str(key));
    b
}

pub fn checkbox(mtm: MainThreadMarker, title: &str, frame: NSRect) -> Retained<NSButton> {
    // SAFETY: as above.
    let b = unsafe {
        NSButton::checkboxWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
    };
    b.setFrame(frame);
    b
}

pub fn text_of(field: &NSTextField) -> String {
    field.stringValue().to_string()
}

pub fn set_text(field: &NSTextField, text: &str) {
    field.setStringValue(&NSString::from_str(text));
}

/// Mark a field for the system's password AutoFill (`NSTextContentType…`).
pub fn set_content_type(field: &NSTextField, ty: &NSString) {
    // SAFETY: NSTextField conforms to NSTextContent (macOS 11+).
    let _: () = unsafe { msg_send![field, setContentType: ty] };
}
