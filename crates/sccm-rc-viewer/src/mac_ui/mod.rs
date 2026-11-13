//! Native macOS window chrome (AppKit via objc2): the menu bar, a toolbar in
//! the title bar with the session controls, and the connection status as the
//! window subtitle. Replaces the self-drawn toolbar strip on macOS only; the
//! other platforms keep `toolbar.rs` (see `mac_ui_stub.rs`).
//!
//! AppKit callbacks never touch the `App`: menu items and toolbar items all
//! target one [`ActionTarget`], which only forwards a [`NativeEvent`] through
//! the closure it was built with (the App posts it to the event loop). The App
//! pushes state back with [`NativeUi::update`].
//!
//! Shortcuts: only the ones winit's default menu already had (Cmd+Q, Cmd+H,
//! Option+Cmd+H) plus Ctrl+Cmd+F for full screen. Menu key equivalents are
//! matched before the remote view sees a key, so anything else (Cmd+C/V/A/F…)
//! must stay free for the remote session, where Cmd is sent as Ctrl. The Edit
//! menu therefore gets its shortcuts (Cmd+Z/X/C/V/A) only while a sheet with
//! text fields is open, and loses them again when it closes.
//!
//! `SCCM_RC_NATIVE_UI=0` turns it off (the drawn toolbar comes back).

use std::cell::RefCell;
use std::rc::Rc;

mod logon;
mod sheet;
mod start;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSEventModifierFlags, NSImage,
    NSMenu, NSMenuItem, NSToolbar, NSToolbarDelegate, NSToolbarDisplayMode,
    NSToolbarFlexibleSpaceItemIdentifier, NSToolbarItem, NSView, NSWindow, NSWindowToolbarStyle,
};
use objc2_foundation::{NSArray, NSCopying, NSObject, NSObjectProtocol, NSString};
use rust_i18n::t;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::toolbar::ToolbarAction;
use crate::NativeEvent;

/// Every action the native chrome can trigger; the index is the item tag.
const ACTIONS: [ToolbarAction; 13] = [
    ToolbarAction::CtrlAltDel,
    ToolbarAction::SendWin,
    ToolbarAction::SendKeys,
    ToolbarAction::SendFile,
    ToolbarAction::ToggleCurtain,
    ToolbarAction::ToggleViewOnly,
    ToolbarAction::ToggleRecord,
    ToolbarAction::ToggleFullscreen,
    ToolbarAction::Disconnect,
    ToolbarAction::MonitorCycle,
    ToolbarAction::BugReport,
    ToolbarAction::About,
    ToolbarAction::ToggleAgentControl,
];

/// Toolbar order (left to right), after a flexible space; `None` = separator
/// space. About and Bug report live in the menus only.
const TOOLBAR_ITEMS: [Option<ToolbarAction>; 13] = [
    Some(ToolbarAction::CtrlAltDel),
    Some(ToolbarAction::SendKeys),
    Some(ToolbarAction::SendWin),
    Some(ToolbarAction::SendFile),
    None,
    Some(ToolbarAction::MonitorCycle),
    Some(ToolbarAction::ToggleViewOnly),
    Some(ToolbarAction::ToggleCurtain),
    Some(ToolbarAction::ToggleRecord),
    Some(ToolbarAction::ToggleFullscreen),
    Some(ToolbarAction::ToggleAgentControl),
    None,
    Some(ToolbarAction::Disconnect),
];

fn tag_of(a: ToolbarAction) -> isize {
    ACTIONS.iter().position(|x| *x == a).unwrap_or(0) as isize
}

/// Stable toolbar item identifier per action.
fn ident_of(a: ToolbarAction) -> &'static str {
    match a {
        ToolbarAction::CtrlAltDel => "rc.ctrl-alt-del",
        ToolbarAction::SendWin => "rc.send-win",
        ToolbarAction::SendKeys => "rc.type-text",
        ToolbarAction::SendFile => "rc.send-file",
        ToolbarAction::ToggleCurtain => "rc.curtain",
        ToolbarAction::ToggleViewOnly => "rc.view-only",
        ToolbarAction::ToggleRecord => "rc.record",
        ToolbarAction::ToggleFullscreen => "rc.fullscreen",
        ToolbarAction::Disconnect => "rc.disconnect",
        ToolbarAction::MonitorCycle => "rc.screen",
        ToolbarAction::BugReport => "rc.report",
        ToolbarAction::About => "rc.about",
        ToolbarAction::ToggleAgentControl => "rc.claude-control",
    }
}

