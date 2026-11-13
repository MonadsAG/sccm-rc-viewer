//! Tool schemas and the mapping from tool arguments to backend calls.

use sccm_rc_core::agent::proto::{Command, Reply};
use serde_json::{json, Map, Value};

/// A validated tool invocation.
pub enum Call {
    Connect { target: String, force: bool },
    Run(Command),
}

const STEP_TYPES: [&str; 6] = ["click", "move", "scroll", "type_text", "key", "wait"];

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        }
    })
}

pub fn list() -> Vec<Value> {
    let rel = json!({"type":"boolean","description":"If true, x and y are fractions 0..1 of the desktop instead of pixels"});
    let x = json!({"type":"number","description":"X in the last screenshot's image space (0..1 if relative)"});
    let y = json!({"type":"number","description":"Y in the last screenshot's image space (0..1 if relative)"});
    let step_props = json!({
        "type": {"type":"string","enum": STEP_TYPES},
        "x": x, "y": y, "relative": rel,
        "button": {"type":"string","enum":["left","right","middle"]},
        "double": {"type":"boolean"},
        "amount": {"type":"integer"},
        "text": {"type":"string"}, "secret": {"type":"boolean"},
        "combo": {"type":"string"},
        "change": {"type":"boolean"},
        "stable_ms": {"type":"integer","minimum":1},
        "timeout_ms": {"type":"integer","minimum":0}
    });
    vec![
        tool(
            "connect",
            "Connect to a till by hostname or IP. Fails if a user is logged on unless force=true, which takes the till over from that user.",
            json!({
                "target": {"type":"string","description":"Hostname or IP of the till"},
                "force": {"type":"boolean","description":"Take over a till where a user is logged on (default false)"}
            }),
            &["target"],
        ),
        tool(
            "disconnect",
            "Disconnect from the till (headless mode). In live mode only hands control back; the viewer stays connected.",
            json!({}),
            &[],
        ),
        tool(
            "screenshot",
            "Capture the till desktop as a PNG, scaled down to max_width (default 1280). Coordinates for click, move and scroll refer to the most recent screenshot's image space.",
            json!({
                "max_width": {"type":"integer","minimum":1,"description":"Maximum image width in pixels (default 1280)"},
                "region": {
                    "type":"object",
                    "description":"Only capture this rectangle, in desktop pixels",
                    "properties": {
                        "x":{"type":"integer"},"y":{"type":"integer"},
                        "w":{"type":"integer"},"h":{"type":"integer"}
                    },
                    "required":["x","y","w","h"]
                }
            }),
            &[],
        ),
        tool(
            "click",
            "Click at a position. Coordinates are in the last screenshot's image space unless relative=true (then 0..1).",
            json!({
                "x": x, "y": y, "relative": rel,
                "button": {"type":"string","enum":["left","right","middle"],"description":"Default left"},
                "double": {"type":"boolean","description":"Double-click (default false)"}
            }),
            &["x", "y"],
        ),
        tool(
            "move",
            "Move the mouse pointer without clicking (for hover effects). Coordinates as for click.",
            json!({"x": x, "y": y, "relative": rel}),
            &["x", "y"],
        ),
        tool(
            "scroll",
            "Scroll the mouse wheel at a position. Positive amount scrolls up, negative down. Coordinates as for click.",
            json!({
                "x": x, "y": y, "relative": rel,
                "amount": {"type":"integer","description":"Wheel notches; positive up, negative down"}
            }),
            &["x", "y", "amount"],
        ),
        tool(
            "type_text",
            "Type text on the till. Layout-independent (works whatever the till's keyboard layout). Set secret=true for passwords so the text is never logged or echoed.",
            json!({
                "text": {"type":"string"},
                "secret": {"type":"boolean","description":"Mark as a password (default false)"}
            }),
            &["text"],
        ),
        tool(
            "key",
            "Press a key or key combination. Names like Enter, Esc, Tab, F5, Ctrl+A, Ctrl+Alt+Del, Num0 to Num9, NumEnter.",
            json!({"combo": {"type":"string","description":"For example Enter, Ctrl+A, Ctrl+Alt+Del"}}),
            &["combo"],
        ),
        tool(
            "batch",
            "Run several steps in order in one call; stops at the first failing step. Each step is {type: click|move|scroll|type_text|key|wait, ...same arguments as that tool}. Batches cannot be nested.",
            json!({"steps": {
                "type":"array","minItems":1,
                "items": {"type":"object","properties": step_props,"required":["type"]}
            }}),
            &["steps"],
        ),
        tool(
            "wait",
            "Wait until the screen changes (change=true) and/or has been stable for stable_ms milliseconds. At least one of change or stable_ms is required; gives up after timeout_ms.",
            json!({
                "change": {"type":"boolean","description":"Return as soon as the screen changes"},
                "stable_ms": {"type":"integer","minimum":1,"description":"Return once the screen has not changed for this long"},
                "timeout_ms": {"type":"integer","minimum":0,"description":"Give up after this long"}
            }),
            &["timeout_ms"],
        ),
        tool(
            "status",
            "Report connection state, target, desktop size, mode and idle time.",
            json!({}),
            &[],
        ),
    ]
}

fn to_command(cmd: &str, mut obj: Map<String, Value>) -> Result<Command, String> {
    obj.insert("cmd".into(), Value::String(cmd.into()));
    serde_json::from_value(Value::Object(obj)).map_err(|e| {
        if cmd == "type_text" {
            // serde's message can quote the offending value, which may be a password.
            format!("invalid arguments for {cmd}")
        } else {
            format!("invalid arguments for {cmd}: {e}")
        }
    })
}

