//! Physical key → PS/2 scancode set 1, the code RDP keyboard events carry.
//!
//! On Windows, winit's `to_scancode()` already returns set-1 codes and the
//! viewer uses it directly. On macOS it returns Apple virtual keycodes
//! (kVK_*), which would make every key wrong on the remote, so this table maps
//! winit's layout-independent `KeyCode` (the physical key position) instead.
//! Layout handling (QWERTZ etc.) stays on the remote, exactly as with a real
//! PC keyboard.

use winit::keyboard::KeyCode;
#[cfg(target_os = "macos")]
use winit::keyboard::{NativeKeyCode, PhysicalKey};

/// macOS names the two ISO-only key positions by legend, not by place: on an
/// ISO keyboard the key left of `1` (`§`/`^`) sends `kVK_ISO_Section` (10),
/// which winit 0.30 leaves unidentified, and the key between left Shift and
/// `Z` (`<>`) sends `kVK_ANSI_Grave` (50, winit `Backquote`). Without this the
/// `<>` key types the wrong character on the remote and `§` types nothing.
/// Same swap as SDL/FreeRDP: on ISO hardware, 10 ↔ 50 before the lookup.
#[cfg(target_os = "macos")]
pub fn mac_iso_aware(key: &PhysicalKey) -> Option<(u8, bool)> {
    let raw: u16 = match key {
        PhysicalKey::Unidentified(NativeKeyCode::MacOS(10)) => 10,
        PhysicalKey::Code(KeyCode::Backquote) => 50,
        _ => return None,
    };
    let code = if mac_kbd::is_iso() { 60 - raw } else { raw };
    Some(if code == 10 {
        (0x56, false) // PC position of the ISO key left of Z
    } else {
        (0x29, false) // PC position of the key left of 1
    })
}

#[cfg(target_os = "macos")]
mod mac_kbd {
    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn LMGetKbdType() -> u8;
        fn KBGetLayoutType(i_keyboard_type: i16) -> u32;
    }

    /// `kKeyboardISO` ('ISO ') from HIToolbox.
    const KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");

    /// Physical layout of the keyboard that produced the last key event.
    /// Queried per event, so plugging in another keyboard just works.
    pub fn is_iso() -> bool {
        // SAFETY: plain C getters without arguments or pointers.
        unsafe { KBGetLayoutType(LMGetKbdType() as i16) == KEYBOARD_ISO }
    }
}