fn action_of_ident(ident: &str) -> Option<ToolbarAction> {
    ACTIONS.iter().copied().find(|a| ident_of(*a) == ident)
}

/// Localized label (the same strings as the drawn toolbar).
fn label_of(a: ToolbarAction) -> String {
    match a {
        ToolbarAction::Disconnect => t!("toolbar.disconnect"),
        ToolbarAction::ToggleFullscreen => t!("toolbar.fullscreen"),
        ToolbarAction::ToggleRecord => t!("toolbar.record"),
        ToolbarAction::ToggleViewOnly => t!("toolbar.view_only"),
        ToolbarAction::ToggleCurtain => t!("toolbar.curtain"),
        ToolbarAction::SendFile => t!("toolbar.send_file"),
        ToolbarAction::CtrlAltDel => t!("toolbar.ctrl_alt_del"),
        ToolbarAction::SendWin => t!("toolbar.send_win"),
        ToolbarAction::SendKeys => t!("toolbar.send_keys"),
        ToolbarAction::BugReport => t!("toolbar.bug_report"),
        ToolbarAction::About => t!("toolbar.about"),
        ToolbarAction::MonitorCycle => t!("menu.next_screen"),
        ToolbarAction::ToggleAgentControl => t!("control.allow"),
    }
    .to_string()
}

/// SF Symbol per action; toggles get a filled variant while on.
fn symbol_of(a: ToolbarAction, on: bool) -> &'static str {
    match (a, on) {
        (ToolbarAction::CtrlAltDel, _) => "lock.shield",
        (ToolbarAction::SendWin, _) => "square.grid.2x2",
        (ToolbarAction::SendKeys, _) => "keyboard",
        (ToolbarAction::SendFile, _) => "doc.badge.arrow.up",
        (ToolbarAction::ToggleCurtain, false) => "eye.slash",
        (ToolbarAction::ToggleCurtain, true) => "eye.slash.fill",
        (ToolbarAction::ToggleViewOnly, false) => "hand.raised",
        (ToolbarAction::ToggleViewOnly, true) => "hand.raised.fill",
        (ToolbarAction::ToggleRecord, false) => "record.circle",
        (ToolbarAction::ToggleRecord, true) => "record.circle.fill",
        (ToolbarAction::ToggleFullscreen, false) => "arrow.up.left.and.arrow.down.right",
        (ToolbarAction::ToggleFullscreen, true) => "arrow.down.right.and.arrow.up.left",
        (ToolbarAction::Disconnect, _) => "xmark.circle",
        (ToolbarAction::MonitorCycle, _) => "display.2",
        (ToolbarAction::BugReport, _) => "ladybug",
        (ToolbarAction::About, _) => "info.circle",
        (ToolbarAction::ToggleAgentControl, false) => "cpu",
        (ToolbarAction::ToggleAgentControl, true) => "cpu.fill",
    }
}

fn symbol(name: &str, label: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&NSString::from_str(label)),
    )
}

/// State the App pushes into the chrome; `update` skips identical states.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chrome {
    pub subtitle: String,
    /// A remote desktop is showing (session controls enabled).
    pub connected: bool,
    /// A session exists or is being set up (Disconnect enabled).
    pub has_session: bool,
    pub view_only: bool,
    pub curtain: bool,
    pub recording: bool,
    pub fullscreen: bool,
    /// Screen switcher label for multi-monitor targets, else disabled.
    pub monitor: Option<String>,
    /// "Allow control by Claude" is on (the control socket listens).
    pub agent_control: bool,
}

impl Chrome {
    fn toggled(&self, a: ToolbarAction) -> bool {
        match a {
            ToolbarAction::ToggleViewOnly => self.view_only,
            ToolbarAction::ToggleCurtain => self.curtain,
            ToolbarAction::ToggleRecord => self.recording,
            ToolbarAction::ToggleFullscreen => self.fullscreen,
            ToolbarAction::ToggleAgentControl => self.agent_control,
            _ => false,
        }
    }

