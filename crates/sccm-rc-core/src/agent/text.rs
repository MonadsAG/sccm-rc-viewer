//! Layout-independent text input via Unicode fast-path events.

use crate::rdp::{FastPathInputEvent, KeyboardFlags};

const ENTER: u8 = 0x1C;

/// Encode `text` as Unicode key events. Returns the events plus the chars that
/// could not be sent (outside the BMP, or control characters other than `\n`).
pub fn encode_text(text: &str) -> (Vec<FastPathInputEvent>, Vec<char>) {
    let mut events = Vec::with_capacity(text.len() * 2);
    let mut skipped = Vec::new();
    for c in text.chars() {
        if c == '\n' {
            events.push(FastPathInputEvent::KeyboardEvent(
                KeyboardFlags::empty(),
                ENTER,
            ));
            events.push(FastPathInputEvent::KeyboardEvent(
                KeyboardFlags::RELEASE,
                ENTER,
            ));
        } else if c.is_control() || (c as u32) > 0xFFFF {
            skipped.push(c);
        } else {
            let code = c as u16;
            events.push(FastPathInputEvent::UnicodeKeyboardEvent(
                KeyboardFlags::empty(),
                code,
            ));
            events.push(FastPathInputEvent::UnicodeKeyboardEvent(
                KeyboardFlags::RELEASE,
                code,
            ));
        }
    }
    (events, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn umlaut_is_unicode_0xe4() {
        let (ev, skipped) = encode_text("ä");
        assert_eq!(
            ev,
            vec![
                FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), 0xE4),
                FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::RELEASE, 0xE4),
            ]
        );
        assert!(skipped.is_empty());
    }

    #[test]
    fn newline_is_enter_scancode() {
        let (ev, skipped) = encode_text("\n");
        assert_eq!(
            ev,
            vec![
                FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 0x1C),
                FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 0x1C),
            ]
        );
        assert!(skipped.is_empty());
    }

    #[test]
    fn emoji_and_controls_are_skipped_and_reported() {
        let (ev, skipped) = encode_text("a😀\u{7}b");
        assert_eq!(ev.len(), 4);
        assert_eq!(
            ev[0],
            FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), 'a' as u16)
        );
        assert_eq!(
            ev[2],
            FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), 'b' as u16)
        );
        assert_eq!(skipped, vec!['😀', '\u{7}']);
    }
}
