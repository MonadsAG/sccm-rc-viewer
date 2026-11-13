//! Minimal MCP server: newline-delimited JSON-RPC 2.0 over stdio.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::backend::Backend;
use crate::tools::{self, Call};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct Server {
    backend: Box<dyn Backend + Send>,
}

impl Server {
    pub fn new(backend: Box<dyn Backend + Send>) -> Self {
        Self { backend }
    }

    pub async fn serve<R, W>(&mut self, mut input: R, mut output: W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let r = self.run_loop(&mut input, &mut output).await;
        self.backend.shutdown().await;
        r
    }

    /// Shut the backend down (e.g. on a signal, when `serve` was cancelled).
    /// Safe to call more than once.
    pub async fn shutdown(&mut self) {
        self.backend.shutdown().await;
    }

    async fn run_loop<R, W>(&mut self, input: &mut R, output: &mut W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if input.read_until(b'\n', &mut buf).await? == 0 {
                return Ok(());
            }
            let resp = match std::str::from_utf8(&buf) {
                Ok(line) if line.trim().is_empty() => continue,
                Ok(line) => self.handle_line(line).await,
                Err(_) => Some(rpc_error(Value::Null, -32700, "parse error: invalid UTF-8")),
            };
            if let Some(resp) = resp {
                let mut s = resp.to_string();
                s.push('\n');
                output.write_all(s.as_bytes()).await?;
                output.flush().await?;
            }
        }
    }

    /// Handle one JSON-RPC message; `None` for notifications.
    pub async fn handle_line(&mut self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if !msg.is_object() {
            return Some(rpc_error(Value::Null, -32600, "invalid request"));
        }
        // Notifications (no id) and stray responses (no method) get no reply.
        let id = msg.get("id").cloned()?;
        let method = msg.get("method").and_then(Value::as_str)?;
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => rpc_ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "sccm-rc-mcp", "version": env!("CARGO_PKG_VERSION")}
                }),
            ),
            "ping" => rpc_ok(id, json!({})),
            "tools/list" => rpc_ok(id, json!({"tools": tools::list()})),
            "tools/call" => {
                let result = self.call_tool(&params).await;
                rpc_ok(id, result)
            }
            other => rpc_error(id, -32601, &format!("method not found: {other}")),
        })
    }

    async fn call_tool(&mut self, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return tools::error_result("tools/call needs a tool name".into());
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let call = match tools::parse(name, &args) {
            Ok(c) => c,
            Err(e) => return tools::error_result(e),
        };
        match call {
            Call::Connect { target, force } => {
                tracing::info!(target, force, "tool connect");
                let reply = self.backend.connect(&target, force).await;
                tools::result(reply, None)
            }
            Call::Run(cmd) => {
                // Debug only: it redacts secret text; never serialize a Command here.
                tracing::info!("tool {name}: {cmd:?}");
                let secret = tools::secret_input(&cmd);
                let reply = self.backend.run(cmd).await;
                tools::result(reply, secret)
            }
        }
    }
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sccm_rc_core::agent::proto::{Button, Command, Reply};
    use std::sync::{Arc, Mutex};

    struct FakeBackend {
        cmds: Arc<Mutex<Vec<Command>>>,
        reply: Reply,
    }

    #[async_trait]
    impl Backend for FakeBackend {
        async fn connect(&mut self, target: &str, force: bool) -> Reply {
            Reply::Ok {
                text: format!("connected {target} force={force}"),
            }
        }
        async fn run(&mut self, cmd: Command) -> Reply {
            self.cmds.lock().unwrap().push(cmd);
            self.reply.clone()
        }
        async fn shutdown(&mut self) {}
    }

    fn server(reply: Reply) -> (Server, Arc<Mutex<Vec<Command>>>) {
        let cmds = Arc::new(Mutex::new(Vec::new()));
        let s = Server::new(Box::new(FakeBackend {
            cmds: cmds.clone(),
            reply,
        }));
        (s, cmds)
    }

    fn ok() -> Reply {
        Reply::Ok {
            text: "done".into(),
        }
    }

    async fn rpc(s: &mut Server, v: Value) -> Value {
        s.handle_line(&v.to_string()).await.expect("response")
    }

    fn call(name: &str, args: Value) -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})
    }

    #[tokio::test]
    async fn initialize_reports_tools_capability() {
        let (mut s, _) = server(ok());
        let r = rpc(
            &mut s,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await;
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["capabilities"], json!({"tools":{}}));
        assert_eq!(r["result"]["serverInfo"]["name"], "sccm-rc-mcp");
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn tools_list_has_eleven_tools() {
        let (mut s, _) = server(ok());
        let r = rpc(
            &mut s,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        let mut names: Vec<_> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                assert!(t["inputSchema"]["type"] == "object");
                assert!(!t["description"].as_str().unwrap().is_empty());
                t["name"].as_str().unwrap().to_string()
            })
            .collect();
        names.sort();
        let mut want = vec![
            "connect",
            "disconnect",
            "screenshot",
            "click",
            "move",
            "scroll",
            "type_text",
            "key",
            "batch",
            "wait",
            "status",
        ];
        want.sort();
        assert_eq!(names, want);
    }

    #[tokio::test]
    async fn click_maps_arguments() {
        let (mut s, cmds) = server(ok());
        let r = rpc(&mut s, call("click", json!({"x":10,"y":20,"double":true}))).await;
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(
            cmds.lock().unwrap().as_slice(),
            [Command::Click {
                x: 10.0,
                y: 20.0,
                relative: false,
                button: Button::Left,
                double: true
            }]
        );
    }

    #[tokio::test]
    async fn secret_text_not_echoed() {
        let (mut s, cmds) = server(Reply::Ok {
            text: "typed hunter22".into(),
        });
        let r = rpc(
            &mut s,
            call("type_text", json!({"text":"hunter22","secret":true})),
        )
        .await;
        let out = r.to_string();
        assert!(!out.contains("hunter22"), "{out}");
        assert!(out.contains("8 characters"), "{out}");
        assert_eq!(cmds.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_tool_is_error() {
        let (mut s, cmds) = server(ok());
        let r = rpc(&mut s, call("format_disk", json!({}))).await;
        assert_eq!(r["result"]["isError"], true);
        assert!(cmds.lock().unwrap().is_empty());
        let r = rpc(&mut s, json!({"jsonrpc":"2.0","id":3,"method":"nope"})).await;
        assert_eq!(r["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn invalid_commands_are_tool_errors() {
        let (mut s, cmds) = server(ok());
        for (n, a) in [
            ("wait", json!({"timeout_ms":100})),
            ("batch", json!({"steps":[]})),
            ("batch", json!({"steps":[{"type":"batch","steps":[]}]})),
            ("click", json!({"x":"a","y":1})),
        ] {
            let r = rpc(&mut s, call(n, a.clone())).await;
            assert_eq!(r["result"]["isError"], true, "{n} {a}");
        }
        assert!(cmds.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn batch_maps_steps() {
        let (mut s, cmds) = server(ok());
        let r = rpc(
            &mut s,
            call(
                "batch",
                json!({"steps":[{"type":"key","combo":"Enter"},{"type":"wait","stable_ms":200,"timeout_ms":1000}]}),
            ),
        )
        .await;
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(
            cmds.lock().unwrap().as_slice(),
            [Command::Batch {
                steps: vec![
                    Command::Key {
                        combo: "Enter".into()
                    },
                    Command::Wait {
                        change: false,
                        stable_ms: Some(200),
                        timeout_ms: 1000
                    }
                ]
            }]
        );
    }

    #[tokio::test]
    async fn image_and_error_results() {
        let (mut s, _) = server(Reply::Image {
            png_base64: "QUJD".into(),
            desk_w: 1920,
            desk_h: 1080,
            scale: 0.5,
        });
        let r = rpc(&mut s, call("screenshot", json!({}))).await;
        let c = &r["result"]["content"];
        assert_eq!(c[0]["type"], "image");
        assert_eq!(c[0]["data"], "QUJD");
        assert_eq!(c[0]["mimeType"], "image/png");
        assert_eq!(c[1]["text"], "desktop 1920x1080, scale 0.5");

        let (mut s, _) = server(Reply::Error {
            code: sccm_rc_core::agent::proto::ErrorCode::NotConnected,
            message: "no till".into(),
            completed_steps: Some(2),
        });
        let r = rpc(&mut s, call("status", json!({}))).await;
        assert_eq!(r["result"]["isError"], true);
        let t = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(t.contains("no till") && t.contains('2'), "{t}");
    }

    struct Echo;

    #[async_trait]
    impl Backend for Echo {
        async fn connect(&mut self, _: &str, _: bool) -> Reply {
            ok()
        }
        async fn run(&mut self, cmd: Command) -> Reply {
            let text = format!("{}", serde_json::to_string(&cmd).unwrap());
            if matches!(cmd, Command::Batch { ref steps } if steps.len() == 3) {
                Reply::Error {
                    code: sccm_rc_core::agent::proto::ErrorCode::Session,
                    message: text,
                    completed_steps: Some(1),
                }
            } else {
                Reply::Ok { text }
            }
        }
        async fn shutdown(&mut self) {}
    }

    #[tokio::test]
    async fn batch_secret_not_echoed() {
        let mut s = Server::new(Box::new(Echo));
        for steps in [
            json!([{"type":"type_text","text":"hunter22","secret":true},{"type":"key","combo":"Enter"}]),
            json!([{"type":"key","combo":"Tab"},{"type":"type_text","text":"hunter22","secret":true},{"type":"key","combo":"Enter"}]),
        ] {
            let r = rpc(&mut s, call("batch", json!({"steps": steps}))).await;
            let out = r.to_string();
            assert!(!out.contains("hunter22"), "{out}");
        }
        // non-secret batch is passed through
        let r = rpc(
            &mut s,
            call("batch", json!({"steps":[{"type":"key","combo":"Tab"}]})),
        )
        .await;
        assert!(r.to_string().contains("Tab"));
    }

    #[tokio::test]
    async fn secret_type_text_parse_error_is_generic() {
        let (mut s, _) = server(ok());
        let r = rpc(
            &mut s,
            call("type_text", json!({"text":"hunter22","secret":"yes"})),
        )
        .await;
        assert_eq!(r["result"]["isError"], true);
        assert!(!r.to_string().contains("hunter22"));
        let r = rpc(
            &mut s,
            call(
                "batch",
                json!({"steps":[{"type":"type_text","text":"hunter22","secret":"yes"}]}),
            ),
        )
        .await;
        assert_eq!(r["result"]["isError"], true);
        assert!(!r.to_string().contains("hunter22"));
    }

    #[tokio::test]
    async fn invalid_utf8_line_is_parse_error_and_loop_continues() {
        let (mut s, _) = server(ok());
        let mut input: Vec<u8> = b"\xff\xfe\n".to_vec();
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n");
        let mut out = Vec::new();
        s.serve(tokio::io::BufReader::new(&input[..]), &mut out)
            .await
            .unwrap();
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["error"]["code"], -32700);
        assert_eq!(lines[0]["id"], Value::Null);
        assert_eq!(lines[1]["id"], 7);
    }

    #[tokio::test]
    async fn non_object_is_invalid_request_and_responses_ignored() {
        let (mut s, _) = server(ok());
        for bad in ["[1,2]", "42"] {
            let r = s.handle_line(bad).await.unwrap();
            assert_eq!(r["error"]["code"], -32600);
            assert_eq!(r["id"], Value::Null);
        }
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","id":5,"result":{}}"#)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn connect_passes_force() {
        let (mut s, _) = server(ok());
        let r = rpc(
            &mut s,
            call("connect", json!({"target":"till1","force":true})),
        )
        .await;
        assert_eq!(
            r["result"]["content"][0]["text"],
            "connected till1 force=true"
        );
    }
}