    fn enabled(&self, a: ToolbarAction) -> bool {
        match a {
            ToolbarAction::About | ToolbarAction::BugReport => true,
            ToolbarAction::ToggleFullscreen | ToolbarAction::ToggleAgentControl => true,
            ToolbarAction::Disconnect => self.has_session,
            ToolbarAction::MonitorCycle => self.connected && self.monitor.is_some(),
            _ => self.connected,
        }
    }
}

pub struct TargetIvars {
    on_event: Rc<dyn Fn(NativeEvent)>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetIvars]
    pub struct ActionTarget;

    unsafe impl NSObjectProtocol for ActionTarget {}

    impl ActionTarget {
        /// Target of every menu item and toolbar item; the sender's tag is
        /// the index into `ACTIONS`.
        #[unsafe(method(performAction:))]
        fn perform_action(&self, sender: &AnyObject) {
            // SAFETY: NSMenuItem and NSToolbarItem both answer `tag` (NSInteger).
            let tag: isize = unsafe { msg_send![sender, tag] };
            if let Some(a) = usize::try_from(tag).ok().and_then(|i| ACTIONS.get(i)) {
                (self.ivars().on_event)(NativeEvent::Action(*a));
            }
        }

        /// Quit from the app menu (Cmd+Q).
        #[unsafe(method(performQuit:))]
        fn perform_quit(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_event)(NativeEvent::Quit);
        }

        /// The "quit" Apple Event: Dock › Quit, logout/shutdown, `osascript`.
        #[unsafe(method(handleQuitEvent:withReply:))]
        fn handle_quit_event(&self, _event: &AnyObject, _reply: &AnyObject) {
            (self.ivars().on_event)(NativeEvent::Quit);
        }
    }
);

/// `kCoreEventClass` ('aevt') and `kAEQuitApplication` ('quit').
const AE_CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const AE_QUIT_APPLICATION: u32 = u32::from_be_bytes(*b"quit");

/// Route the "quit" Apple Event to `target`. AppKit's own handling calls
/// `terminate:`, which refuses while a sheet is open (and the start sheet
/// is open whenever there is no session), so Dock › Quit did nothing. The
/// App instead closes the sheet, releases the session and exits.
fn install_quit_handler(target: &ActionTarget) {
    let manager = objc2_foundation::NSAppleEventManager::sharedAppleEventManager();
    // SAFETY: the selector exists on ActionTarget with the
    // (NSAppleEventDescriptor, NSAppleEventDescriptor) signature; the target
    // lives as long as NativeUi, i.e. the whole run.
    let _: () = unsafe {
        msg_send![
            &manager,
            setEventHandler: target,
            andSelector: sel!(handleQuitEvent:withReply:),
            forEventClass: AE_CORE_EVENT_CLASS,
            andEventID: AE_QUIT_APPLICATION
        ]
    };
}

impl ActionTarget {
    fn new(mtm: MainThreadMarker, on_event: Rc<dyn Fn(NativeEvent)>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { on_event });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

pub struct ToolbarIvars {
    target: Retained<ActionTarget>,
    state: RefCell<Chrome>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ToolbarIvars]
    pub struct ToolbarDelegate;

    unsafe impl NSObjectProtocol for ToolbarDelegate {}

