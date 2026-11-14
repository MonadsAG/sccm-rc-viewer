//! Command/reply protocol shared by the MCP server and the viewer control
//! socket. Wire format: one JSON object per line.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::agent::shot::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Status,
    Screenshot {
        #[serde(default)]
        region: Option<Rect>,
        #[serde(default)]
        max_width: Option<u32>,
    },
    Click {
        x: f64,
        y: f64,
        #[serde(default)]
        relative: bool,
        #[serde(default)]
        button: Button,
        #[serde(default)]
        double: bool,
    },
    Move {
        x: f64,
        y: f64,
        #[serde(default)]
        relative: bool,
    },
    Scroll {
        x: f64,
        y: f64,
        amount: i16,
        #[serde(default)]
        relative: bool,
    },
    TypeText {
        text: String,
        #[serde(default)]
        secret: bool,
    },
    Key {
        combo: String,
        /// Part of a password (e.g. digit by digit): the combo is never
        /// logged, audited or echoed.
        #[serde(default)]
        secret: bool,
    },
    Batch {
        steps: Vec<Command>,
    },
    Wait {
        #[serde(default)]
        change: bool,
        #[serde(default)]
        stable_ms: Option<u64>,
        timeout_ms: u64,
    },
    Disconnect,
}

impl Command {
    /// Structural checks that serde cannot express.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Command::Batch { steps } => {
                if steps.is_empty() {
                    return Err("batch has no steps".into());
                }
                for s in steps {
                    if matches!(s, Command::Batch { .. }) {
                        return Err("batch steps cannot contain another batch".into());
                    }
                    s.validate()?;
                }
                Ok(())
            }
            Command::Wait {
                change: false,
                stable_ms: None,
                ..
            } => Err("wait needs change=true or stable_ms, otherwise it can only time out".into()),
            _ => Ok(()),
        }
    }
}

