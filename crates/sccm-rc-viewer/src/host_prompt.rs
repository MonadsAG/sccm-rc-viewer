//! In-window host-picker overlay ("start screen"). Shown when the viewer
//! starts without a CLI target and after the operator clicks Disconnect.
//!
//! One list: saved targets (`servers.rs`, shown by name) first, then recent
//! targets that are not saved. Typing filters the list by name or host.
//!
//! Keyboard:
//!   - printable char / Backspace → edit the field (filters the list)
//!   - ↑ / ↓ → select a row (the field keeps what was typed)
//!   - Enter → connect the selected row, else the typed text (a saved name
//!     is resolved to its host). Mouse hover only highlights a row; it never
//!     changes what Enter connects to.
//!   - Cmd/Ctrl+S → save the highlighted row or the typed host under a name
//!     (rename when it is saved already); Enter saves, Esc cancels
//!   - Cmd/Ctrl+Backspace → delete the highlighted saved target (press twice);
//!     with nothing highlighted it clears the field
//!   - Esc → leave naming / delete confirmation, else quit
//!
//! State-only: no windowing calls. `draw()` rasterises into the u32 ARGB
//! buffer; the App polls `take_outcome()` in `about_to_wait`.

use crate::recent;
use crate::servers::{self, Row, Server};
use crate::text::TextRenderer;
use crate::toolbar;
use rust_i18n::t;

const CARD_W: u32 = 520;
const ROW_H: u32 = 28;
const MAX_ROWS_SHOWN: usize = 8;
const FIELD_H: u32 = 36;
/// Card height: title, field, rows, scroll marker, hint.
const CARD_H: u32 = 68 + FIELD_H + 18 + MAX_ROWS_SHOWN as u32 * ROW_H + 14 + 44;
/// Width of one action link ("Rename", "Delete", "Save") at the row's right.
const ACTION_W: u32 = 96;

#[derive(Debug, Clone)]
pub enum PromptOutcome {
    Confirmed(String),
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    /// The field holds the name for `host`.
    Naming {
        host: String,
    },
    /// The saved entry for `host` is marked for deletion.
    ConfirmDelete {
        host: String,
    },
}

/// What a click on a row's action link does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Save,
    Delete,
}

pub struct HostPromptOverlay {
    /// Normal mode: the filter / target text. Naming mode: the name.
    input: String,
    /// The Normal-mode text while naming, restored afterwards.
    parked_input: String,
    servers: Vec<Server>,
    recents: Vec<String>,
    rows: Vec<Row>,
    /// Index into `visible()`, chosen with the arrow keys or a click;
    /// None = the text field is active.
    selected: Option<usize>,
    /// Row under the mouse: highlighted and shows its links, but Enter
    /// ignores it.
    hover: Option<usize>,
    /// Naming: the name the user was already warned about (taken by
    /// another host); saving it again replaces that entry.
    warned_name: Option<String>,
    /// Naming: the "name is taken" warning shown instead of the hint.
    notice: Option<String>,
    /// First visible row of `visible()`.
    scroll: usize,
    mode: Mode,
    /// Write changes to disk (false in tests).
    persist: bool,
    outcome: Option<PromptOutcome>,
    /// Anchor for the caret-blink phase; reset on every keystroke.
    caret_anchor: std::time::Instant,
}

impl HostPromptOverlay {
    pub fn new() -> Self {
        Self::with_data(servers::load(), recent::load(), true)
    }

    fn with_data(servers: Vec<Server>, recents: Vec<String>, persist: bool) -> Self {
        let mut o = Self {
            input: String::new(),
            parked_input: String::new(),
            servers,
            recents,
            rows: Vec::new(),
            selected: None,
            hover: None,
            warned_name: None,
            notice: None,
            scroll: 0,
            mode: Mode::Normal,
            persist,
            outcome: None,
            caret_anchor: std::time::Instant::now(),
        };
        o.rebuild();
        o
    }

    /// Saved targets first, then recents that are not saved.
    fn rebuild(&mut self) {
        self.rows = servers::rows(&self.servers, &self.recents);
    }

    /// The filter text: what was typed in Normal mode.
    fn filter(&self) -> &str {
        match self.mode {
            Mode::Naming { .. } => &self.parked_input,
            _ => &self.input,
        }
    }