    unsafe impl NSToolbarDelegate for ToolbarDelegate {
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn item_for_identifier(
            &self,
            _toolbar: &NSToolbar,
            ident: &NSString,
            _will_insert: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            self.item_for(ident)
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn default_identifiers(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            default_identifiers()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed_identifiers(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            let mut v: Vec<Retained<NSString>> = ACTIONS
                .iter()
                .map(|a| NSString::from_str(ident_of(*a)))
                .collect();
            // SAFETY: AppKit-provided constant.
            v.push(unsafe { NSToolbarFlexibleSpaceItemIdentifier }.copy());
            NSArray::from_retained_slice(&v)
        }
    }
);

impl ToolbarDelegate {
    /// Build the toolbar item for `ident` in the current state. `update`
    /// reaches the live ones through `NSToolbar::items`, so palette copies
    /// (customization) are not kept around.
    fn item_for(&self, ident: &NSString) -> Option<Retained<NSToolbarItem>> {
        let action = action_of_ident(&ident.to_string())?;
        let item = make_toolbar_item(self.mtm(), action, &self.ivars().target, ident);
        apply_item_state(&item, action, &self.ivars().state.borrow());
        Some(item)
    }
}

fn default_identifiers() -> Retained<NSArray<NSString>> {
    // SAFETY: AppKit-provided constant.
    let flex = unsafe { NSToolbarFlexibleSpaceItemIdentifier }.copy();
    let mut v = vec![flex.clone()];
    for slot in TOOLBAR_ITEMS {
        match slot {
            Some(a) => v.push(NSString::from_str(ident_of(a))),
            None => v.push(flex.clone()),
        }
    }
    v.pop_if(|s| **s == *flex);
    NSArray::from_retained_slice(&v)
}

fn make_toolbar_item(
    mtm: MainThreadMarker,
    action: ToolbarAction,
    target: &ActionTarget,
    ident: &NSString,
) -> Retained<NSToolbarItem> {
    let item = NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(mtm), ident);
    let label = NSString::from_str(&label_of(action));
    item.setLabel(&label);
    item.setPaletteLabel(&label);
    item.setToolTip(Some(&label));
    item.setTag(tag_of(action));
    item.setAutovalidates(false);
    item.setBordered(true);
    // SAFETY: `target` outlives the item (NativeUi keeps it); the selector
    // exists on ActionTarget.
    unsafe {
        item.setTarget(Some(target));
        item.setAction(Some(sel!(performAction:)));
    }
    item
}

fn apply_item_state(item: &NSToolbarItem, action: ToolbarAction, c: &Chrome) {
    let on = c.toggled(action);
    let label = match (action, &c.monitor) {
        (ToolbarAction::MonitorCycle, Some(m)) => m.clone(),
        _ => label_of(action),
    };
    item.setImage(symbol(symbol_of(action, on), &label).as_deref());
    item.setToolTip(Some(&NSString::from_str(&label)));
    item.setEnabled(c.enabled(action));
}

/// The installed chrome. Dropping it would leave dangling targets, so the App
/// keeps it for the lifetime of the window.
pub struct NativeUi {
    window: Retained<NSWindow>,
    _target: Retained<ActionTarget>,
    toolbar_delegate: Retained<ToolbarDelegate>,
    toolbar: Retained<NSToolbar>,
    menu_items: Vec<(ToolbarAction, Retained<NSMenuItem>)>,
    /// Edit menu items and the shortcut each gets while a sheet is open.
    edit_items: Vec<(Retained<NSMenuItem>, &'static str)>,
    last: RefCell<Option<Chrome>>,
    emit: Rc<dyn Fn(NativeEvent)>,
    /// The open sheet.
    sheet: RefCell<Option<OpenSheet>>,
}

/// An open sheet: its window, plus whatever must live as long as it (the
/// target owning the closures, the start sheet's controller).
struct OpenSheet {
    window: Retained<NSWindow>,
    _target: Retained<sheet::SheetTarget>,
    start: Option<Rc<start::Ctl>>,
}

impl NativeUi {
    /// Install menu bar and toolbar on the winit window. `None` when turned
    /// off (`SCCM_RC_NATIVE_UI=0`) or the window isn't an AppKit window.
    pub fn install(
        window: &winit::window::Window,
        on_event: Box<dyn Fn(NativeEvent)>,
    ) -> Option<Self> {
        if std::env::var("SCCM_RC_NATIVE_UI").as_deref() == Ok("0") {
            return None;
        }
        let mtm = MainThreadMarker::new()?;
        let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: winit hands out a valid NSView pointer for the window's lifetime.
        let view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        let ns_window = view.window()?;

        let emit: Rc<dyn Fn(NativeEvent)> = Rc::from(on_event);
        let target = ActionTarget::new(mtm, emit.clone());
        install_quit_handler(&target);
        let (menu_items, edit_items) = install_menu(mtm, &target);

        let delegate_obj = ToolbarDelegate::alloc(mtm).set_ivars(ToolbarIvars {
            target: target.clone(),
            state: RefCell::new(Chrome::default()),
        });
        // SAFETY: NSObject's init.
        let toolbar_delegate: Retained<ToolbarDelegate> =
            unsafe { msg_send![super(delegate_obj), init] };
        // The identifier keys the autosaved layout. Bumped to v2 when the
        // "Allow control by Claude" toggle was added, so existing users get
        // the new default layout once instead of a saved one without it.
        let toolbar = NSToolbar::initWithIdentifier(
            NSToolbar::alloc(mtm),
            &NSString::from_str("sccm-rc.session.v2"),
        );
        toolbar.setDisplayMode(NSToolbarDisplayMode::IconOnly);
        toolbar.setAllowsUserCustomization(true);
        toolbar.setAutosavesConfiguration(true);
        toolbar.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(
            &*toolbar_delegate,
        )));
        ns_window.setToolbarStyle(NSWindowToolbarStyle::Unified);
        ns_window.setToolbar(Some(&toolbar));