/// `(scancode, extended)` for a physical key, or `None` if RDP has no
/// equivalent (media keys, Fn, …).
pub fn set1(code: KeyCode) -> Option<(u8, bool)> {
    use KeyCode::*;
    let (sc, ext) = match code {
        Escape => (0x01, false),
        Digit1 => (0x02, false),
        Digit2 => (0x03, false),
        Digit3 => (0x04, false),
        Digit4 => (0x05, false),
        Digit5 => (0x06, false),
        Digit6 => (0x07, false),
        Digit7 => (0x08, false),
        Digit8 => (0x09, false),
        Digit9 => (0x0A, false),
        Digit0 => (0x0B, false),
        Minus => (0x0C, false),
        Equal => (0x0D, false),
        Backspace => (0x0E, false),
        Tab => (0x0F, false),
        KeyQ => (0x10, false),
        KeyW => (0x11, false),
        KeyE => (0x12, false),
        KeyR => (0x13, false),
        KeyT => (0x14, false),
        KeyY => (0x15, false),
        KeyU => (0x16, false),
        KeyI => (0x17, false),
        KeyO => (0x18, false),
        KeyP => (0x19, false),
        BracketLeft => (0x1A, false),
        BracketRight => (0x1B, false),
        Enter => (0x1C, false),
        ControlLeft => (0x1D, false),
        KeyA => (0x1E, false),
        KeyS => (0x1F, false),
        KeyD => (0x20, false),
        KeyF => (0x21, false),
        KeyG => (0x22, false),
        KeyH => (0x23, false),
        KeyJ => (0x24, false),
        KeyK => (0x25, false),
        KeyL => (0x26, false),
        Semicolon => (0x27, false),
        Quote => (0x28, false),
        Backquote => (0x29, false),
        ShiftLeft => (0x2A, false),
        Backslash => (0x2B, false),
        KeyZ => (0x2C, false),
        KeyX => (0x2D, false),
        KeyC => (0x2E, false),
        KeyV => (0x2F, false),
        KeyB => (0x30, false),
        KeyN => (0x31, false),
        KeyM => (0x32, false),
        Comma => (0x33, false),
        Period => (0x34, false),
        Slash => (0x35, false),
        ShiftRight => (0x36, false),
        NumpadMultiply => (0x37, false),
        AltLeft => (0x38, false),
        Space => (0x39, false),
        CapsLock => (0x3A, false),
        F1 => (0x3B, false),
        F2 => (0x3C, false),
        F3 => (0x3D, false),
        F4 => (0x3E, false),
        F5 => (0x3F, false),
        F6 => (0x40, false),
        F7 => (0x41, false),
        F8 => (0x42, false),
        F9 => (0x43, false),
        F10 => (0x44, false),
        NumLock => (0x45, false),
        ScrollLock => (0x46, false),
        Numpad7 => (0x47, false),
        Numpad8 => (0x48, false),
        Numpad9 => (0x49, false),
        NumpadSubtract => (0x4A, false),
        Numpad4 => (0x4B, false),
        Numpad5 => (0x4C, false),
        Numpad6 => (0x4D, false),
        NumpadAdd => (0x4E, false),
        Numpad1 => (0x4F, false),
        Numpad2 => (0x50, false),
        Numpad3 => (0x51, false),
        Numpad0 => (0x52, false),
        NumpadDecimal => (0x53, false),
        // ISO key between left Shift and Z (<> on German/Swiss keyboards;
        // on Mac ISO keyboards it is the § / ° key's neighbour).
        IntlBackslash => (0x56, false),
        F11 => (0x57, false),
        F12 => (0x58, false),
        NumpadEqual => (0x59, false),
        F13 => (0x64, false),
        F14 => (0x65, false),
        F15 => (0x66, false),
        F16 => (0x67, false),
        F17 => (0x68, false),
        F18 => (0x69, false),
        F19 => (0x6A, false),
        IntlRo => (0x73, false),
        IntlYen => (0x7D, false),
        NumpadComma => (0x7E, false),
        // Extended (E0-prefixed) keys.
        NumpadEnter => (0x1C, true),
        ControlRight => (0x1D, true),
        NumpadDivide => (0x35, true),
        PrintScreen => (0x37, true),
        AltRight => (0x38, true),
        Home => (0x47, true),
        ArrowUp => (0x48, true),
        PageUp => (0x49, true),
        ArrowLeft => (0x4B, true),
        ArrowRight => (0x4D, true),
        End => (0x4F, true),
        ArrowDown => (0x50, true),
        PageDown => (0x51, true),
        Insert => (0x52, true),
        Delete => (0x53, true),
        SuperLeft => (0x5B, true),
        SuperRight => (0x5C, true),
        ContextMenu => (0x5D, true),
        _ => return None,
    };
    Some((sc, ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_navigation() {
        assert_eq!(set1(KeyCode::KeyA), Some((0x1E, false)));
        assert_eq!(set1(KeyCode::KeyZ), Some((0x2C, false)));
        assert_eq!(set1(KeyCode::Digit0), Some((0x0B, false)));
        assert_eq!(set1(KeyCode::Enter), Some((0x1C, false)));
        assert_eq!(set1(KeyCode::NumpadEnter), Some((0x1C, true)));
        assert_eq!(set1(KeyCode::ArrowLeft), Some((0x4B, true)));
        assert_eq!(set1(KeyCode::Delete), Some((0x53, true)));
        assert_eq!(set1(KeyCode::F12), Some((0x58, false)));
        assert_eq!(set1(KeyCode::IntlBackslash), Some((0x56, false)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_iso_keys_get_distinct_pc_positions() {
        use winit::keyboard::{NativeKeyCode, PhysicalKey};
        let section = mac_iso_aware(&PhysicalKey::Unidentified(NativeKeyCode::MacOS(10)));
        let grave = mac_iso_aware(&PhysicalKey::Code(KeyCode::Backquote));
        let (section, grave) = (section.expect("mapped"), grave.expect("mapped"));
        assert_ne!(section, grave);
        for v in [section, grave] {
            assert!(v == (0x29, false) || v == (0x56, false), "{v:?}");
        }
        assert_eq!(mac_iso_aware(&PhysicalKey::Code(KeyCode::KeyA)), None);
    }

    #[test]
    fn super_keys_map_to_windows_keys() {
        assert_eq!(set1(KeyCode::SuperLeft), Some((0x5B, true)));
        assert_eq!(set1(KeyCode::SuperRight), Some((0x5C, true)));
    }

    #[test]
    fn no_scancode_collisions() {
        use std::collections::HashMap;
        let all = [
            KeyCode::Escape, KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4,
            KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9,
            KeyCode::Digit0, KeyCode::Minus, KeyCode::Equal, KeyCode::Backspace, KeyCode::Tab,
            KeyCode::KeyQ, KeyCode::KeyW, KeyCode::KeyE, KeyCode::KeyR, KeyCode::KeyT, KeyCode::KeyY,
            KeyCode::KeyU, KeyCode::KeyI, KeyCode::KeyO, KeyCode::KeyP, KeyCode::BracketLeft,
            KeyCode::BracketRight, KeyCode::Enter, KeyCode::ControlLeft, KeyCode::KeyA, KeyCode::KeyS,
            KeyCode::KeyD, KeyCode::KeyF, KeyCode::KeyG, KeyCode::KeyH, KeyCode::KeyJ, KeyCode::KeyK,
            KeyCode::KeyL, KeyCode::Semicolon, KeyCode::Quote, KeyCode::Backquote, KeyCode::ShiftLeft,
            KeyCode::Backslash, KeyCode::KeyZ, KeyCode::KeyX, KeyCode::KeyC, KeyCode::KeyV,
            KeyCode::KeyB, KeyCode::KeyN, KeyCode::KeyM, KeyCode::Comma, KeyCode::Period,
            KeyCode::Slash, KeyCode::ShiftRight, KeyCode::AltLeft, KeyCode::Space, KeyCode::CapsLock,
            KeyCode::F1, KeyCode::F5, KeyCode::F10, KeyCode::F11, KeyCode::F12, KeyCode::IntlBackslash,
            KeyCode::NumpadEnter, KeyCode::ControlRight, KeyCode::AltRight, KeyCode::Home,
            KeyCode::ArrowUp, KeyCode::PageUp, KeyCode::ArrowLeft, KeyCode::ArrowRight, KeyCode::End,
            KeyCode::ArrowDown, KeyCode::PageDown, KeyCode::Insert, KeyCode::Delete,
            KeyCode::SuperLeft, KeyCode::SuperRight,
        ];
        let mut seen: HashMap<(u8, bool), KeyCode> = HashMap::new();
        for k in all {
            let v = set1(k).expect("mapped");
            if let Some(prev) = seen.insert(v, k) {
                panic!("{k:?} and {prev:?} share scancode {v:?}");
            }
        }
    }
}
