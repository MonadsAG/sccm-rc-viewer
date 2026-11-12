//! Native start sheet: a search field over a list of saved targets (by name)
//! and recent ones, with Connect, Save…/Rename…, Delete and Quit.
//!
//! Same model as the drawn start screen (`servers::rows` / `servers::filter`).
//! Typing filters; ↑/↓ in the search field move the selection; Enter connects
//! the selection or the typed text (a saved name resolves to its host);
//! double-click connects a row. Save/Rename and Delete open a small sheet on
//! top. The result goes out as `NativeEvent::Start(PromptOutcome)`.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSButton, NSColor, NSControlTextEditingDelegate, NSEventModifierFlags, NSScrollView,
    NSSearchField, NSTableColumn, NSTableView, NSTableViewDataSource, NSTableViewDelegate,
    NSTableViewStyle, NSTextAlignment, NSTextField, NSView, NSWindow,
};
use objc2_foundation::{
    NSIndexSet, NSInteger, NSNotification, NSObject, NSObjectProtocol, NSString,
};
use rust_i18n::t;

use super::sheet::{self, rect, SheetTarget};
use crate::host_prompt::PromptOutcome;
use crate::servers::{self, Row, Server};
use crate::{recent, NativeEvent};

const W: f64 = 520.0;
const H: f64 = 440.0;

pub struct ListIvars {
    rows: RefCell<Vec<Row>>,
    on_select: RefCell<Option<Box<dyn Fn()>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ListIvars]
    pub struct ListSource;

    unsafe impl NSObjectProtocol for ListSource {}

    // Required by NSTableViewDelegate; no methods needed.
    unsafe impl NSControlTextEditingDelegate for ListSource {}

    unsafe impl NSTableViewDataSource for ListSource {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn row_count(&self, _table: &NSTableView) -> NSInteger {
            self.ivars().rows.borrow().len() as NSInteger
        }
    }

    unsafe impl NSTableViewDelegate for ListSource {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for(
            &self,
            _table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            self.cell(column, row)
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_changed(&self, _n: &NSNotification) {
            if let Some(f) = self.ivars().on_select.borrow().as_ref() {
                f();
            }
        }
    }
);

impl ListSource {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ListIvars {
            rows: RefCell::new(Vec::new()),
            on_select: RefCell::new(None),
        });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }

    /// Name column: the saved name (or the host of a recent, dimmed).
    /// Host column: the host of a saved target, right-aligned and dimmed.
    fn cell(&self, column: Option<&NSTableColumn>, row: NSInteger) -> Option<Retained<NSView>> {
        let rows = self.ivars().rows.borrow();
        let r = rows.get(usize::try_from(row).ok()?)?;
        let host_col = column.is_some_and(|c| c.identifier().to_string() == "host");
        let (text, dim) = match (r, host_col) {
            (Row::Saved { name, .. }, false) => (name.as_str(), false),
            (Row::Saved { host, .. }, true) => (host.as_str(), true),
            (Row::Recent { host }, false) => (host.as_str(), true),
            (Row::Recent { .. }, true) => ("", true),
        };
        let l = NSTextField::labelWithString(&NSString::from_str(text), self.mtm());
        if dim {
            l.setTextColor(Some(&NSColor::secondaryLabelColor()));
        }
        if host_col {
            l.setAlignment(NSTextAlignment::Right);
        }
        Some(Retained::into_super(Retained::into_super(l)))
    }
}

/// The start sheet's state and controls.
pub struct Ctl {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    search: Retained<NSTextField>,
    table: Retained<NSTableView>,
    source: Retained<ListSource>,
    save: Retained<NSButton>,
    delete: Retained<NSButton>,
    connect: Retained<NSButton>,
    empty: Retained<NSTextField>,
    servers: RefCell<Vec<Server>>,
    recents: Vec<String>,
    /// Name / delete sheets on top. Kept (not dropped) after closing: they
    /// close from inside their own button callbacks.
    subs: RefCell<Vec<(Retained<NSWindow>, Retained<SheetTarget>)>>,
    emit: Rc<dyn Fn(NativeEvent)>,
}