        Some(Self {
            window: ns_window,
            _target: target,
            toolbar_delegate,
            toolbar,
            menu_items,
            edit_items,
            last: RefCell::new(None),
            emit,
            sheet: RefCell::new(None),
        })
    }

    /// Show the logon sheet (replacing any open sheet). The result arrives as
    /// `NativeEvent::Logon`; the App then calls [`close_sheet`](Self::close_sheet).
    pub fn show_logon(
        &self,
        target: &str,
        user: Option<String>,
        error: Option<String>,
        fill: bool,
        remember: Option<bool>,
    ) {
        self.close_sheet();
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let s = logon::build(mtm, target, user, error, fill, remember, self.emit.clone());
        self.window.beginSheet_completionHandler(&s.window, None);
        s.window.makeFirstResponder(Some(&s.focus));
        *self.sheet.borrow_mut() = Some(OpenSheet {
            window: s.window,
            _target: s.target,
            start: None,
        });
        self.edit_shortcuts(true);
    }

    /// Show the start sheet (saved and recent targets). The result arrives
    /// as `NativeEvent::Start`; the App then calls [`close_sheet`](Self::close_sheet).
    pub fn show_start(&self) {
        self.close_sheet();
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let s = start::build(mtm, self.emit.clone());
        self.window.beginSheet_completionHandler(&s.window, None);
        s.window.makeFirstResponder(Some(&s.focus));
        *self.sheet.borrow_mut() = Some(OpenSheet {
            window: s.window,
            _target: s.target,
            start: Some(s.ctl),
        });
        self.edit_shortcuts(true);
    }

    pub fn close_sheet(&self) {
        // Take it out first: endSheet may re-enter AppKit, which must not
        // find the RefCell still borrowed.
        let open = self.sheet.borrow_mut().take();
        if let Some(open) = open {
            if let Some(ctl) = &open.start {
                ctl.close_subs();
            }
            self.window.endSheet(&open.window);
            open.window.orderOut(None);
        }
        self.edit_shortcuts(false);
    }

    /// Edit-menu shortcuts on (a sheet's text fields need them) or off (the
    /// remote session gets Cmd+C/V/… as Ctrl+C/V/…).
    fn edit_shortcuts(&self, on: bool) {
        for (item, key) in &self.edit_items {
            item.setKeyEquivalent(&NSString::from_str(if on { key } else { "" }));
        }
    }

    pub fn sheet_open(&self) -> bool {
        self.sheet.borrow().is_some()
    }

    /// The start sheet is the open sheet.
    pub fn start_open(&self) -> bool {
        self.sheet
            .borrow()
            .as_ref()
            .is_some_and(|s| s.start.is_some())
    }

    /// The logon sheet is the open sheet.
    pub fn logon_open(&self) -> bool {
        self.sheet
            .borrow()
            .as_ref()
            .is_some_and(|s| s.start.is_none())
    }

