//! Non-macOS stand-in for `mac_ui`: there is no native chrome, so `install`
//! returns `None` and the self-drawn toolbar (`toolbar.rs`) stays in use.

use crate::{Chrome, NativeEvent, UpdateSheet};

pub struct NativeUi;

impl NativeUi {
    pub fn install(
        _window: &winit::window::Window,
        _on_event: Box<dyn Fn(NativeEvent)>,
    ) -> Option<Self> {
        None
    }

    pub fn update(&self, _c: &Chrome) {}

    pub fn show_logon(
        &self,
        _target: &str,
        _user: Option<String>,
        _error: Option<String>,
        _fill: bool,
        _remember: Option<bool>,
    ) {
    }

    pub fn show_start(&self) {}

    pub fn close_sheet(&self) {}

    pub fn sheet_open(&self) -> bool {
        false
    }

    pub fn start_open(&self) -> bool {
        false
    }

    pub fn logon_open(&self) -> bool {
        false
    }

    pub fn show_update(&self, _what: &UpdateSheet) {}

    pub fn update_open(&self) -> bool {
        false
    }

    pub fn update_busy(&self, _msg: &str) {}

    pub fn update_failed(&self, _msg: &str) {}

    pub fn set_auto_update(&self, _on: bool) {}
}
