# Script interface: Claude operates a till through SCCM Remote Control

Status: design, 2026-11-12. Not yet planned or built.

## Intent

We want Claude (Claude Code) to operate the user interface of a till (TCPOS
on Windows) through the existing SCCM Remote Control connection — the same
way a person does in the viewer: look at the screen, press buttons, type a
user and password, click through workflows. Today that only works with
screenshots of the whole Mac and simulated clicks (needs macOS Accessibility,
depends on window size and Retina scaling, slow and fragile). A direct
interface to the session is simpler and more reliable.

Use cases (from the user):
- recurring workflows on test tills (open the till, ring up a test receipt, …),
- **exploratory testing**: Claude tries things, reacts to unexpected screens,
  looks for bugs. So the interface offers small, freely combinable tools
  rather than predefined workflows.

SCCM Remote Control only carries image, keyboard and mouse. There is no
structured access to the till software: Claude still decides from the image
what is on screen. The gain is that the image and the input go straight
through the session, in the till's own pixel coordinates.

## Goals

1. **Background mode (headless):** a separate program connects to a till by
   itself, without a window, and offers the tools to Claude.
2. **Watch-along mode (live):** the user runs the normal viewer, allows
   control by Claude there, and sees every action in the window; they can
   step in or take control away at any time.
3. **One tool set for both modes**, so Claude does not need to know which
   mode is running.
4. Layout-independent text input, including umlauts (`ä ö ü é à`), which the
   till's Swiss keyboard layout and its forms need.

## Non-goals

- Access to the Windows system behind the till (commands, files, services):
  that is SCCM "Run Scripts" / PowerShell remoting, not remote control.
- OCR or any understanding of the screen inside the tool: Claude looks at the
  image itself.