    /// Indices into `rows` that match the filter.
    fn visible(&self) -> Vec<usize> {
        servers::filter(&self.rows, self.filter())
    }

    fn selected_row(&self) -> Option<&Row> {
        let vis = self.visible();
        self.selected
            .and_then(|i| vis.get(i))
            .map(|&r| &self.rows[r])
    }

    fn bump_caret(&mut self) {
        self.caret_anchor = std::time::Instant::now();
    }

    /// Any key other than a second delete leaves the delete confirmation.
    fn leave_confirm(&mut self) {
        if matches!(self.mode, Mode::ConfirmDelete { .. }) {
            self.mode = Mode::Normal;
        }
    }

    /// The field changed in Normal mode: the list is filtered anew.
    fn input_changed(&mut self) {
        if self.mode == Mode::Normal {
            self.selected = None;
            self.hover = None;
            self.scroll = 0;
        } else {
            // A different name: warn again if that one is taken too.
            self.warned_name = None;
            self.notice = None;
        }
        self.bump_caret();
    }

    pub fn on_text(&mut self, s: &str) {
        self.leave_confirm();
        self.input.extend(s.chars().filter(|c| !c.is_control()));
        self.input_changed();
    }

    pub fn on_backspace(&mut self) {
        self.leave_confirm();
        self.input.pop();
        self.input_changed();
    }

    /// Ctrl+A: clear the field (no visual selection is drawn, so "select
    /// all + type" and "clear" look the same).
    pub fn on_clear(&mut self) {
        self.leave_confirm();
        self.input.clear();
        self.input_changed();
    }

    fn select(&mut self, sel: Option<usize>) {
        self.selected = sel;
        if let Some(i) = sel {
            if i < self.scroll {
                self.scroll = i;
            } else if i >= self.scroll + MAX_ROWS_SHOWN {
                self.scroll = i + 1 - MAX_ROWS_SHOWN;
            }
        }
        self.bump_caret();
    }

    pub fn on_arrow_down(&mut self) {
        self.leave_confirm();
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        let n = self.visible().len();
        if n == 0 {
            return;
        }
        let next = match self.selected {
            None => 0,
            Some(i) => (i + 1).min(n - 1),
        };
        self.select(Some(next));
    }

    pub fn on_arrow_up(&mut self) {
        self.leave_confirm();
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        let prev = match self.selected {
            Some(0) | None => None,
            Some(i) => Some(i - 1),
        };
        self.select(prev);
    }

    pub fn on_enter(&mut self) {
        match self.mode.clone() {
            Mode::Naming { host } => self.finish_naming(&host),
            Mode::ConfirmDelete { .. } => self.mode = Mode::Normal,
            Mode::Normal => {
                if let Some(row) = self.selected_row() {
                    self.outcome = Some(PromptOutcome::Confirmed(row.host().to_string()));
                    return;
                }
                let typed = self.input.trim();
                if typed.is_empty() {
                    return;
                }
                let host =
                    servers::resolve_in(&self.servers, typed).unwrap_or_else(|| typed.to_string());
                self.outcome = Some(PromptOutcome::Confirmed(host));
            }
        }
    }

    pub fn on_esc(&mut self) {
        match self.mode {
            Mode::Naming { .. } => {
                self.input = std::mem::take(&mut self.parked_input);
                self.mode = Mode::Normal;
                self.bump_caret();
            }
            Mode::ConfirmDelete { .. } => self.mode = Mode::Normal,
            Mode::Normal => self.outcome = Some(PromptOutcome::Cancelled),
        }
    }

    /// Cmd/Ctrl+S: name the highlighted row, else the typed host (a typed
    /// saved name renames that entry).
    pub fn on_save(&mut self) {
        self.leave_confirm();
        if self.mode != Mode::Normal {
            return;
        }
        let host = match self.selected_row() {
            Some(row) => row.host().to_string(),
            None => {
                let typed = self.input.trim();
                if typed.is_empty() {
                    return;
                }
                servers::resolve_in(&self.servers, typed).unwrap_or_else(|| typed.to_string())
            }
        };
        let current = self
            .servers
            .iter()
            .find(|s| s.host.eq_ignore_ascii_case(&host))
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.parked_input = std::mem::replace(&mut self.input, current);
        self.mode = Mode::Naming { host };
        self.warned_name = None;
        self.notice = None;
        self.bump_caret();
    }