    /// Push the current state into subtitle, toolbar and menus.
    pub fn update(&self, c: &Chrome) {
        if self.last.borrow().as_ref() == Some(c) {
            return;
        }
        self.window.setSubtitle(&NSString::from_str(&c.subtitle));
        *self.toolbar_delegate.ivars().state.borrow_mut() = c.clone();
        // The items actually in the toolbar (customization may add/remove).
        for item in self.toolbar.items().iter() {
            if let Some(a) = action_of_ident(&item.itemIdentifier().to_string()) {
                apply_item_state(&item, a, c);
            }
        }
        for (a, item) in &self.menu_items {
            item.setEnabled(c.enabled(*a));
            let on = if c.toggled(*a) {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            };
            item.setState(on);
            if *a == ToolbarAction::MonitorCycle {
                // Back to the plain label on a single-screen target, so the
                // last "(Screen: 2)" doesn't stick.
                let title = match &c.monitor {
                    Some(m) => format!("{} ({m})", label_of(*a)),
                    None => label_of(*a),
                };
                item.setTitle(&NSString::from_str(&title));
            }
        }
        *self.last.borrow_mut() = Some(c.clone());
    }
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<Sel>,
    key: &str,
) -> Retained<NSMenuItem> {
    // SAFETY: plain NSMenuItem initialiser with a valid selector (or none).
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        )
    }
}

fn submenu(mtm: MainThreadMarker, bar: &NSMenu, title: &str) -> Retained<NSMenu> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    menu.setAutoenablesItems(false);
    let top = menu_item(mtm, title, None, "");
    top.setSubmenu(Some(&menu));
    bar.addItem(&top);
    menu
}

/// Edit menu items: title key, selector, shortcut (Cmd+key; "Z" = Shift+Cmd+Z).
const EDIT_ITEMS: [(&str, &str, &str); 6] = [
    ("menu.undo", "undo:", "z"),
    ("menu.redo", "redo:", "Z"),
    ("menu.cut", "cut:", "x"),
    ("menu.copy", "copy:", "c"),
    ("menu.paste", "paste:", "v"),
    ("menu.select_all", "selectAll:", "a"),
];