/// Map a tool name and its arguments to a call. Errors become tool errors.
pub fn parse(name: &str, args: &Value) -> Result<Call, String> {
    let obj = match args {
        Value::Object(m) => m.clone(),
        Value::Null => Map::new(),
        _ => return Err("arguments must be an object".into()),
    };
    let cmd = match name {
        "connect" => {
            let target = obj
                .get("target")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or("connect needs a non-empty target")?;
            let force = obj.get("force").and_then(Value::as_bool).unwrap_or(false);
            return Ok(Call::Connect {
                target: target.to_string(),
                force,
            });
        }
        "disconnect" => Command::Disconnect,
        "status" => Command::Status,
        "screenshot" | "click" | "move" | "scroll" | "type_text" | "key" | "wait" => {
            to_command(name, obj)?
        }
        "batch" => {
            let steps = obj
                .get("steps")
                .and_then(Value::as_array)
                .ok_or("batch needs a steps array")?;
            let mut out = Vec::with_capacity(steps.len());
            for (i, s) in steps.iter().enumerate() {
                let mut step = s
                    .as_object()
                    .ok_or_else(|| format!("step {i} must be an object"))?
                    .clone();
                let t = step
                    .remove("type")
                    .and_then(|t| t.as_str().map(str::to_string))
                    .ok_or_else(|| format!("step {i} needs a type"))?;
                if !STEP_TYPES.contains(&t.as_str()) {
                    return Err(format!("step {i}: unsupported type {t:?}"));
                }
                out.push(to_command(&t, step).map_err(|e| format!("step {i}: {e}"))?);
            }
            Command::Batch { steps: out }
        }
        other => return Err(format!("unknown tool {other:?}")),
    };
    cmd.validate()?;
    Ok(Call::Run(cmd))
}

pub fn error_result(message: String) -> Value {
    json!({"content":[{"type":"text","text":message}], "isError": true})
}

fn text_result(text: String) -> Value {
    json!({"content":[{"type":"text","text":text}], "isError": false})
}

/// Secret input carried by a command, so its result can be sanitized.
#[derive(Clone, Copy)]
pub enum Secret {
    /// A top-level secret type_text: characters typed, and skipped
    /// because they cannot be sent (counts only, never the characters).
    Text { typed: usize, skipped: usize },
    /// A batch of this many steps containing a secret type_text.
    Batch(usize),
}

/// Build the tool result. For secret input the success message is ours and
/// error text from the backend is dropped, never passed through.
pub fn result(reply: Reply, secret: Option<Secret>) -> Value {
    match reply {
        Reply::Ok { text } => match secret {
            Some(Secret::Text { typed, skipped }) => {
                let mut t = format!("typed {typed} characters (secret)");
                if skipped > 0 {
                    t.push_str(&format!("; skipped {skipped} that cannot be sent"));
                }
                text_result(t)
            }
            Some(Secret::Batch(n)) => {
                text_result(format!("batch done: {n} steps (contains secret input)"))
            }
            None => text_result(text),
        },
        Reply::Image {
            png_base64,
            desk_w,
            desk_h,
            scale,
        } => json!({
            "content": [
                {"type":"image","data":png_base64,"mimeType":"image/png"},
                {"type":"text","text":format!("desktop {desk_w}x{desk_h}, scale {scale}")}
            ],
            "isError": false
        }),
        Reply::Status {
            connected,
            target,
            desk_w,
            desk_h,
            mode,
            idle_s,
        } => text_result(if connected {
            format!(
                "connected to {}, desktop {desk_w}x{desk_h}, mode {mode}, idle {idle_s}s",
                target.as_deref().unwrap_or("?")
            )
        } else {
            format!("not connected, mode {mode}")
        }),
        Reply::Error {
            code,
            message,
            completed_steps,
        } => {
            let code = serde_json::to_value(code)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let mut t = if secret.is_some() {
                format!("failed ({code}); details withheld because the input was secret")
            } else {
                format!("{message} ({code})")
            };
            if let Some(n) = completed_steps {
                t.push_str(&format!("; {n} step(s) completed before the error"));
            }
            error_result(t)
        }
    }
}

/// Whether the command carries secret input.
pub fn secret_input(cmd: &Command) -> Option<Secret> {
    match cmd {
        Command::TypeText { text, secret: true } => {
            let skipped = sccm_rc_core::agent::text::encode_text(text).1.len();
            Some(Secret::Text {
                typed: text.chars().count() - skipped,
                skipped,
            })
        }
        Command::Batch { steps } if steps.iter().any(|s| secret_input(s).is_some()) => {
            Some(Secret::Batch(steps.len()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_text(v: &Value) -> &str {
        assert_eq!(v["isError"], false, "{v}");
        v["content"][0]["text"].as_str().unwrap()
    }

    #[test]
    fn secret_text_reports_skipped_count_without_the_text() {
        let cmd = Command::TypeText {
            text: "pa\tss😀wd".into(),
            secret: true,
        };
        let secret = secret_input(&cmd);
        let r = result(
            Reply::Ok {
                text: "typed 6 characters; skipped 2".into(),
            },
            secret,
        );
        let t = ok_text(&r);
        assert_eq!(
            t,
            "typed 6 characters (secret); skipped 2 that cannot be sent"
        );
        assert!(!t.contains("pa") && !t.contains('😀') && !t.contains('\t'));
    }

    #[test]
    fn secret_text_without_skips_says_so_plainly() {
        let cmd = Command::TypeText {
            text: "hunter2ä".into(),
            secret: true,
        };
        let r = result(
            Reply::Ok {
                text: "anything".into(),
            },
            secret_input(&cmd),
        );
        assert_eq!(ok_text(&r), "typed 8 characters (secret)");
    }
}
