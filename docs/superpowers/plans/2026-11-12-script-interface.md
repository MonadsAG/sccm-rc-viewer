# Script interface (MCP) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Claude Code operate a TCPOS till's UI through SCCM Remote Control with an MCP server (`sccm-rc-mcp`), either with its own windowless session (headless) or by driving the user's running viewer (live).

**Architecture:** Shared, UI-free building blocks go into `sccm-rc-core` (`agent` module: key/text encoding, screenshot space, wait logic, socket protocol; plus `paths`, `keychain`, `audit` moved from the viewer). A new crate `sccm-rc-mcp` speaks MCP over stdio and talks to a `Backend` trait with two implementations: headless (own session, pattern of `rc-bench`) and live (Unix socket to the viewer). The viewer gets an opt-in control socket that executes the same commands on its session.

**Tech Stack:** Rust 2021 workspace; tokio; ironrdp-pdu 0.7 fast-path input; `image` 0.25 (PNG); serde/serde_json; base64; hand-written MCP JSON-RPC over stdio (no MCP library).

**Spec:** `docs/superpowers/specs/2026-11-12-script-interface-design.md`

## Global Constraints

- Tools (same in both modes): `connect(target, force?)`, `disconnect()`, `screenshot(region?, max_width?)`, `click(x, y, relative?, button?, double?)`, `move(x, y, relative?)`, `scroll(x, y, amount, relative?)`, `type_text(text, secret?)`, `key(combo)`, `batch(steps)`, `wait(change?, stable_ms?, timeout_ms)`, `status()`.
- Coordinates: in the space of the last screenshot (tool maps back, incl. crop and scale), or `relative: true` = 0.0–1.0 of the till desktop. Before any screenshot, absolute coordinates are till pixels.
- `screenshot` default `max_width` = 1280; never upscales; returns real desktop size and scale.
- Default delay between `batch` steps: 100 ms. Idle timeout: 10 min without a tool call → graceful disconnect.
- Mode fixed by `sccm-rc-mcp --mode headless|live`; no fallback between modes.
- Live: socket `paths::data_dir()/control.sock`, mode 0600, open only while the viewer toggle is on; toggle off after every viewer start. User input aborts the running Claude command and blocks Claude for 2 s.
- `type_text` with `secret: true` never appears in logs, audit or tool results.
- In use: without `force`, never take over. `HostInUse` + `force: true` → take-over (audited). `ERROR_EXISTING_SESSION` (another RC session, incl. an orphan after a crash) cannot be taken over → clear error.
- macOS first; `sccm-rc-mcp --mode headless` must also compile on Windows; the control socket and live mode are `cfg(unix)`.
- Viewer behaviour unchanged while the toggle is off; Windows viewer UI unchanged.
- No credentials, real IPs or till names in code, tests or docs (use `192.0.2.x`).

## Review Focus

- A screenshot cropped to a region, then a click: coordinates must map relative to the crop's origin, not the full desktop (test in Task 5).
- Desktop size changes mid-session (multi-monitor switch, reconnect): coordinates from an old screenshot must be rejected, not mapped onto the new size (test in Task 5).
- `type_text` with characters outside the BMP (emoji) or control chars: skipped and reported, never sent half (test in Task 4).
- Live mode: user presses a key in the middle of a `batch` → batch stops, reports the step it reached, all pressed keys/buttons released (test in Task 10).
- Headless `connect` while already connected (Claude calls it twice): returns "already connected to X", does not open a second session (test in Task 9).

---

### Task 1: Take-over becomes an explicit option in the core

**Files:**
- Modify: `crates/sccm-rc-core/src/rdp.rs` (ArbitrationChannel ~480-570, `connect_rdp` ~1006)
- Test: same file, `#[cfg(test)] mod arbitration_tests`