// Hand-written so a secret TypeText never reaches logs.
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Status => f.write_str("Status"),
            Command::Screenshot { region, max_width } => f
                .debug_struct("Screenshot")
                .field("region", region)
                .field("max_width", max_width)
                .finish(),
            Command::Click {
                x,
                y,
                relative,
                button,
                double,
            } => f
                .debug_struct("Click")
                .field("x", x)
                .field("y", y)
                .field("relative", relative)
                .field("button", button)
                .field("double", double)
                .finish(),
            Command::Move { x, y, relative } => f
                .debug_struct("Move")
                .field("x", x)
                .field("y", y)
                .field("relative", relative)
                .finish(),
            Command::Scroll {
                x,
                y,
                amount,
                relative,
            } => f
                .debug_struct("Scroll")
                .field("x", x)
                .field("y", y)
                .field("amount", amount)
                .field("relative", relative)
                .finish(),
            Command::TypeText { text, secret } => {
                let mut d = f.debug_struct("TypeText");
                if *secret {
                    d.field(
                        "text",
                        &format_args!("<secret, {} chars>", text.chars().count()),
                    );
                } else {
                    d.field("text", text);
                }
                d.field("secret", secret).finish()
            }
            Command::Key { combo, secret } => {
                let mut d = f.debug_struct("Key");
                if *secret {
                    d.field("combo", &format_args!("<secret>"));
                } else {
                    d.field("combo", combo);
                }
                d.field("secret", secret).finish()
            }
            Command::Batch { steps } => f.debug_struct("Batch").field("steps", steps).finish(),
            Command::Wait {
                change,
                stable_ms,
                timeout_ms,
            } => f
                .debug_struct("Wait")
                .field("change", change)
                .field("stable_ms", stable_ms)
                .field("timeout_ms", timeout_ms)
                .finish(),
            Command::Disconnect => f.write_str("Disconnect"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotConnected,
    InUse,
    ExistingSession,
    UserTookOver,
    ControlDisabled,
    BadArgs,
    Session,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Ok {
        text: String,
    },
    Image {
        png_base64: String,
        desk_w: u16,
        desk_h: u16,
        scale: f64,
    },
    Status {
        connected: bool,
        target: Option<String>,
        desk_w: u16,
        desk_h: u16,
        mode: String,
        idle_s: u64,
    },
    Error {
        code: ErrorCode,
        message: String,
        completed_steps: Option<usize>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::shot::Rect;

    fn all_commands() -> Vec<Command> {
        vec![
            Command::Status,
            Command::Screenshot {
                region: Some(Rect {
                    x: 1,
                    y: 2,
                    w: 3,
                    h: 4,
                }),
                max_width: Some(800),
            },
            Command::Screenshot {
                region: None,
                max_width: None,
            },
            Command::Click {
                x: 1.5,
                y: 2.0,
                relative: true,
                button: Button::Right,
                double: true,
            },
            Command::Move {
                x: 3.0,
                y: 4.0,
                relative: false,
            },
            Command::Scroll {
                x: 1.0,
                y: 2.0,
                amount: -3,
                relative: false,
            },
            Command::TypeText {
                text: "hi".into(),
                secret: true,
            },
            Command::Key {
                combo: "ctrl+c".into(),
                secret: false,
            },
            Command::Batch {
                steps: vec![
                    Command::Status,
                    Command::Key {
                        combo: "enter".into(),
                        secret: false,
                    },
                ],
            },
            Command::Wait {
                change: true,
                stable_ms: Some(300),
                timeout_ms: 5000,
            },
            Command::Disconnect,
        ]
    }

    #[test]
    fn round_trip_every_command() {
        for c in all_commands() {
            let s = serde_json::to_string(&c).unwrap();
            assert!(!s.contains('\n'));
            assert_eq!(serde_json::from_str::<Command>(&s).unwrap(), c, "{s}");
        }
    }

    #[test]
    fn optional_fields_default() {
        let c: Command = serde_json::from_str(r#"{"cmd":"click","x":1,"y":2}"#).unwrap();
        assert_eq!(
            c,
            Command::Click {
                x: 1.0,
                y: 2.0,
                relative: false,
                button: Button::Left,
                double: false
            }
        );
        let c: Command = serde_json::from_str(r#"{"cmd":"type_text","text":"a"}"#).unwrap();
        assert_eq!(
            c,
            Command::TypeText {
                text: "a".into(),
                secret: false
            }
        );
    }

    #[test]
    fn reply_round_trip() {
        let r = Reply::Error {
            code: ErrorCode::UserTookOver,
            message: "m".into(),
            completed_steps: Some(2),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("user_took_over"), "{s}");
        let _: Reply = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn secret_text_redacted_in_debug() {
        let c = Command::TypeText {
            text: "hunter2".into(),
            secret: true,
        };
        let d = format!("{c:?}");
        assert!(!d.contains("hunter2"), "{d}");
        assert!(d.contains("secret"), "{d}");
        let b = Command::Batch { steps: vec![c] };
        assert!(!format!("{b:?}").contains("hunter2"));
        let p = Command::TypeText {
            text: "visible".into(),
            secret: false,
        };
        assert!(format!("{p:?}").contains("visible"));
    }

    #[test]
    fn batch_nested_batch_rejected() {
        let c = Command::Batch {
            steps: vec![Command::Batch {
                steps: vec![Command::Status],
            }],
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rules() {
        assert!(Command::Batch { steps: vec![] }.validate().is_err());
        assert!(Command::Wait {
            change: false,
            stable_ms: None,
            timeout_ms: 100
        }
        .validate()
        .is_err());
        assert!(Command::Wait {
            change: true,
            stable_ms: None,
            timeout_ms: 100
        }
        .validate()
        .is_ok());
        assert!(Command::Wait {
            change: false,
            stable_ms: Some(1),
            timeout_ms: 100
        }
        .validate()
        .is_ok());
        for c in all_commands() {
            assert!(c.validate().is_ok(), "{c:?}");
        }
    }
}
