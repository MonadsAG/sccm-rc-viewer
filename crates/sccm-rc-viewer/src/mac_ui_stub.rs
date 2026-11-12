//! Non-macOS stand-in for `mac_ui`: there is no native chrome, so `install`
//! returns `None` and the self-drawn toolbar (`toolbar.rs`) stays in use.

use crate::NativeEvent;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chrome {
    pub subtitle: String,
    pub connected: bool,
    pub has_session: bool,
    pub view_only: bool,
    pub curtain: bool,
    pub recording: bool,
    pub fullscreen: bool,
    pub monitor: Option<String>,
}

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

    pub fn close_sheet(&self) {}

    pub fn sheet_open(&self) -> bool {
        false
    }
}