    fn finish_naming(&mut self, host: &str) {
        let name = self.input.trim().to_string();
        if name.is_empty() {
            return; // keep naming; Esc cancels
        }
        // A name used by another host: warn first, replace on the second Enter.
        if let Some(other) = servers::name_taken_by(&self.servers, &name, host) {
            let key = name.to_lowercase();
            if self.warned_name.as_deref() != Some(key.as_str()) {
                self.warned_name = Some(key);
                self.notice = Some(t!("prompt.name_taken", host => other).to_string());
                return;
            }
        }
        self.warned_name = None;
        self.notice = None;
        if servers::upsert_in(&mut self.servers, &name, host) && self.persist {
            servers::upsert(&name, host);
        }
        self.mode = Mode::Normal;
        // Back to the list with the saved row highlighted. Clear the filter
        // if it would hide the new name.
        self.input = std::mem::take(&mut self.parked_input);
        self.rebuild();
        let mut pos = self.position_of(host);
        if pos.is_none() {
            self.input.clear();
            pos = self.position_of(host);
        }
        self.scroll = 0;
        self.select(pos);
    }

    /// Position of `host` in the visible list.
    fn position_of(&self, host: &str) -> Option<usize> {
        self.visible()
            .iter()
            .position(|&r| self.rows[r].host().eq_ignore_ascii_case(host))
    }

    /// Cmd/Ctrl+Backspace: first press marks the highlighted saved target,
    /// the second deletes it. Nothing highlighted: clear the field.
    pub fn on_delete(&mut self) {
        match self.mode.clone() {
            Mode::Naming { .. } => {
                self.input.clear();
                self.bump_caret();
            }
            Mode::ConfirmDelete { host } => self.delete(&host),
            Mode::Normal => match self.selected_row().cloned() {
                Some(Row::Saved { host, .. }) => self.mode = Mode::ConfirmDelete { host },
                Some(Row::Recent { .. }) => {}
                None => self.on_clear(),
            },
        }
    }

    fn delete(&mut self, host: &str) {
        self.servers.retain(|s| !s.host.eq_ignore_ascii_case(host));
        if self.persist {
            servers::remove(host);
        }
        self.mode = Mode::Normal;
        self.hover = None;
        self.rebuild();
        // Keep the highlight on the same place, within the shorter list.
        let n = self.visible().len();
        let sel = self.selected.filter(|_| n > 0).map(|i| i.min(n - 1));
        self.scroll = self.scroll.min(n.saturating_sub(MAX_ROWS_SHOWN));
        self.select(sel);
    }

    /// Left-click: a row's action link, a row (connect), or the field.
    pub fn on_click(&mut self, x: f64, y: f64, win_w: u32, win_h: u32) {
        let l = layout(win_w, win_h);
        if l.text_field.contains(x, y) {
            self.leave_confirm();
            if self.mode == Mode::Normal {
                self.select(None);
            }
            return;
        }
        let Some((vis_idx, rect)) = self.row_at(&l, x, y) else {
            self.leave_confirm();
            return;
        };
        if self.mode != Mode::Normal && !matches!(self.mode, Mode::ConfirmDelete { .. }) {
            return; // naming: clicks on the list do nothing
        }
        let row = self.rows[self.visible()[vis_idx]].clone();
        // Links only count on the row that shows them (selected or hovered).
        let links_shown = self.selected == Some(vis_idx) || self.hover == Some(vis_idx);
        let action = if links_shown {
            action_at(&row, rect, x, y)
        } else {
            None
        };
        match action {
            Some(Action::Save) => {
                self.leave_confirm();
                self.select(Some(vis_idx));
                self.on_save();
            }
            Some(Action::Delete) => {
                let marked =
                    matches!(&self.mode, Mode::ConfirmDelete { host } if host == row.host());
                if !marked {
                    self.leave_confirm();
                }
                self.select(Some(vis_idx));
                self.on_delete();
            }
            None => {
                self.leave_confirm();
                self.outcome = Some(PromptOutcome::Confirmed(row.host().to_string()));
            }
        }
    }