impl Ctl {
    fn refresh(&self, select_host: Option<&str>) {
        let all = servers::rows(&self.servers.borrow(), &self.recents);
        let visible: Vec<Row> = servers::filter(&all, &sheet::text_of(&self.search))
            .into_iter()
            .map(|i| all[i].clone())
            .collect();
        let pos = select_host.and_then(|h| {
            visible
                .iter()
                .position(|r| r.host().eq_ignore_ascii_case(h))
        });
        *self.source.ivars().rows.borrow_mut() = visible;
        self.table.reloadData();
        match pos {
            Some(i) => self.select(i),
            // SAFETY: plain AppKit call on the main thread.
            None => unsafe { self.table.deselectAll(None) },
        }
        self.empty.setHidden(!all.is_empty());
        self.update_buttons();
    }

    fn select(&self, i: usize) {
        let set = NSIndexSet::indexSetWithIndex(i);
        self.table
            .selectRowIndexes_byExtendingSelection(&set, false);
        self.table.scrollRowToVisible(i as NSInteger);
    }

    fn selected(&self) -> Option<Row> {
        let i = usize::try_from(self.table.selectedRow()).ok()?;
        self.source.ivars().rows.borrow().get(i).cloned()
    }

    fn typed(&self) -> String {
        sheet::text_of(&self.search).trim().to_string()
    }

    fn update_buttons(&self) {
        let sel = self.selected();
        let typed = !self.typed().is_empty();
        let title = match sel {
            Some(Row::Saved { .. }) => t!("prompt.rename_dots"),
            _ => t!("prompt.save_dots"),
        };
        self.save.setTitle(&NSString::from_str(&title));
        self.save.setEnabled(sel.is_some() || typed);
        self.delete
            .setEnabled(matches!(sel, Some(Row::Saved { .. })));
        self.connect.setEnabled(sel.is_some() || typed);
    }

    /// The host to connect / save: the selection, else the typed text (a
    /// saved name resolves to its host).
    fn target_host(&self) -> Option<String> {
        if let Some(r) = self.selected() {
            return Some(r.host().to_string());
        }
        let typed = self.typed();
        if typed.is_empty() {
            return None;
        }
        Some(servers::resolve_in(&self.servers.borrow(), &typed).unwrap_or(typed))
    }

    fn connect(&self) {
        if let Some(h) = self.target_host() {
            (self.emit)(NativeEvent::Start(PromptOutcome::Confirmed(h)));
        }
    }

    /// ↑/↓ from the search field.
    fn move_selection(&self, down: bool) {
        let n = self.source.ivars().rows.borrow().len();
        if n == 0 {
            return;
        }
        match (usize::try_from(self.table.selectedRow()).ok(), down) {
            (None, true) => self.select(0),
            (None, false) => {}
            // SAFETY: as above.
            (Some(0), false) => unsafe { self.table.deselectAll(None) },
            (Some(i), false) => self.select(i - 1),
            (Some(i), true) => self.select((i + 1).min(n - 1)),
        }
        self.update_buttons();
    }

    fn open_sub(&self, win: Retained<NSWindow>, target: Retained<SheetTarget>) {
        // Earlier subs are closed and not running any callback: safe to drop.
        self.subs.borrow_mut().clear();
        self.window.beginSheet_completionHandler(&win, None);
        self.subs.borrow_mut().push((win, target));
    }

    fn close_sub(&self) {
        if let Some((w, _)) = self.subs.borrow().last() {
            self.window.endSheet(w);
            w.orderOut(None);
        }
    }

    pub fn close_subs(&self) {
        for (w, _) in self.subs.borrow().iter() {
            self.window.endSheet(w);
            w.orderOut(None);
        }
    }

