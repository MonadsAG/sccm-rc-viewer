//! Key-combo parser: `"Ctrl+Alt+Del"` → set-1 scancodes → fast-path events.

use crate::rdp::{FastPathInputEvent, KeyboardFlags};

/// One key as a set-1 scancode plus the extended (E0) prefix flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub scancode: u8,
    pub extended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("unknown key: {0}")]
    Unknown(String),
    #[error("empty key combo")]
    Empty,
}

const fn k(scancode: u8) -> Key {
    Key {
        scancode,
        extended: false,
    }
}
const fn ke(scancode: u8) -> Key {
    Key {
        scancode,
        extended: true,
    }
}

fn lookup(name: &str) -> Option<Key> {
    let n = name.to_ascii_lowercase();
    let key = match n.as_str() {
        "ctrl" => k(0x1D),
        "shift" => k(0x2A),
        "alt" => k(0x38),
        "win" => ke(0x5B),
        "enter" => k(0x1C),
        "esc" => k(0x01),
        "tab" => k(0x0F),
        "space" => k(0x39),
        "backspace" => k(0x0E),
        "delete" | "del" => ke(0x53),
        "insert" => ke(0x52),
        "home" => ke(0x47),
        "end" => ke(0x4F),
        "pageup" => ke(0x49),
        "pagedown" => ke(0x51),
        "up" => ke(0x48),
        "down" => ke(0x50),
        "left" => ke(0x4B),
        "right" => ke(0x4D),
        "num0" => k(0x52),
        "num1" => k(0x4F),
        "num2" => k(0x50),
        "num3" => k(0x51),
        "num4" => k(0x4B),
        "num5" => k(0x4C),
        "num6" => k(0x4D),
        "num7" => k(0x47),
        "num8" => k(0x48),
        "num9" => k(0x49),
        "numenter" => ke(0x1C),
        "numplus" => k(0x4E),
        "numminus" => k(0x4A),
        "nummultiply" => k(0x37),
        "numdivide" => ke(0x35),
        "numdecimal" => k(0x53),
        _ => return lookup_fkey_alnum(&n),
    };
    Some(key)
}

fn lookup_fkey_alnum(n: &str) -> Option<Key> {
    if let Some(num) = n.strip_prefix('f') {
        if let Ok(i) = num.parse::<u8>() {
            return match i {
                1..=10 => Some(k(0x3A + i)),
                11 => Some(k(0x57)),
                12 => Some(k(0x58)),
                _ => None,
            };
        }
    }
    let mut chars = n.chars();
    let (c, None) = (chars.next()?, chars.next()) else {
        return None;
    };
    const DIGITS: [u8; 10] = [0x0B, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A];
    match c {
        '0'..='9' => Some(k(DIGITS[(c as u8 - b'0') as usize])),
        'a'..='z' => {
            const LETTERS: [u8; 26] = [
                0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31,
                0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
            ];
            Some(k(LETTERS[(c as u8 - b'a') as usize]))
        }
        _ => None,
    }
}

/// Parse a `+`-separated combo (case-insensitive), e.g. `"Ctrl+Alt+Del"`.
pub fn parse_combo(combo: &str) -> Result<Vec<Key>, KeyError> {
    if combo.trim().is_empty() {
        return Err(KeyError::Empty);
    }
    combo
        .split('+')
        .map(|part| {
            let part = part.trim();
            lookup(part).ok_or_else(|| KeyError::Unknown(part.to_string()))
        })
        .collect()
}

/// Modifiers down in order, last key down/up, modifiers up in reverse.
pub fn press_combo(keys: &[Key]) -> Vec<FastPathInputEvent> {
    let ev = |key: &Key, release: bool| {
        let mut flags = KeyboardFlags::empty();
        if key.extended {
            flags |= KeyboardFlags::EXTENDED;
        }
        if release {
            flags |= KeyboardFlags::RELEASE;
        }
        FastPathInputEvent::KeyboardEvent(flags, key.scancode)
    };
    let Some((last, mods)) = keys.split_last() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(keys.len() * 2);
    out.extend(mods.iter().map(|m| ev(m, false)));
    out.push(ev(last, false));
    out.push(ev(last, true));
    out.extend(mods.iter().rev().map(|m| ev(m, true)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(s: &str) -> Key {
        parse_combo(s).unwrap()[0]
    }

    #[test]
    fn enter_is_0x1c() {
        assert_eq!(
            one("Enter"),
            Key {
                scancode: 0x1C,
                extended: false
            }
        );
    }

    #[test]
    fn num5_is_0x4c_not_extended() {
        assert_eq!(
            one("Num5"),
            Key {
                scancode: 0x4C,
                extended: false
            }
        );
    }

    #[test]
    fn num_enter_is_extended_0x1c() {
        assert_eq!(
            one("NumEnter"),
            Key {
                scancode: 0x1C,
                extended: true
            }
        );
    }

    #[test]
    fn ctrl_a_presses_and_releases_in_order() {
        let ev = press_combo(&parse_combo("Ctrl+A").unwrap());
        let (d, u) = (KeyboardFlags::empty(), KeyboardFlags::RELEASE);
        assert_eq!(
            ev,
            vec![
                FastPathInputEvent::KeyboardEvent(d, 0x1D),
                FastPathInputEvent::KeyboardEvent(d, 0x1E),
                FastPathInputEvent::KeyboardEvent(u, 0x1E),
                FastPathInputEvent::KeyboardEvent(u, 0x1D),
            ]
        );
    }

    #[test]
    fn unknown_key_errors() {
        assert_eq!(
            parse_combo("Ctrl+Foo"),
            Err(KeyError::Unknown("Foo".into()))
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(parse_combo("ctrl+alt+del"), parse_combo("Ctrl+Alt+Del"));
    }

    #[test]
    fn ctrl_alt_del_matches_viewer_sequence() {
        let (d, u, e) = (
            KeyboardFlags::empty(),
            KeyboardFlags::RELEASE,
            KeyboardFlags::EXTENDED,
        );
        assert_eq!(
            press_combo(&parse_combo("Ctrl+Alt+Del").unwrap()),
            vec![
                FastPathInputEvent::KeyboardEvent(d, 0x1D),
                FastPathInputEvent::KeyboardEvent(d, 0x38),
                FastPathInputEvent::KeyboardEvent(e, 0x53),
                FastPathInputEvent::KeyboardEvent(e | u, 0x53),
                FastPathInputEvent::KeyboardEvent(u, 0x38),
                FastPathInputEvent::KeyboardEvent(u, 0x1D),
            ]
        );
    }

    #[test]
    fn function_and_digit_keys() {
        assert_eq!(one("F1").scancode, 0x3B);
        assert_eq!(one("F10").scancode, 0x44);
        assert_eq!(one("F12").scancode, 0x58);
        assert_eq!(one("0").scancode, 0x0B);
        assert_eq!(one("1").scancode, 0x02);
        assert_eq!(one("Q").scancode, 0x10);
        assert_eq!(one("M").scancode, 0x32);
        assert!(parse_combo("F13").is_err());
    }
}