    /// The visible-list index and rect of the row under `(x, y)`.
    fn row_at(&self, l: &Layout, x: f64, y: f64) -> Option<(usize, Rect)> {
        let n = self.visible().len();
        l.rows.iter().enumerate().find_map(|(slot, r)| {
            let idx = self.scroll + slot;
            (r.contains(x, y) && idx < n).then_some((idx, *r))
        })
    }

    /// Mouse-move: hover highlights a row (the field text is kept).
    pub fn on_mouse_move(&mut self, x: f64, y: f64, win_w: u32, win_h: u32) {
        if self.mode != Mode::Normal {
            return;
        }
        let l = layout(win_w, win_h);
        self.hover = self.row_at(&l, x, y).map(|(idx, _)| idx);
    }

    pub fn take_outcome(&mut self) -> Option<PromptOutcome> {
        self.outcome.take()
    }
}

/// Rectangle in window coordinates. `contains` uses f64 to match the winit
/// cursor position type.
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
            && x < (self.x as f64 + self.w as f64)
            && y < (self.y as f64 + self.h as f64)
    }
}

struct Layout {
    card_x: i32,
    card_y: i32,
    text_field: Rect,
    rows: Vec<Rect>,
}

/// Geometry shared by `draw()`, `on_click()` and `on_mouse_move()`.
fn layout(win_w: u32, win_h: u32) -> Layout {
    let card_x = (win_w / 2) as i32 - (CARD_W as i32) / 2;
    let card_y = (win_h / 2) as i32 - (CARD_H as i32) / 2;
    let tf_x = card_x + 24;
    let tf_y = card_y + 68;
    let tf_w = CARD_W - 48;
    let text_field = Rect {
        x: tf_x,
        y: tf_y,
        w: tf_w,
        h: FIELD_H,
    };
    let first = tf_y + FIELD_H as i32 + 18;
    let rows = (0..MAX_ROWS_SHOWN)
        .map(|i| Rect {
            x: tf_x,
            y: first + (i as u32 * ROW_H) as i32,
            w: tf_w,
            h: ROW_H,
        })
        .collect();
    Layout {
        card_x,
        card_y,
        text_field,
        rows,
    }
}

/// Action links at the right end of a row: saved rows get Rename + Delete,
/// recent rows get Save.
fn action_rects(row: &Row, r: Rect) -> Vec<(Action, Rect)> {
    let right = r.x + r.w as i32;
    let link = |i: i32| Rect {
        x: right - (i * ACTION_W as i32),
        y: r.y,
        w: ACTION_W,
        h: r.h,
    };
    match row {
        Row::Saved { .. } => vec![(Action::Save, link(2)), (Action::Delete, link(1))],
        Row::Recent { .. } => vec![(Action::Save, link(1))],
    }
}

fn action_at(row: &Row, r: Rect, x: f64, y: f64) -> Option<Action> {
    action_rects(row, r)
        .into_iter()
        .find(|(_, a)| a.contains(x, y))
        .map(|(a, _)| a)
}

/// "Cmd+" on macOS, "Ctrl+" elsewhere (the overlay font has no ⌘ glyph).
fn mod_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd+"
    } else {
        "Ctrl+"
    }
}

/// Name of the delete key as printed on the keyboard (Mac: "delete").
fn del_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Delete"
    } else {
        "Del"
    }
}