type MenuItems = (
    Vec<(ToolbarAction, Retained<NSMenuItem>)>,
    Vec<(Retained<NSMenuItem>, &'static str)>,
);

/// Build and install the main menu; returns the action items for `update`
/// and the Edit items (whose shortcuts `edit_shortcuts` switches).
fn install_menu(mtm: MainThreadMarker, target: &ActionTarget) -> MenuItems {
    let app = NSApplication::sharedApplication(mtm);
    let bar = NSMenu::new(mtm);
    let mut actions = Vec::new();
    let mut add_action = |menu: &NSMenu, a: ToolbarAction, title: String, key: &str| {
        let item = menu_item(mtm, &title, Some(sel!(performAction:)), key);
        item.setTag(tag_of(a));
        // SAFETY: the target outlives the menu (NativeUi keeps both).
        unsafe { item.setTarget(Some(target)) };
        menu.addItem(&item);
        actions.push((a, item.clone()));
        item
    };

    // App menu (macOS shows the app name as its title).
    let app_menu = submenu(mtm, &bar, "SCCM RC");
    app_menu.setAutoenablesItems(true);
    app_menu.addItem(&menu_item(
        mtm,
        &t!("menu.about"),
        Some(sel!(orderFrontStandardAboutPanel:)),
        "",
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&menu_item(mtm, &t!("menu.hide"), Some(sel!(hide:)), "h"));
    let others = menu_item(
        mtm,
        &t!("menu.hide_others"),
        Some(sel!(hideOtherApplications:)),
        "h",
    );
    others
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Option | NSEventModifierFlags::Command);
    app_menu.addItem(&others);
    app_menu.addItem(&menu_item(
        mtm,
        &t!("menu.show_all"),
        Some(sel!(unhideAllApplications:)),
        "",
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Quit goes through the App (see `install_quit_handler`), not terminate:.
    let quit = menu_item(mtm, &t!("menu.quit"), Some(sel!(performQuit:)), "q");
    // SAFETY: the target outlives the menu (NativeUi keeps both).
    unsafe { quit.setTarget(Some(target)) };
    app_menu.addItem(&quit);

    // Edit: sent to the first responder (the sheet's text fields). Shortcuts
    // only while a sheet is open; see `NativeUi::edit_shortcuts`.
    let edit = submenu(mtm, &bar, &t!("menu.edit"));
    edit.setAutoenablesItems(true);
    let mut edit_items = Vec::new();
    for (i, (title, selector, key)) in EDIT_ITEMS.iter().enumerate() {
        if i == 2 {
            edit.addItem(&NSMenuItem::separatorItem(mtm));
        }
        let item = menu_item(
            mtm,
            &t!(*title),
            Some(Sel::register(&std::ffi::CString::new(*selector).unwrap())),
            "",
        );
        edit.addItem(&item);
        edit_items.push((item, *key));
    }

    let conn = submenu(mtm, &bar, &t!("menu.connection"));
    add_action(
        &conn,
        ToolbarAction::Disconnect,
        label_of(ToolbarAction::Disconnect),
        "",
    );

    let view = submenu(mtm, &bar, &t!("menu.view"));
    let fs = add_action(
        &view,
        ToolbarAction::ToggleFullscreen,
        label_of(ToolbarAction::ToggleFullscreen),
        "f",
    );
    fs.setKeyEquivalentModifierMask(NSEventModifierFlags::Control | NSEventModifierFlags::Command);
    add_action(
        &view,
        ToolbarAction::MonitorCycle,
        label_of(ToolbarAction::MonitorCycle),
        "",
    );
    view.addItem(&NSMenuItem::separatorItem(mtm));
    add_action(
        &view,
        ToolbarAction::ToggleViewOnly,
        label_of(ToolbarAction::ToggleViewOnly),
        "",
    );
    add_action(
        &view,
        ToolbarAction::ToggleCurtain,
        label_of(ToolbarAction::ToggleCurtain),
        "",
    );

    let control = submenu(mtm, &bar, &t!("menu.control"));
    for a in [
        ToolbarAction::CtrlAltDel,
        ToolbarAction::SendWin,
        ToolbarAction::SendKeys,
        ToolbarAction::SendFile,
    ] {
        add_action(&control, a, label_of(a), "");
    }
    control.addItem(&NSMenuItem::separatorItem(mtm));
    add_action(
        &control,
        ToolbarAction::ToggleRecord,
        label_of(ToolbarAction::ToggleRecord),
        "",
    );
    control.addItem(&NSMenuItem::separatorItem(mtm));
    add_action(
        &control,
        ToolbarAction::ToggleAgentControl,
        label_of(ToolbarAction::ToggleAgentControl),
        "",
    );

    let window_menu = submenu(mtm, &bar, &t!("menu.window"));
    window_menu.setAutoenablesItems(true);
    window_menu.addItem(&menu_item(
        mtm,
        &t!("menu.minimize"),
        Some(sel!(performMiniaturize:)),
        "",
    ));
    window_menu.addItem(&menu_item(
        mtm,
        &t!("menu.zoom"),
        Some(sel!(performZoom:)),
        "",
    ));
    app.setWindowsMenu(Some(&window_menu));

    let help = submenu(mtm, &bar, &t!("menu.help"));
    add_action(
        &help,
        ToolbarAction::BugReport,
        label_of(ToolbarAction::BugReport),
        "",
    );
    add_action(
        &help,
        ToolbarAction::About,
        t!("menu.about_details").to_string(),
        "",
    );
    app.setHelpMenu(Some(&help));

    app.setMainMenu(Some(&bar));
    (actions, edit_items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_identifiers_round_trip() {
        for a in ACTIONS {
            assert_eq!(ACTIONS[tag_of(a) as usize], a);
            assert_eq!(action_of_ident(ident_of(a)), Some(a));
        }
    }

    #[test]
    fn enabled_rules() {
        let idle = Chrome::default();
        assert!(!idle.enabled(ToolbarAction::CtrlAltDel));
        assert!(!idle.enabled(ToolbarAction::Disconnect));
        assert!(idle.enabled(ToolbarAction::About));
        let connecting = Chrome {
            has_session: true,
            ..Chrome::default()
        };
        assert!(connecting.enabled(ToolbarAction::Disconnect));
        assert!(!connecting.enabled(ToolbarAction::SendKeys));
        let live = Chrome {
            has_session: true,
            connected: true,
            ..Chrome::default()
        };
        assert!(live.enabled(ToolbarAction::SendKeys));
        assert!(!live.enabled(ToolbarAction::MonitorCycle), "single screen");
    }
}