- A macro or recording language; predefined workflows.
- Several sessions at once.
- An allow-list of targets: there are only test tills (user's decision).
- Windows support for the watch-along mode in the first version (macOS
  first; see Constraints).

## Anti-goals (would be a failure even if it technically works)

- A till left occupied ("existing session") because the tool crashed or was
  stopped: every exit path must release the session, like the viewer does.
- Claude acting with credentials or rights the user doesn't have: the tool
  only uses the user's own logon and SCCM permissions.
- In watch-along mode, Claude taking control without the user having
  switched it on in the viewer, or the user being unable to stop it.
- Passwords in tool output, logs or the MCP transcript.

## What we know about operating TCPOS remotely

From an exploration session on a test till (TCPOS 8.1.3, Denner layout),
operating it through the viewer with screenshots of the Mac:

- **Clicks are reliable, keys are not.** In the sale and payment screens,
  keyboard input does not arrive at all; it only works in dialogs with a
  Windows text field (logon, password, article search, price/amount).
  Escape and PageDown have no effect in preview and settlement screens.
  EAN codes and amounts are therefore entered with the on-screen digit
  buttons (13 clicks in a row worked).
- **Lists need a double click** ("Transaktion auswählen"); a single click
  only marks, a second one unmarks.
- **Timing:** dialogs appear after 1–2 s; after closing a receipt
  "Total … / Rückgeld …" shows for ~3 s, then "Willkommen!" returns.
  Unexpected flows exist (first logon after midnight forces a till closing
  and an operator settlement).
- **Feedback:** the two-line operator display (bottom left) shows the input
  buffer and messages; a crop of it is cheap feedback.
- **Layout:** fixed Denner keyboard layout; buttons keep their arrangement
  (user: they never move), pop-ups are full-screen layers with their own
  grid and a ← button top left. Only one monitor; the customer display is
  not part of the session.
- **What broke with Mac screenshots:** typed text never arrived, numpad keys
  could not be sent, the macOS focus jumped to other apps. A direct session
  interface avoids all three.

## The tools (same in both modes)

Coordinates are **in the space of the last screenshot Claude received** (the
tool maps them back to the till's real pixels, including scaling and crop),
or **relative** (`0.0–1.0` of width/height) for positions taken from the till
handbook. Claude never has to convert coordinates itself.

| Tool | What it does |
|---|---|
| `connect(target, force?)` | Background mode: log on (Keychain / env credentials) and start the session. If the till is already in use, report "in use" and do **not** connect; only `force: true` takes the till over (audited, and the result says so). Watch-along mode: report the viewer's current target instead. |
| `disconnect()` | Release the session (background) or hand control back to the user (watch-along). |
| `screenshot(region?, max_width?)` | Current desktop as PNG (MCP image content). Down-scaled by the tool to a safe size (default max 1280 px wide; Claude would otherwise shrink large images internally and click beside the target). Optional crop rectangle (e.g. the operator display) in full resolution. Returns the real desktop size and the scale. |
| `click(x, y, relative?, button?, double?)` | Move there and click (left default); `double: true` for lists. `relative: true` takes 0–1 values. |
| `batch(steps)` | Several clicks / keys / short waits in one call (e.g. an EAN on the digit buttons), with a small default delay between steps. Stops at the first error and reports how far it got. |
| `move(x, y)`, `scroll(x, y, amount)` | Mouse move / wheel. |
| `type_text(text)` | Type text, layout-independent (see below). Never logged in clear if marked as secret. |
| `key(combo)` | Single key or combination: `Enter`, `Esc`, `Tab`, `F5`, `Ctrl+A`, `Alt+F4`, `Ctrl+Alt+Del`, arrow keys, and numpad keys (`Num0`–`Num9`, `NumEnter`, …). |
| `wait(change?, stable_ms?, timeout_ms)` | Wait until the image changes, and/or until it has been stable for `stable_ms` — so Claude doesn't act on half-loaded screens. Returns what happened (changed / stable / timeout). |
| `status()` | Connected or not, target, desktop size, mode, how long idle. |

A `secret: true` flag on `type_text` keeps the text out of logs and tool
results (for passwords).

## Taking over a till that is in use (user's decision)

The core takes a till over only when `SCCM_RC_TAKEOVER=1` is set (checked
2026-11-12; an earlier review assumed it always did). Two different "busy"
cases exist:

- **HostInUse** (a user is logged on at the till): can be taken over.
- **ERROR_EXISTING_SESSION** (another remote-control session is active, e.g.
  the user's viewer, or an orphan left by a crash): the server refuses; it
  cannot be taken over, only waited out.

So:

- Take-over becomes an explicit option of the core instead of an env var;
  the viewer keeps its current behaviour, the headless backend passes
  `force`.
- `connect` without `force` returns a clear "till is in use" result.
- `connect(target, force: true)` takes the till over. It is written to the
  audit log and the tool result says a session was taken over.
- `ERROR_EXISTING_SESSION` gets its own clear message ("another
  remote-control session is active … try again later"); `force` does not
  help there.

## How it is built (decided)

- **Language/stack:** Rust, in this workspace, reusing `sccm-rc-core`
  (connect, sealing, RDP session loop, graceful disconnect). Same stack as the
  viewer; no new language.
- **MCP server:** a new binary `sccm-rc-mcp`, speaking MCP over stdio, so
  Claude Code starts it from its MCP configuration.
- **Two backends behind the same tools:**
  - *Headless backend:* the MCP server runs its own session with
    `SccmSession::connect_with` + `rdp::connect_rdp` + `rdp::run_active_session`
    (pattern: `crates/sccm-rc-core/src/bin/rc-bench.rs`), keeps the latest
    frame from a `SessionSink`, and sends input through the session's input
    channel.
  - *Live backend:* the MCP server connects to a running viewer over a local
    control socket and forwards the same commands; the viewer executes them on
    its own session and returns frames from its shared framebuffer.
- **Viewer side (watch-along):**
  - Menu item / toolbar toggle "Allow control by Claude" (off by default,
    off again after every restart).
  - Only while it is on, the viewer listens on a Unix domain socket in its
    data folder (`~/Library/Application Support/sccm-rc/control.sock`,
    folder already 0700, socket 0600). Off → socket closed.
  - While Claude controls: window subtitle shows it (e.g. "Controlled by
    Claude"), the toggle is highlighted; the user's own keyboard/mouse still
    work and win when both act; switching the toggle off cuts Claude off
    immediately.
- **Text input:** RDP fast-path Unicode keyboard events (layout-independent,
  covers umlauts). If the SCCM RC target turns out not to accept them, fall
  back to scancodes with a Swiss German layout table (today's
  `type_text::encode_string` is US-ASCII only). Shared code for viewer and
  MCP server lives in the core (or a small shared module), not duplicated.
- **Credentials (headless):** user from `SCCM_USER` or the viewer's last user;
  password from the macOS Keychain entry the viewer saves ("SCCM RC logon"),
  else `SCCM_PASS`. No interactive prompt; a missing password is a clear
  error. A rejected logon is not retried (lockout), same as the viewer.
- **Session hygiene:** one session at a time; graceful disconnect on
  `disconnect`, on MCP shutdown, on SIGINT/SIGTERM, and after an idle timeout
  (default 10 minutes without a tool call) so a forgotten session doesn't
  block a till.
- **Mode selection (my call):** fixed in the MCP configuration
  (`sccm-rc-mcp --mode headless` or `--mode live`). No automatic switching or
  fallback: in live mode with the viewer's toggle off (no socket), every tool
  returns "control not enabled in the viewer" and the server never connects
  to a till by itself.
- **The user's input wins (my call):** in live mode, local keyboard/mouse
  input during a Claude command aborts that command at once; the viewer
  releases any keys/buttons the command had pressed, and the tool returns
  "user took over". For 2 seconds after the user's last input, Claude's
  commands are rejected with the same message. Input from Claude is applied
  per command, so an abort never leaves half a password typed and continues.
- **Toggle off mid-command (my call):** same as above: the running command
  is aborted, pressed keys/buttons are released, queued Claude input is
  dropped, the socket closes.
- **Crash (my call):** one process, no watchdog. A hard crash (SIGKILL) can
  leave the till occupied until the server times the session out
  (`ERROR_EXISTING_SESSION` until then); SIGINT/SIGTERM, stdin EOF and the
  idle timeout all disconnect gracefully.
- **Audit:** connect/disconnect and each action (without secret text) go to
  the existing audit log (`audit.rs`), marked as done by the agent.

## Constraints

- macOS first (the user's platform). The headless MCP server should also
  build on Windows (core is cross-platform); the watch-along socket is
  Unix-only in v1 (Windows would need a named pipe later).
- Must not change the viewer's behaviour when the toggle is off; Windows
  viewer UI unchanged.
- Credentials and target names never in committed code or docs (test tills
  only reachable via the user's VPN / port-forwards).
- CI: unit tests must run without a till.

## Related viewer fix (separate change)

When the viewer window is maximized, the till image is stretched to the
window instead of keeping its aspect ratio (seen as text stretched wide).
The viewer should letterbox instead. Not part of the MCP work, but found
while exploring; tracked separately. The MCP tools are unaffected (they use
the till's own pixels, not the window).

## The till handbook (input for Claude, kept outside this repo)

Knowledge about TCPOS (screens, buttons, workflows) is collected by the
exploration session as Markdown and a planned Claude skill: per screen a
recognition anchor (fixed text such as "Willkommen!"), buttons by label with
a **relative** position, workflows as steps with the expected feedback.
Kept outside the public repo (no passwords, IPs or customer data). The MCP
tools are designed so positions from the handbook can be used directly
(`relative: true`).

## Left to the builder

- MCP library: the official Rust SDK (`rmcp`) if its license fits
  (MIT/Apache) and it is stable enough; otherwise a small hand-written
  JSON-RPC over stdio (MCP needs only `initialize`, `tools/list`,
  `tools/call`).
- Wire format on the control socket (e.g. line-delimited JSON mirroring the
  tool calls).
- Exact change detection for `wait` (frame counter vs. pixel diff).
- PNG encoder (the `png` crate is already a dependency of the core via
  `PngDumpSink`).
- Key-name parsing details and the list of named keys.
- Default delay between `batch` steps (start with ~100 ms; the exploration
  saw 13 consecutive clicks arrive).

## Verification

- Unit tests: key-combo parsing → scancode sequences; Unicode/Swiss text
  encoding; `wait` logic on synthetic frames; control-socket protocol
  round trip; credential lookup order.
- Live on the dev till (user present): headless — connect, screenshot, open
  the till login, type user + password (incl. one umlaut in a field), ring
  up a test article, disconnect; check the till is free afterwards.
  Watch-along — enable in the viewer, same flow, user steps in mid-way,
  disables the toggle, Claude is cut off.
- In-use test: with the user's viewer connected, headless `connect` reports
  "in use" and leaves the viewer's session alone; `force: true` takes over
  and is in the audit log.
- Crash test: kill the MCP server mid-session → `connect` reports
  "existing session" until the server releases it; then it connects.