/// Rasterise the overlay into an ARGB u32 buffer: dim backdrop, centered card.
pub fn draw(
    buf: &mut [u32],
    w: u32,
    h: u32,
    font: Option<&TextRenderer>,
    overlay: &HostPromptOverlay,
) {
    for px in buf.iter_mut() {
        *px = 0x0020_2020;
    }
    let l = layout(w, h);
    let (card_x, card_y) = (l.card_x, l.card_y);
    fill_rect(buf, w, h, card_x, card_y, CARD_W, CARD_H, 0x002E_343C);
    stroke_rect(buf, w, h, card_x, card_y, CARD_W, CARD_H, 0x0050_5860);

    let title = match &overlay.mode {
        Mode::Naming { host } => t!("prompt.name_for", host => host).to_string(),
        _ => t!("prompt.label").to_string(),
    };
    if let Some(f) = font {
        f.draw_centered(buf, w, h, (card_y + 40) as f32, &title, 0x00E4_EAF0, 22.0);
    } else {
        toolbar::draw_text_centered(buf, w, h, (card_y + 32) as u32, &title, 0x00E4_EAF0, 2);
    }

    // Text field.
    let tf = l.text_field;
    fill_rect(buf, w, h, tf.x, tf.y, tf.w, tf.h, 0x001B_1E23);
    let field_active = overlay.selected.is_none() || matches!(overlay.mode, Mode::Naming { .. });
    let border = if field_active {
        0x0060_88B8
    } else {
        0x0045_4C55
    };
    stroke_rect(buf, w, h, tf.x, tf.y, tf.w, tf.h, border);
    let text_x = (tf.x + 10) as f32;
    let text_y = (tf.y + 6) as f32;
    let band = tf.h - 12;
    if overlay.input.is_empty() {
        let ph = match overlay.mode {
            Mode::Naming { .. } => t!("prompt.name_placeholder"),
            _ => t!("prompt.placeholder"),
        };
        draw_left(
            buf,
            w,
            h,
            text_x,
            text_y,
            band,
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
            text_x,
            text_y,
            band,
            &overlay.input,
            0x00E4_EAF0,
            18.0,
            font,
        );
    }
    if field_active && (overlay.caret_anchor.elapsed().as_millis() / 500).is_multiple_of(2) {
        let tw = if overlay.input.is_empty() {
            0.0
        } else {
            font.map(|f| f.width(&overlay.input, 18.0))
                .unwrap_or(overlay.input.chars().count() as f32 * 8.0)
        };
        fill_rect(
            buf,
            w,
            h,
            (text_x + tw) as i32,
            tf.y + 8,
            2,
            (tf.h - 16).max(1),
            0x00E4_EAF0,
        );
    }

    // Rows.
    let vis = overlay.visible();
    let link_col = 0x0080_B0E0;
    if overlay.rows.is_empty() {
        let empty = t!("prompt.empty", cmd => mod_label());
        let r = l.rows[0];
        draw_left(
            buf,
            w,
            h,
            (r.x + 12) as f32,
            r.y as f32,
            r.h,
            &empty,
            0x0070_7880,
            15.0,
            font,
        );
    }
    for (slot, &ri) in vis
        .iter()
        .skip(overlay.scroll)
        .take(MAX_ROWS_SHOWN)
        .enumerate()
    {
        let idx = overlay.scroll + slot;
        let r = l.rows[slot];
        let row = &overlay.rows[ri];
        let highlighted = overlay.selected == Some(idx) || overlay.hover == Some(idx);
        let marked = matches!(&overlay.mode, Mode::ConfirmDelete { host } if host == row.host());
        if marked {
            fill_rect(buf, w, h, r.x, r.y, r.w, r.h, 0x005A_2A2E);
        } else if highlighted {
            fill_rect(buf, w, h, r.x, r.y, r.w, r.h, 0x0038_4454);
        }
        let ty = r.y as f32;
        let left = (r.x + 12) as f32;
        if marked {
            let name = match row {
                Row::Saved { name, .. } => name.as_str(),
                Row::Recent { host } => host.as_str(),
            };
            let q = t!("prompt.confirm_delete", name => name);
            draw_left(buf, w, h, left, ty, r.h, &q, 0x00FF_D0D0, 16.0, font);
            let d = action_rects(row, r).last().map(|&(_, d)| d).unwrap_or(r);
            draw_left(
                buf,
                w,
                h,
                (d.x + 8) as f32,
                ty,
                r.h,
                &t!("prompt.delete"),
                0x00FF_8080,
                14.0,
                font,
            );
            continue;
        }
        let (main, sub) = match row {
            Row::Saved { name, host } => ((name.as_str(), 0x00F0_F4F8), Some(host.as_str())),
            Row::Recent { host } => ((host.as_str(), 0x00A8_B0B8), None),
        };
        draw_left(buf, w, h, left, ty, r.h, main.0, main.1, 17.0, font);
        let show_actions = highlighted && overlay.mode == Mode::Normal;
        if show_actions {
            for (a, ar) in action_rects(row, r) {
                let label = match (a, row) {
                    (Action::Save, Row::Saved { .. }) => t!("prompt.rename"),
                    (Action::Save, Row::Recent { .. }) => t!("prompt.save"),
                    (Action::Delete, _) => t!("prompt.delete"),
                };
                draw_left(
                    buf,
                    w,
                    h,
                    (ar.x + 8) as f32,
                    ty,
                    r.h,
                    &label,
                    link_col,
                    14.0,
                    font,
                );
            }
        } else if let (Some(host), Some(f)) = (sub, font) {
            // Host right-aligned, dimmed.
            let hw = f.width(host, 14.0);
            let hx = (r.x + r.w as i32 - 12) as f32 - hw;
            draw_left(buf, w, h, hx, ty, r.h, host, 0x0078_8088, 14.0, font);
        }
    }
    // More rows below / above: a small marker under the list.
    let below = vis.len().saturating_sub(overlay.scroll + MAX_ROWS_SHOWN);
    if below > 0 || overlay.scroll > 0 {
        let last = l.rows[MAX_ROWS_SHOWN - 1];
        let msg = t!("prompt.more", n => below + overlay.scroll);
        draw_left(
            buf,
            w,
            h,
            (last.x + 12) as f32,
            (last.y + ROW_H as i32) as f32,
            14,
            &msg,
            0x0070_7880,
            12.0,
            font,
        );
    }

    // Hint.
    let hint = match &overlay.mode {
        Mode::Naming { .. } if overlay.notice.is_some() => {
            overlay.notice.clone().unwrap_or_default().into()
        }
        Mode::Naming { .. } => t!("prompt.hint_naming"),
        Mode::ConfirmDelete { .. } => {
            t!("prompt.hint_confirm", cmd => mod_label(), del => del_label())
        }
        Mode::Normal => match overlay.selected_row() {
            Some(Row::Saved { .. }) => {
                t!("prompt.hint_saved", cmd => mod_label(), del => del_label())
            }
            _ => t!("prompt.hint", cmd => mod_label()),
        },
    };
    let hint_y = card_y + CARD_H as i32 - 30;
    if let Some(f) = font {
        f.draw_centered(buf, w, h, (hint_y + 12) as f32, &hint, 0x0070_7880, 14.0);
    } else {
        toolbar::draw_text_centered(buf, w, h, hint_y as u32, &hint, 0x0070_7880, 1);
    }
}