    /// Save or rename the target host under a name.
    fn ask_name(self: &Rc<Self>) {
        let Some(host) = self.target_host() else {
            return;
        };
        let mtm = self.mtm;
        let current = self
            .servers
            .borrow()
            .iter()
            .find(|s| s.host.eq_ignore_ascii_case(&host))
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let (w, h) = (420.0, 150.0);
        let win = sheet::sheet_window(mtm, w, h);
        let view = win.contentView().expect("content view");
        let label = sheet::label(
            mtm,
            &t!("prompt.name_for", host => &host),
            rect(20.0, h - 40.0, w - 40.0, 18.0),
        );
        let field = sheet::text_field(mtm, rect(20.0, h - 74.0, w - 40.0, 24.0));
        sheet::set_text(&field, &current);
        field.setPlaceholderString(Some(&NSString::from_str(&t!("prompt.name_placeholder"))));
        let cancel = sheet::push_button(
            mtm,
            &t!("prompt.cancel"),
            rect(w - 250.0, 12.0, 110.0, 32.0),
            "\u{1b}",
        );
        let ok = sheet::push_button(
            mtm,
            &t!("prompt.save"),
            rect(w - 130.0, 12.0, 110.0, 32.0),
            "\r",
        );
        for v in [&*label as &NSView, &field, &cancel, &ok] {
            sheet::add(&view, v);
        }
        let target = SheetTarget::new(mtm);
        let me = Rc::downgrade(self);
        target.on_click(
            &cancel,
            Box::new(move || {
                if let Some(c) = me.upgrade() {
                    c.close_sub();
                }
            }),
        );
        let me = Rc::downgrade(self);
        let f = field.clone();
        target.on_click(
            &ok,
            Box::new(move || {
                let Some(c) = me.upgrade() else { return };
                let name = sheet::text_of(&f).trim().to_string();
                if name.is_empty() {
                    return;
                }
                if servers::upsert_in(&mut c.servers.borrow_mut(), &name, &host) {
                    servers::upsert(&name, &host);
                }
                c.close_sub();
                c.refresh(Some(&host));
                if c.selected().is_none() {
                    // The filter hides the new name: show everything.
                    sheet::set_text(&c.search, "");
                    c.refresh(Some(&host));
                }
            }),
        );
        self.open_sub(win.clone(), target);
        win.makeFirstResponder(Some(&field));
    }

    /// Delete the selected saved target after a confirmation.
    fn confirm_delete(self: &Rc<Self>) {
        let Some(Row::Saved { name, host }) = self.selected() else {
            return;
        };
        let mtm = self.mtm;
        let (w, h) = (420.0, 120.0);
        let win = sheet::sheet_window(mtm, w, h);
        let view = win.contentView().expect("content view");
        let label = sheet::title_label(
            mtm,
            &t!("prompt.confirm_delete", name => &name),
            rect(20.0, h - 44.0, w - 40.0, 22.0),
        );
        let cancel = sheet::push_button(
            mtm,
            &t!("prompt.cancel"),
            rect(w - 250.0, 12.0, 110.0, 32.0),
            "\u{1b}",
        );
        let ok = sheet::push_button(
            mtm,
            &t!("prompt.delete"),
            rect(w - 130.0, 12.0, 110.0, 32.0),
            "\r",
        );
        ok.setHasDestructiveAction(true);
        for v in [&*label as &NSView, &cancel, &ok] {
            sheet::add(&view, v);
        }
        let target = SheetTarget::new(mtm);
        let me = Rc::downgrade(self);
        target.on_click(
            &cancel,
            Box::new(move || {
                if let Some(c) = me.upgrade() {
                    c.close_sub();
                }
            }),
        );
        let me = Rc::downgrade(self);
        target.on_click(
            &ok,
            Box::new(move || {
                let Some(c) = me.upgrade() else { return };
                c.servers
                    .borrow_mut()
                    .retain(|s| !s.host.eq_ignore_ascii_case(&host));
                servers::remove(&host);
                c.close_sub();
                c.refresh(Some(&host));
            }),
        );
        self.open_sub(win, target);
    }
}

pub struct StartSheet {
    pub window: Retained<NSWindow>,
    pub target: Retained<SheetTarget>,
    pub ctl: Rc<Ctl>,
    pub focus: Retained<NSTextField>,
}