**Interfaces:**
- Produces: `pub struct RdpOptions { pub takeover: bool }` with `impl Default` (takeover = `SCCM_RC_TAKEOVER=1`, i.e. today's behaviour); `pub async fn connect_rdp_with(session, width, height, monitors, cliprdr_req_counter, opts: RdpOptions) -> Result<(ConnectionResult, Vec<u8>, u32)>`; `connect_rdp(...)` becomes a wrapper passing `RdpOptions::default()`. `ArbitrationChannel::new(takeover: bool)`.
- Produces: a host-in-use signal the caller can see: `pub static`-free design — `connect_rdp_with` returns `Err(anyhow!(HostInUse))` where `#[derive(Debug, thiserror::Error)] #[error("the till is in use by a logged-on user")] pub struct HostInUse;` is defined in `rdp.rs`, when the server reports event 1 and `takeover == false` and the shadow is not granted within the existing wait.

- [ ] **Step 1: Write failing tests** `takeover_disabled_ignores_host_in_use` (feeding a 12-byte payload with event 1 yields no `SvcMessage`) and `takeover_enabled_sends_type_5_once` (first event 1 → one message whose bytes 8..12 are `5u32` LE; second event 1 → none).
- [ ] **Step 2:** `cargo test -p sccm-rc-core arbitration_tests` → FAIL (`new` takes no argument).
- [ ] **Step 3:** Implement as in Interfaces; the env var is read only in `RdpOptions::default()`.
- [ ] **Step 4:** `cargo test -p sccm-rc-core` → PASS; `cargo build --workspace` → viewer and rc-bench unchanged.
- [ ] **Step 5:** Commit `feat(core): take-over as an explicit RdpOptions flag`.

### Task 2: Move `paths`, `keychain`, `audit` into the core

**Files:**
- Move: `crates/sccm-rc-viewer/src/{paths,keychain,audit}.rs` → `crates/sccm-rc-core/src/{paths,keychain,audit}.rs`
- Modify: `crates/sccm-rc-core/src/lib.rs` (`pub mod paths; pub mod keychain; pub mod audit;`), `crates/sccm-rc-core/Cargo.toml` (`[target.'cfg(target_os = "macos")'.dependencies] security-framework = "3.7"`), `crates/sccm-rc-viewer/src/main.rs` (`use sccm_rc_core::{paths, keychain, audit};` instead of `mod`), viewer `Cargo.toml` (drop its own security-framework).

**Interfaces:**
- Produces: `sccm_rc_core::paths::data_dir() -> PathBuf`, `sccm_rc_core::keychain::{AVAILABLE, load, save, delete, Lookup}` (unchanged signatures), `sccm_rc_core::audit::log_event(target, grant, event, duration_s)` plus new `audit::log_agent(target: &str, action: &str)` (writes `"event":"agent","action":...`; callers pass already-redacted text).

- [ ] **Step 1:** Move files and existing tests; add test `log_agent_writes_action` in `audit.rs` (writes to a temp data dir via `HOME` override, asserts the JSON line contains `"event":"agent"` and the action).
- [ ] **Step 2:** `cargo test --workspace` → PASS (existing keychain/paths tests now in core); `cargo test -p sccm-rc-core -- --ignored keychain` → round trip still passes.
- [ ] **Step 3:** Commit `refactor: paths, keychain and audit live in sccm-rc-core`.

### Task 3: Key combos → scancodes (`agent::keys`)

**Files:**
- Create: `crates/sccm-rc-core/src/agent/mod.rs`, `crates/sccm-rc-core/src/agent/keys.rs`
- Modify: `crates/sccm-rc-core/src/lib.rs` (`pub mod agent;`)

**Interfaces:**
- Produces: `pub fn parse_combo(combo: &str) -> Result<Vec<Key>, KeyError>`; `pub struct Key { pub scancode: u8, pub extended: bool }`; `pub fn press_combo(keys: &[Key]) -> Vec<FastPathInputEvent>` (modifiers down in order, last key down/up, modifiers up in reverse). Names (case-insensitive, `+`-separated): `Ctrl Shift Alt Win Enter Esc Tab Space Backspace Delete Insert Home End PageUp PageDown Up Down Left Right F1–F12 A–Z 0–9 Num0–Num9 NumEnter NumPlus NumMinus NumMultiply NumDivide NumDecimal`. `Ctrl+Alt+Del` maps to the same sequence the viewer's `send_ctrl_alt_del` sends.

- [ ] **Step 1:** Tests: `enter_is_0x1c`, `num5_is_0x4c_not_extended`, `num_enter_is_extended_0x1c`, `ctrl_a_presses_and_releases_in_order` (exact event list: Ctrl down 0x1D, A down 0x1E, A up, Ctrl up), `unknown_key_errors` (`"Ctrl+Foo"` → `KeyError::Unknown("Foo")`), `case_insensitive` (`"ctrl+alt+del" == "Ctrl+Alt+Del"`).
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(core): key combo parser for the agent`.

### Task 4: Layout-independent text (`agent::text`)

**Files:**
- Create: `crates/sccm-rc-core/src/agent/text.rs`

**Interfaces:**
- Produces: `pub fn encode_text(text: &str) -> (Vec<FastPathInputEvent>, Vec<char>)` — for every char in the BMP that is not a control char: `UnicodeKeyboardEvent(KeyboardFlags::empty(), code)` then `UnicodeKeyboardEvent(KeyboardFlags::RELEASE, code)`; `\n` → Enter scancode press/release; the second vec lists skipped chars.

- [ ] **Step 1:** Tests: `umlaut_is_unicode_0xe4` (`"ä"` → down/up with code `0xE4`), `newline_is_enter_scancode`, `emoji_and_controls_are_skipped_and_reported` (`"a😀\u{7}b"` → events for a and b only, skipped = `['😀', '\u{7}']`).
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(core): Unicode text encoding for the agent`.
- Note for Task 9's live test: if the till ignores Unicode events, add `encode_text_swiss(text)` (scancodes, Swiss German layout) in this file and select it with `SCCM_RC_AGENT_TEXT=scancodes`; do not build it before the live test shows it is needed.

### Task 5: Screenshot space and coordinate mapping (`agent::shot`)

**Files:**
- Create: `crates/sccm-rc-core/src/agent/shot.rs`

**Interfaces:**
- Produces: `pub struct ShotSpace { pub desk_w: u16, pub desk_h: u16, pub crop: Rect, pub img_w: u32, pub img_h: u32 }`, `pub struct Rect { pub x: u16, pub y: u16, pub w: u16, pub h: u16 }`; `pub fn take(rgba: &[u8], desk_w: u16, desk_h: u16, region: Option<Rect>, max_width: u32) -> Result<(Vec<u8> /*png*/, ShotSpace), ShotError>`; `impl ShotSpace { pub fn to_desktop_checked(&self, x: f64, y: f64, cur_w: u16, cur_h: u16) -> Result<(u16, u16), ShotError>; pub fn scale(&self) -> f64 }`; `pub fn relative_to_desktop(rx: f64, ry: f64, desk_w: u16, desk_h: u16) -> Result<(u16, u16), ShotError>`; `ShotError::{OutOfBounds, StaleShot, BadRegion}`.

- [ ] **Step 1:** Tests:
  - `downscales_to_max_width` (1920×1080 desktop, max 1280 → img 1280×720, scale 1.5),
  - `never_upscales` (1024×768, max 1280 → 1024×768),
  - `click_maps_back_through_scale` (img point 640,360 → desktop 960,540; via `to_desktop_checked(640.0, 360.0, 1920, 1080)`),
  - `click_maps_back_through_crop` (crop x=100,y=50,w=200,h=100, no scaling; img 10,10 → desktop 110,60),
  - `stale_shot_rejected` (`to_desktop_checked` with a current size different from the shot's desk size → `StaleShot`),
  - `relative_corners` (0,0 → 0,0; 1,1 → w-1,h-1; 1.2 → `OutOfBounds`),
  - `bad_region_rejected` (region outside desktop → `BadRegion`).
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement (crop then `image::imageops::resize` with `Triangle`, PNG via `image`). **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(core): screenshot space with coordinate mapping`.

### Task 6: Waiting for the screen (`agent::wait`)

**Files:**
- Create: `crates/sccm-rc-core/src/agent/wait.rs`

**Interfaces:**
- Produces: `pub struct FrameWatch` fed by the session sink: `pub fn on_update(&mut self, now: Instant)` (called from `on_graphics_update`); `pub fn changed_since(&self, mark: u64) -> bool`; `pub fn mark(&self) -> u64` (update counter); `pub fn quiet_for(&self, now: Instant) -> Duration`; `pub enum WaitResult { Changed, Stable, Timeout }`; `pub fn evaluate(&self, mark: u64, want_change: bool, stable_ms: Option<u64>, started: Instant, timeout: Duration, now: Instant) -> Option<WaitResult>` (None = keep waiting). Change = at least one graphics update after `mark`; stable = no update for `stable_ms`. With both: first change, then stable.

- [ ] **Step 1:** Tests with injected `Instant`s: `change_detected`, `stable_after_quiet_period`, `change_then_stable`, `timeout_when_nothing_happens`, `stable_only_without_change_requirement`.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5:** Commit `feat(core): frame watch for agent waits`.

### Task 7: Command protocol (`agent::proto`)

**Files:**
- Create: `crates/sccm-rc-core/src/agent/proto.rs`
- Modify: `crates/sccm-rc-core/Cargo.toml` (`serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`, `base64 = "0.22"`)

**Interfaces:**
- Produces: `#[derive(Serialize, Deserialize)] #[serde(tag = "cmd", rename_all = "snake_case")] pub enum Command { Status, Screenshot { region: Option<Rect>, max_width: Option<u32> }, Click { x: f64, y: f64, relative: bool, button: Button, double: bool }, Move {..}, Scroll { x, y, amount: i16, relative }, TypeText { text: String, secret: bool }, Key { combo: String }, Batch { steps: Vec<Command> }, Wait { change: bool, stable_ms: Option<u64>, timeout_ms: u64 }, Disconnect }` (Connect is MCP-only: headless backend handles it, live returns the viewer's target); `pub enum Reply { Ok { text: String }, Image { png_base64: String, desk_w: u16, desk_h: u16, scale: f64 }, Status { connected: bool, target: Option<String>, desk_w: u16, desk_h: u16, mode: String, idle_s: u64 }, Error { code: ErrorCode, message: String, completed_steps: Option<usize> } }`; `ErrorCode::{NotConnected, InUse, ExistingSession, UserTookOver, ControlDisabled, BadArgs, Session}`; `impl Debug for Command` redacts `TypeText` text when `secret`. Wire: one JSON object per line.

- [ ] **Step 1:** Tests: `round_trip_every_command` (serialize → deserialize equals), `secret_text_redacted_in_debug` (`format!("{:?}")` of a secret TypeText doesn't contain the text), `batch_nested_batch_rejected` (`Command::validate()` → Err for Batch inside Batch).
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5:** Commit `feat(core): agent command protocol`.

### Task 8: MCP server crate with a Backend trait

**Files:**
- Create: `crates/sccm-rc-mcp/Cargo.toml`, `crates/sccm-rc-mcp/src/main.rs` (CLI `--mode headless|live`, stdio loop), `src/mcp.rs` (JSON-RPC: `initialize` → protocolVersion `2025-06-18`, capabilities `{tools:{}}`; `tools/list`; `tools/call`; `notifications/initialized` ignored), `src/tools.rs` (tool JSON schemas + argument → `Command`/connect mapping), `src/backend.rs`
- Modify: root `Cargo.toml` (`members += "crates/sccm-rc-mcp"`)

**Interfaces:**
- Consumes: `agent::proto::{Command, Reply}`.
- Produces: `#[async_trait] pub trait Backend { async fn connect(&mut self, target: &str, force: bool) -> Reply; async fn run(&mut self, cmd: Command) -> Reply; async fn shutdown(&mut self); }` (via the `async-trait` crate, so the backend can be stored as `Box<dyn Backend>`); tool results: `Reply::Image` → MCP content `[{type:"image", data, mimeType:"image/png"}, {type:"text", text:"desktop WxH, scale S"}]`; `Reply::Error` → `isError: true` with the message; logs to stderr only (stdout is the protocol).

- [ ] **Step 1:** Tests with a `FakeBackend` recording commands: `initialize_reports_tools_capability`, `tools_list_has_eleven_tools` (names exactly as Global Constraints), `click_maps_arguments` (`{"x":10,"y":20,"double":true}` → `Command::Click{relative:false, button:Left, double:true}`), `secret_text_not_echoed` (tool result for secret `type_text` contains no text), `unknown_tool_is_error`.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5:** Commit `feat(mcp): MCP stdio server skeleton`.

### Task 9: Headless backend

**Files:**
- Create: `crates/sccm-rc-mcp/src/headless.rs`, `src/creds.rs`

**Interfaces:**
- Consumes: `SccmSession::connect_with`, `rdp::connect_rdp_with(.., RdpOptions { takeover: force })`, `rdp::run_active_session`, `rdp::HostInUse`, `Error::{ExistingSession, LogonRejected}`, `agent::{keys, text, shot, wait}`, `keychain::load`, `paths::data_dir`, `audit::log_agent`.
- Produces: `pub struct Headless` implementing `Backend`. Session runs in a spawned task with a sink that copies the latest RGBA frame into `Arc<Mutex<Frame { rgba, w, h }>>` and feeds `FrameWatch`; input via the `InputSender`. `creds::resolve(env: impl Fn(&str)->Option<String>, last_user: Option<String>, keychain: Lookup) -> Result<LogonCredentials, CredError>`: user = `SCCM_USER` else last user (`data_dir()/last_user.txt`); password = Keychain(user) else `SCCM_PASS`; missing → `CredError::NoPassword`.
- Behaviour: second `connect` while connected → `Reply::Error{BadArgs, "already connected to X"}`; `ExistingSession` → `ErrorCode::ExistingSession` ("another remote-control session is active, e.g. your viewer or a session that was not released; try again later"); `HostInUse` without force → `ErrorCode::InUse` ("use force to take over"); `LogonRejected` → no retry. Idle 10 min → `shutdown`. SIGINT/SIGTERM (`tokio::signal`) and stdin EOF → `shutdown` (graceful disconnect, wait up to 3 s). Every action → `audit::log_agent` (secret text as `"type_text (secret, N chars)"`).

- [ ] **Step 1:** Unit tests for `creds::resolve`: `env_user_and_keychain_password`, `last_user_when_no_env`, `env_password_fallback`, `no_password_errors`. Test `second_connect_rejected` with the session task replaced by a stub (constructor `Headless::with_connector(fn)`).
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5 (live, user present, dev till):** in Claude Code with `sccm-rc-mcp --mode headless` registered: connect, screenshot, open the till login, `type_text` user + password (`secret: true`) incl. one umlaut in a text field, ring up a test article via the digit buttons with `batch`, disconnect; then the viewer connects without "existing session". Record whether Unicode text arrived (see Task 4 note).
- [ ] **Step 6:** Commit `feat(mcp): headless backend`.

### Task 10: Viewer control socket (live mode, viewer side)

**Files:**
- Create: `crates/sccm-rc-viewer/src/control.rs` (`#[cfg(unix)]`; stub elsewhere)
- Modify: `crates/sccm-rc-viewer/src/main.rs` (App field `control: Option<control::Server>`, `UserEvent::Control(Command, reply: std::sync::mpsc::Sender<Reply>)`, execution on the App thread, user-input hooks), `crates/sccm-rc-viewer/src/mac_ui/mod.rs` (menu item + toolbar toggle "Allow control by Claude", subtitle "Controlled by Claude" while a command ran in the last 5 s), locales (`control.allow`, `control.active`, `control.user_took_over`)

**Interfaces:**
- Consumes: `agent::proto`, `agent::{keys, text, shot, wait}`, `paths::data_dir`.
- Produces: `control::Server::start(data_dir: &Path, proxy: EventLoopProxy<UserEvent>) -> io::Result<Server>` (binds `control.sock`, removes a stale file first, `chmod 0600`, accept loop on a thread; each line → `UserEvent::Control`); `Server::stop()` (closes, deletes the socket); `pub struct Arbiter` (pure state): `on_user_input(now)`, `begin_command()`, `should_abort(now) -> bool`, `accepts_commands(now) -> bool` (false for 2 s after user input). `NativeEvent::Action(ToolbarAction::ToggleAgentControl)` toggles start/stop.
- Behaviour: commands run step by step on the App thread (batch steps and typed chars scheduled via `ControlFlow::WaitUntil`, 100 ms apart); every keyboard/mouse event from the user calls `Arbiter::on_user_input` → running command aborted: release all keys/buttons it pressed, reply `UserTookOver` with `completed_steps`. Toggle off → same abort, socket closed. Toggle defaults off on every start.

- [ ] **Step 1:** Unit tests for `Arbiter`: `user_input_aborts_running_command`, `commands_rejected_for_two_seconds`, `accepts_again_after_cooldown`; `control_socket_round_trip` (start on a temp dir, connect a `UnixStream`, send `{"cmd":"status"}`, receive one JSON line; the App side faked by a thread answering from the proxy channel); `socket_is_0600`; `stop_removes_socket`.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5:** Commit `feat(viewer): opt-in control socket for Claude`.

### Task 11: Live backend in the MCP server

**Files:**
- Create: `crates/sccm-rc-mcp/src/live.rs` (`#[cfg(unix)]`)

**Interfaces:**
- Consumes: `Backend`, `agent::proto`, `paths::data_dir`.
- Produces: `pub struct Live` implementing `Backend`: each `run` opens/reuses a `UnixStream` to `control.sock`, writes one line, reads one line. No socket / connection refused → `Reply::Error{ControlDisabled, "enable 'Allow control by Claude' in the viewer"}`. `connect(target, _)` → `Status` of the viewer (never connects to a till). `shutdown` → closes the stream only.

- [ ] **Step 1:** Tests against a fake socket server in a temp dir: `relays_command_and_reply`, `missing_socket_reports_control_disabled`, `connect_never_dials_a_till` (returns the fake viewer's target).
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5 (live, user present):** viewer connected to the dev till, toggle on, same flow as Task 9 Step 5 through `--mode live`; mid-`batch` the user clicks → Claude gets `UserTookOver` with `completed_steps`; toggle off → next tool call `ControlDisabled`.
- [ ] **Step 6:** Commit `feat(mcp): live backend over the viewer socket`.

### Task 12: Docs and packaging

**Files:**
- Modify: `README.md` (section "Letting Claude operate a till": build, `claude mcp add sccm-rc -- <path>/sccm-rc-mcp --mode headless`, live mode toggle, tools table, safety notes), `packaging/macos/bundle.sh` (copy `sccm-rc-mcp` into `SCCM RC.app/Contents/MacOS/`), `.github/workflows/ci.yml` (nothing new: `cargo test --workspace` covers the crate)

- [ ] **Step 1:** `packaging/macos/bundle.sh` → app contains both binaries; `codesign --verify` passes.
- [ ] **Step 2:** Commit `docs: MCP script interface`.

### Task 13 (separate fix): Viewer keeps the till's aspect ratio

**Files:**
- Modify: `crates/sccm-rc-viewer/src/main.rs` (GPU desktop quad rect and CPU scaling path, `map_cursor`)

**Interfaces:**
- Produces: `fn fit_rect(desk_w: u32, desk_h: u32, area_w: u32, area_h: u32) -> (x, y, w, h)` (largest centered rect with the desktop's aspect ratio); rendering fills the rest with the background colour; `map_cursor` maps through the same rect and ignores clicks in the bars.

- [ ] **Step 1:** Tests: `fit_rect_letterboxes_wide_window` (1024×768 into 1467×650 → h 650, w 867, x centred), `fit_rect_exact_fit`, `cursor_in_bar_is_outside`.
- [ ] **Step 2–4:** FAIL → implement → PASS; manual: maximize the viewer, text no longer stretched.
- [ ] **Step 5:** Commit `fix(viewer): keep the remote desktop's aspect ratio`.