pub(crate) fn draw_left(
    buf: &mut [u32],
    w: u32,
    h: u32,
    x: f32,
    band_top: f32,
    band_h: u32,
    s: &str,
    color: u32,
    px: f32,
    font: Option<&TextRenderer>,
) {
    if let Some(f) = font {
        f.draw_vcenter(buf, w, h, x, band_top, band_h as f32, s, color, px);
    } else {
        toolbar::draw_text_scaled(buf, w, h, x as u32, band_top as u32, s, color, 1);
    }
}

pub(crate) fn fill_rect(
    buf: &mut [u32],
    w: u32,
    h: u32,
    x: i32,
    y: i32,
    rw: u32,
    rh: u32,
    color: u32,
) {
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = ((x + rw as i32).max(0) as u32).min(w);
    let y1 = ((y + rh as i32).max(0) as u32).min(h);
    for py in y0..y1 {
        let row = (py * w) as usize;
        for px in x0..x1 {
            buf[row + px as usize] = color;
        }
    }
}

pub(crate) fn stroke_rect(
    buf: &mut [u32],
    w: u32,
    h: u32,
    x: i32,
    y: i32,
    rw: u32,
    rh: u32,
    color: u32,
) {
    let w_i = w as i32;
    let h_i = h as i32;
    for i in 0..rw as i32 {
        let px = x + i;
        if px < 0 || px >= w_i {
            continue;
        }
        for &py in &[y, y + rh as i32 - 1] {
            if py < 0 || py >= h_i {
                continue;
            }
            buf[(py * w_i + px) as usize] = color;
        }
    }
    for i in 0..rh as i32 {
        let py = y + i;
        if py < 0 || py >= h_i {
            continue;
        }
        for &px in &[x, x + rw as i32 - 1] {
            if px < 0 || px >= w_i {
                continue;
            }
            buf[(py * w_i + px) as usize] = color;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srv(name: &str, host: &str) -> Server {
        Server {
            name: name.into(),
            host: host.into(),
        }
    }

    /// Two saved targets (one also in the recents) and two recents.
    fn overlay() -> HostPromptOverlay {
        HostPromptOverlay::with_data(
            vec![srv("Dev till", "192.0.2.10"), srv("Lab", "192.0.2.20")],
            vec!["192.0.2.10".into(), "192.0.2.30".into(), "pc-042".into()],
            false,
        )
    }

    fn confirmed(o: &mut HostPromptOverlay) -> Option<String> {
        match o.take_outcome() {
            Some(PromptOutcome::Confirmed(h)) => Some(h),
            _ => None,
        }
    }

    fn hosts(o: &HostPromptOverlay) -> Vec<String> {
        o.visible()
            .iter()
            .map(|&i| o.rows[i].host().to_string())
            .collect()
    }

    #[test]
    fn saved_first_then_unsaved_recents() {
        let o = overlay();
        assert_eq!(
            hosts(&o),
            ["192.0.2.10", "192.0.2.20", "192.0.2.30", "pc-042"]
        );
    }

    #[test]
    fn typing_filters_by_name_and_host() {
        let mut o = overlay();
        o.on_text("dev");
        assert_eq!(hosts(&o), ["192.0.2.10"]);
        o.on_clear();
        o.on_text("042");
        assert_eq!(hosts(&o), ["pc-042"]);
    }

    #[test]
    fn enter_connects_selection_typed_text_or_saved_name() {
        let mut o = overlay();
        o.on_arrow_down();
        o.on_arrow_down();
        assert!(o.input.is_empty(), "arrows must not overwrite the field");
        o.on_enter();
        assert_eq!(confirmed(&mut o).as_deref(), Some("192.0.2.20"));

        let mut o = overlay();
        o.on_text("lab");
        o.on_enter();
        assert_eq!(
            confirmed(&mut o).as_deref(),
            Some("192.0.2.20"),
            "name resolves"
        );

        let mut o = overlay();
        o.on_text("new-host");
        o.on_enter();
        assert_eq!(confirmed(&mut o).as_deref(), Some("new-host"));
    }

    #[test]
    fn scrolling_follows_the_selection() {
        let recents = (0..12).map(|i| format!("pc-{i:02}")).collect();
        let mut o = HostPromptOverlay::with_data(Vec::new(), recents, false);
        for _ in 0..10 {
            o.on_arrow_down();
        }
        assert_eq!(o.selected, Some(9));
        assert_eq!(o.scroll, 2);
        for _ in 0..9 {
            o.on_arrow_up();
        }
        assert_eq!((o.selected, o.scroll), (Some(0), 0));
    }

    #[test]
    fn save_a_recent_under_a_name() {
        let mut o = overlay();
        o.on_text("2.30");
        o.on_arrow_down();
        o.on_save();
        assert_eq!(
            o.mode,
            Mode::Naming {
                host: "192.0.2.30".into()
            }
        );
        assert!(o.input.is_empty());
        o.on_text("Store 3");
        o.on_enter();
        assert_eq!(o.mode, Mode::Normal);
        assert_eq!(o.input, "2.30", "the filter comes back");
        assert!(o.servers.contains(&srv("Store 3", "192.0.2.30")));
        assert_eq!(o.selected_row().map(|r| r.host()), Some("192.0.2.30"));
    }

    #[test]
    fn save_typed_host_and_rename_and_cancel() {
        let mut o = overlay();
        o.on_text("new-host");
        o.on_save();
        o.on_text("Brand new");
        o.on_enter();
        assert!(o.servers.contains(&srv("Brand new", "new-host")));

        // Rename: the field starts with the current name.
        let mut o = overlay();
        o.on_arrow_down();
        o.on_save();
        assert_eq!(o.input, "Dev till");
        o.on_clear();
        o.on_text("Dev till 2");
        o.on_enter();
        assert!(o.servers.contains(&srv("Dev till 2", "192.0.2.10")));
        assert_eq!(o.servers.len(), 2);

        // Esc cancels without saving; empty name does not save.
        let mut o = overlay();
        o.on_text("x-host");
        o.on_save();
        o.on_enter(); // empty name: stays in naming
        assert!(matches!(o.mode, Mode::Naming { .. }));
        o.on_esc();
        assert_eq!(o.mode, Mode::Normal);
        assert_eq!(o.input, "x-host");
        assert_eq!(o.servers.len(), 2);
        assert!(o.take_outcome().is_none(), "Esc in naming must not quit");
    }

    #[test]
    fn delete_needs_two_presses_and_other_keys_cancel() {
        let mut o = overlay();
        o.on_arrow_down(); // Dev till
        o.on_delete();
        assert_eq!(
            o.mode,
            Mode::ConfirmDelete {
                host: "192.0.2.10".into()
            }
        );
        o.on_arrow_down(); // any other key cancels
        assert_eq!(o.mode, Mode::Normal);
        assert_eq!(o.servers.len(), 2);

        o.on_arrow_up();
        o.on_delete();
        o.on_delete();
        assert_eq!(o.servers, vec![srv("Lab", "192.0.2.20")]);
        // The host is still a recent, so it stays in the list unnamed.
        assert!(hosts(&o).contains(&"192.0.2.10".to_string()));
    }

    #[test]
    fn delete_without_selection_clears_and_recents_are_not_deleted() {
        let mut o = overlay();
        o.on_text("abc");
        o.on_delete();
        assert!(o.input.is_empty());
        o.on_arrow_down();
        o.on_arrow_down();
        o.on_arrow_down(); // 192.0.2.30 (recent)
        o.on_delete();
        assert_eq!(o.mode, Mode::Normal);
    }

    #[test]
    fn click_actions_and_row_connect() {
        let mut o = overlay();
        let l = layout(800, 700);
        let r = l.rows[0];
        let (x, y) = ((r.x + 20) as f64, (r.y + 5) as f64);
        o.on_mouse_move(x, y, 800, 700);
        assert_eq!(o.hover, Some(0));
        assert_eq!(o.selected, None, "hover does not select");
        // Delete link twice.
        let (_, d) = action_rects(&o.rows[0], r)[1];
        let (dx, dy) = ((d.x + 5) as f64, (d.y + 5) as f64);
        o.on_click(dx, dy, 800, 700);
        assert!(matches!(o.mode, Mode::ConfirmDelete { .. }));
        o.on_click(dx, dy, 800, 700);
        assert_eq!(o.servers.len(), 1);
        // Clicking the row body connects.
        o.on_click(x, y, 800, 700);
        assert!(confirmed(&mut o).is_some());
    }

    #[test]
    fn hover_never_changes_what_enter_connects() {
        let mut o = overlay();
        let r = layout(800, 700).rows[0];
        o.on_text("pc-new");
        o.on_mouse_move((r.x + 20) as f64, (r.y + 5) as f64, 800, 700);
        o.on_enter();
        assert_eq!(confirmed(&mut o).as_deref(), Some("pc-new"));
    }

    #[test]
    fn hidden_links_are_not_clickable() {
        let mut o = overlay();
        let r = layout(800, 700).rows[1];
        let (_, d) = action_rects(&o.rows[1], r)[1];
        // Mouse never hovered row 1: a click on its right end connects.
        o.on_click((d.x + 5) as f64, (d.y + 5) as f64, 800, 700);
        assert_eq!(confirmed(&mut o).as_deref(), Some("192.0.2.20"));
        assert_eq!(o.servers.len(), 2);
    }

    #[test]
    fn taken_name_warns_then_replaces() {
        let mut o = overlay();
        o.on_text("pc-new");
        o.on_save();
        o.on_text("lab");
        o.on_enter();
        assert!(
            matches!(o.mode, Mode::Naming { .. }),
            "first Enter only warns"
        );
        assert!(o.notice.is_some());
        o.on_enter();
        assert_eq!(o.mode, Mode::Normal);
        assert!(o.servers.contains(&srv("lab", "pc-new")));
        assert!(
            !o.servers.iter().any(|s| s.host == "192.0.2.20"),
            "old Lab replaced"
        );

        // Changing the name after the warning warns again for the new name.
        let mut o = overlay();
        o.on_text("pc-new");
        o.on_save();
        o.on_text("Lab");
        o.on_enter();
        o.on_backspace();
        o.on_text("b");
        assert!(o.notice.is_none());
        o.on_enter();
        assert!(matches!(o.mode, Mode::Naming { .. }));
    }

    #[test]
    fn esc_in_normal_mode_quits() {
        let mut o = overlay();
        o.on_esc();
        assert!(matches!(o.take_outcome(), Some(PromptOutcome::Cancelled)));
    }
}