pub fn build(mtm: MainThreadMarker, emit: Rc<dyn Fn(NativeEvent)>) -> StartSheet {
    let window = sheet::sheet_window(mtm, W, H);
    let view = window.contentView().expect("sheet content view");
    let m = 20.0;
    let fw = W - 2.0 * m;

    let title = sheet::title_label(mtm, &t!("prompt.native_title"), rect(m, H - 42.0, fw, 22.0));
    let search_field =
        NSSearchField::initWithFrame(NSSearchField::alloc(mtm), rect(m, H - 80.0, fw, 28.0));
    let search: Retained<NSTextField> = Retained::into_super(search_field);
    search.setPlaceholderString(Some(&NSString::from_str(&t!("prompt.placeholder"))));

    let list_top = H - 92.0;
    let list_bottom = 60.0;
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        rect(m, list_bottom, fw, list_top - list_bottom),
    );
    scroll.setHasVerticalScroller(true);
    let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), rect(0.0, 0.0, fw, 10.0));
    let name_col =
        NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &NSString::from_str("name"));
    name_col.setWidth(fw - 200.0);
    let host_col =
        NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &NSString::from_str("host"));
    host_col.setWidth(140.0);
    table.addTableColumn(&name_col);
    table.addTableColumn(&host_col);
    table.setHeaderView(None);
    table.setStyle(NSTableViewStyle::Inset);
    table.setRowHeight(26.0);
    scroll.setDocumentView(Some(&table));

    let empty = sheet::note_label(
        mtm,
        &t!("prompt.empty_native"),
        rect(
            m + 20.0,
            (list_top + list_bottom) / 2.0 - 10.0,
            fw - 40.0,
            20.0,
        ),
    );
    empty.setAlignment(NSTextAlignment::Center);
    empty.setHidden(true);

    let save = sheet::push_button(
        mtm,
        &t!("prompt.save_dots"),
        rect(m, 14.0, 130.0, 32.0),
        "s",
    );
    save.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
    let delete = sheet::push_button(
        mtm,
        &t!("prompt.delete"),
        rect(m + 138.0, 14.0, 100.0, 32.0),
        "\u{8}",
    );
    delete.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
    let quit = sheet::push_button(
        mtm,
        &t!("prompt.quit"),
        rect(W - m - 218.0, 14.0, 100.0, 32.0),
        "\u{1b}",
    );
    let connect = sheet::push_button(
        mtm,
        &t!("prompt.connect"),
        rect(W - m - 110.0, 14.0, 110.0, 32.0),
        "\r",
    );

    for v in [
        &*title as &NSView,
        &search,
        &scroll,
        &empty,
        &save,
        &delete,
        &quit,
        &connect,
    ] {
        sheet::add(&view, v);
    }

    let source = ListSource::new(mtm);
    // SAFETY: the source outlives the table (the sheet keeps both via Ctl).
    unsafe {
        table.setDataSource(Some(ProtocolObject::from_ref(&*source)));
        table.setDelegate(Some(ProtocolObject::from_ref(&*source)));
    }

    let ctl = Rc::new(Ctl {
        mtm,
        window: window.clone(),
        search: search.clone(),
        table: table.clone(),
        source: source.clone(),
        save: save.clone(),
        delete: delete.clone(),
        connect: connect.clone(),
        empty,
        servers: RefCell::new(servers::load()),
        recents: recent::load(),
        subs: RefCell::new(Vec::new()),
        emit: emit.clone(),
    });

    let weak = |c: &Rc<Ctl>| -> Weak<Ctl> { Rc::downgrade(c) };
    let target = SheetTarget::new(mtm);
    {
        let me = weak(&ctl);
        *source.ivars().on_select.borrow_mut() = Some(Box::new(move || {
            if let Some(c) = me.upgrade() {
                c.update_buttons();
            }
        }));
    }
    {
        let me = weak(&ctl);
        target.on_click(
            &connect,
            Box::new(move || {
                if let Some(c) = me.upgrade() {
                    c.connect();
                }
            }),
        );
        let me = weak(&ctl);
        target.on_double_click(
            &table,
            Box::new(move || {
                if let Some(c) = me.upgrade() {
                    if c.table.clickedRow() >= 0 {
                        c.connect();
                    }
                }
            }),
        );
        let me = weak(&ctl);
        target.on_click(
            &save,
            Box::new(move || {
                if let Some(c) = me.upgrade() {
                    c.ask_name();
                }
            }),
        );
        let me = weak(&ctl);
        target.on_click(
            &delete,
            Box::new(move || {
                if let Some(c) = me.upgrade() {
                    c.confirm_delete();
                }
            }),
        );
        let emit = emit.clone();
        target.on_click(
            &quit,
            Box::new(move || emit(NativeEvent::Start(PromptOutcome::Cancelled))),
        );
    }
    {
        let me = weak(&ctl);
        target.on_text(Box::new(move |_obj, ended| {
            if let (false, Some(c)) = (ended, me.upgrade()) {
                c.refresh(None);
            }
        }));
        let me = weak(&ctl);
        target.on_command(Box::new(move |cmd| {
            let Some(c) = me.upgrade() else { return false };
            match cmd {
                "moveDown:" => {
                    c.move_selection(true);
                    true
                }
                "moveUp:" => {
                    c.move_selection(false);
                    true
                }
                _ => false,
            }
        }));
    }
    target.watch(&search);
    ctl.refresh(None);

    StartSheet {
        window,
        target,
        ctl,
        focus: search,
    }
}
