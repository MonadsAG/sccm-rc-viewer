//! SCCM Remote Control viewer — winit window rendering the remote desktop
//! over the pure-Rust SCCM transport, with mouse + keyboard forwarding.

// Drawing/render/protocol helpers take many positional params (x, y, w, h,
// color, scale, …); bundling them into structs would hurt readability here.
#![allow(clippy::too_many_arguments)]

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::Parser;
use sccm_rc_core::rdp::{
    self, FastPathInputEvent, FrameView, InputSender, KeyboardFlags, MousePdu, PointerFlags,
    PointerUpdate, SessionSink, SessionStats, UpdateRegion,
};
use sccm_rc_core::{LogonCredentials, SccmSession};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{Window, WindowId};

use sccm_rc_core::agent::exec;
use sccm_rc_core::agent::proto::{Command, ErrorCode, Reply};
use sccm_rc_core::agent::shot::{self, ShotSpace};
use sccm_rc_core::{audit, keychain, paths};

mod control;
mod gpu;
mod host_prompt;
mod keymap;
mod login_prompt;
#[cfg(target_os = "macos")]
mod mac_ui;
#[cfg(not(target_os = "macos"))]
#[path = "mac_ui_stub.rs"]
mod mac_ui;
mod recent;
mod record;
mod report;
mod servers;
mod text;
mod toolbar;
mod type_text;
#[cfg(windows)]
mod winhook;
mod wol;
use toolbar::ToolbarAction;

// Localization: embeds locales/*.yml at compile time. `t!()` resolves against the
// locale chosen by `init_locale()` at startup (the Windows UI language, or --lang).
rust_i18n::i18n!("locales", fallback = "en");
use rust_i18n::t;

/// Pick and activate the UI locale: `--lang`/`SCCM_RC_LANG` override, else the
/// Windows UI language, else English. Only en/nl/de are supported.
fn init_locale(cli_lang: Option<&str>) {
    let raw = cli_lang
        .map(str::to_string)
        .or_else(|| std::env::var("SCCM_RC_LANG").ok())
        .or_else(sys_locale::get_locale);
    let lang = match raw
        .as_deref()
        .unwrap_or("en")
        .get(0..2)
        .unwrap_or("en")
        .to_ascii_lowercase()
        .as_str()
    {
        "nl" => "nl",
        "de" => "de",
        _ => "en",
    };
    rust_i18n::set_locale(lang);
    info!(locale = lang, source = ?raw, "UI language");
}

/// Full version string for `--version`: package version + embedded git hash
/// (set by build.rs). The window title shows just the package version.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")");

#[derive(Parser)]
#[command(name = "sccm-rc-viewer", about = "SCCM Remote Control viewer", version = VERSION)]
struct Cli {
    /// Target hostname or IP (like CmRcViewer). If omitted, a prompt is shown.
    target: Option<String>,
    /// Requested desktop width
    #[arg(long, default_value_t = 1280)]
    width: u16,
    /// Requested desktop height
    #[arg(long, default_value_t = 720)]
    height: u16,
    /// MAC address for Wake-on-LAN (e.g. AA-BB-CC-DD-EE-FF). Seeds the WoL cache
    /// for this target and wakes it before connecting.
    #[arg(long)]
    mac: Option<String>,
    /// Send a Wake-on-LAN magic packet before connecting (uses the cached or
    /// --mac address for the target).
    #[arg(long)]
    wake: bool,
    /// Advertise a multi-monitor layout (repeatable). Geometry per monitor:
    /// WIDTHxHEIGHT+LEFT+TOP, e.g. `--monitor 1920x1080+0+0 --monitor 1280x1024+1920+0`.
    /// The first `--monitor` is the primary; omit for a single monitor.
    #[arg(long = "monitor")]
    monitors: Vec<String>,
    /// UI language: en, nl or de. Default: follow the Windows UI language.
    #[arg(long)]
    lang: Option<String>,
    /// Capture ALL of a multi-monitor target's screens as one combined desktop
    /// (SCCM "All Screens"), instead of only the primary. Once connected you can
    /// switch between individual screens from the toolbar (or Ctrl+Tab). Note:
    /// this persistently sets UseAllMonitors=1 in the target's registry, so it
    /// also affects later remote-control sessions to that machine.
    #[arg(long = "all-screens")]
    all_screens: bool,
    /// Demo/screenshot mode: don't connect to anything — paint a synthetic
    /// multi-monitor desktop so the full UI (toolbar + monitor switcher) renders
    /// for a privacy-safe screenshot. No real machine is contacted.
    #[arg(long)]
    demo: bool,
}

/// Load the embedded application icon for the live window (taskbar / title bar).
/// The .exe also carries this icon as a Win32 resource (build.rs) so Explorer and
/// the Properties tab show it, but winit needs the window icon set explicitly.
/// Set the Dock icon on macOS. A bare binary has no `.app` bundle to take it
/// from, and winit ignores the window icon there, so hand the embedded PNG to
/// `NSApplication` directly. Must run on the main thread after the app exists.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(include_bytes!("../assets/icon.png"));
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        warn!("could not decode the embedded icon for the Dock");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: called on the main thread (MainThreadMarker) with a valid image.
    unsafe { app.setApplicationIconImage(Some(&image)) };
}

fn load_window_icon() -> Option<winit::window::Icon> {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .ok()?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Draw the in-app About screen into a full-window overlay buffer (0x00RRGGBB),
/// in the same centred style as the connect/closed splash. Version comes from the
/// `VERSION` const (package version + git hash); the rest is localized.
fn draw_about(buf: &mut [u32], w: u32, h: u32, font: Option<&text::TextRenderer>) {
    for px in buf.iter_mut() {
        *px = 0x0020_2020;
    }
    let cy = (h / 2) as f32;
    // (y, text, colour, font-px, bitmap-scale fallback)
    let lines: [(f32, String, u32, f32, u32); 7] = [
        (cy - 96.0, "sccm-rc".to_string(), 0x00FF_FFFF, 40.0, 3),
        (cy - 50.0, format!("v{VERSION}"), 0x00C8_D2DC, 20.0, 2),
        (cy - 16.0, t!("about.tagline").to_string(), 0x00B0_C0D0, 19.0, 2),
        (cy + 16.0, "MIT OR Apache-2.0".to_string(), 0x0090_9CA8, 17.0, 2),
        (
            cy + 44.0,
            "github.com/conocidotech/sccm-rc-viewer".to_string(),
            0x0090_9CA8,
            17.0,
            2,
        ),
        (cy + 76.0, t!("about.security").to_string(), 0x0070_C070, 17.0, 2),
        (cy + 118.0, t!("about.dismiss").to_string(), 0x0070_7880, 15.0, 1),
    ];
    for (y, text, color, size_px, scale) in lines {
        if let Some(f) = font {
            f.draw_centered(buf, w, h, y, &text, color, size_px);
        } else {
            toolbar::draw_text_centered(buf, w, h, (y as u32).saturating_sub(4), &text, color, scale);
        }
    }
}


/// Native file picker (PowerShell OpenFileDialog) → the selected path.
#[cfg(windows)]
fn pick_file() -> Option<std::path::PathBuf> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = "Add-Type -AssemblyName System.Windows.Forms; \
        $d=New-Object System.Windows.Forms.OpenFileDialog; \
        $d.Title='Bestand naar de remote sturen'; \
        if($d.ShowDialog() -eq 'OK'){Write-Output $d.FileName}";
    let out = std::process::Command::new("powershell")
        .creation_flags(CREATE_NO_WINDOW)
        .args(["-NoProfile", "-STA", "-Command", script])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(s))
    }
}

#[cfg(not(windows))]
fn pick_file() -> Option<std::path::PathBuf> {
    None
}

/// Read the OS clipboard as text for pasting into the connect field. Runs the
/// `OpenClipboard` call on a throwaway thread with a hard timeout: the picker
/// lives on the winit UI thread, and `OpenClipboard` is famously prone to
/// blocking while another app (Chrome/Office/RDP) holds the clipboard-owner
/// lock, which would freeze the whole window. Mirrors the guard sccm-rc-core
/// uses for the cliprdr channel. Returns None on empty/unavailable/timeout.
#[cfg(windows)]
fn read_clipboard_text() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = clipboard_win::get_clipboard_string()
            .ok()
            .filter(|s| !s.is_empty());
        let _ = tx.send(r);
    });
    rx.recv_timeout(std::time::Duration::from_millis(1500))
        .ok()
        .flatten()
}

#[cfg(not(windows))]
fn read_clipboard_text() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = arboard::Clipboard::new()
            .ok()
            .and_then(|mut c| c.get_text().ok())
            .filter(|s| !s.is_empty());
        let _ = tx.send(r);
    });
    rx.recv_timeout(std::time::Duration::from_millis(1500))
        .ok()
        .flatten()
}

/// Without single sign-on (every OS but Windows) a session needs explicit
/// NTLM credentials, collected by the logon overlay.
const NEEDS_EXPLICIT_LOGON: bool = cfg!(not(windows));

/// The "command" modifier for editing shortcuts in the overlays: Ctrl, plus
/// Cmd on macOS (Cmd+V / Cmd+A are what Mac users press).
fn edit_mod(m: ModifiersState) -> bool {
    m.control_key() || (cfg!(target_os = "macos") && m.super_key())
}

/// `SCCM_RC_CMD_AS_WIN=1`: on macOS send Cmd as the Windows key instead of
/// Ctrl. Read once; it is consulted on every key event.
fn cmd_as_win() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("SCCM_RC_CMD_AS_WIN").is_ok_and(|v| v == "1"))
}

/// Whether the remote sees Ctrl held: the Ctrl key itself, or (macOS, by
/// default) Cmd, which `remote_scancode` forwards as Ctrl. Local shortcuts
/// that watch for a remote Ctrl+V (the UAC paste fallback) must use this.
fn remote_ctrl(m: ModifiersState) -> bool {
    m.control_key() || (cfg!(target_os = "macos") && m.super_key() && !cmd_as_win())
}

/// The logon overlay, with the Keychain checkbox where the platform has one.
/// `fill`: fill in a saved password for `user`; `remember`: the user's
/// earlier checkbox choice to keep (None: the box follows the Keychain).
fn login_overlay(
    target: String,
    user: Option<String>,
    error: Option<String>,
    fill: bool,
    remember: Option<bool>,
) -> login_prompt::LoginPromptOverlay {
    let o = login_prompt::LoginPromptOverlay::new(target, user, error);
    if keychain::AVAILABLE {
        o.with_keychain(keychain::load, fill, remember)
    } else {
        o
    }
}

/// Window title for `target` (empty: no target yet). macOS shows just the
/// target, as Mac apps do (the version is in the About panel); elsewhere
/// "SCCM RC <version> — <target>". `shown` is `servers::display(target)`.
fn window_title(target: &str, shown: &str) -> String {
    if cfg!(target_os = "macos") {
        if target.is_empty() {
            "SCCM RC".to_string()
        } else {
            shown.to_string()
        }
    } else if target.is_empty() {
        format!("SCCM RC {}", env!("CARGO_PKG_VERSION"))
    } else {
        format!("SCCM RC {} — {shown}", env!("CARGO_PKG_VERSION"))
    }
}

/// User name to pre-fill in the logon overlay: `SCCM_USER`, else the last one used.
fn initial_login_user() -> Option<String> {
    std::env::var("SCCM_USER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(recent::last_user)
}

/// Draw an animated "rotating dots" spinner centered at (cx, cy). A bright head
/// dot advances around the ring over time with a fading trail.
fn draw_spinner(
    buf: &mut [u32],
    win_w: u32,
    win_h: u32,
    cx: u32,
    cy: u32,
    elapsed: std::time::Duration,
) {
    const N: usize = 12;
    let r = 24.0f32;
    let head = ((elapsed.as_millis() / 70) as usize) % N;
    for i in 0..N {
        let ang = (i as f32) / (N as f32) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let dx = (ang.cos() * r) as i32;
        let dy = (ang.sin() * r) as i32;
        let dist = (head + N - i) % N; // 0 = head (brightest)
        let b = 235u32.saturating_sub(dist as u32 * 20).max(45);
        let color = (b << 16) | (b << 8) | b;
        for oy in -2i32..=2 {
            for ox in -2i32..=2 {
                if ox * ox + oy * oy > 5 {
                    continue; // round-ish dot
                }
                let px = cx as i32 + dx + ox;
                let py = cy as i32 + dy + oy;
                if px >= 0 && py >= 0 && (px as u32) < win_w && (py as u32) < win_h {
                    buf[(py as u32 * win_w + px as u32) as usize] = color;
                }
            }
        }
    }
}

/// Wake-ups delivered from the RDP task to the winit event loop.
#[derive(Debug, Clone)]
enum UserEvent {
    Frame,
    /// The RDP connection sequence finished (first connect or a reconnect):
    /// time to mirror the local lock keys onto the remote session.
    /// RDP is up; tagged with the session's `running` flag like LoginFailed.
    SessionActive(Arc<AtomicBool>),
    /// The target rejected the explicit logon (wrong password, user not a
    /// Permitted Viewer, …). The session thread has stopped retrying; the UI
    /// reopens the logon overlay with this message. Carries the `running`
    /// flag of the reporting session so the UI can drop a report from a
    /// session it has already stopped or replaced.
    LoginFailed(String, Arc<AtomicBool>),
    #[allow(dead_code)] // retained for an alternate teardown path
    Closed(String),
    /// From the native macOS chrome (menu bar / title-bar toolbar).
    Native(NativeEvent),
    /// A request from the control socket ("Allow control by Claude"), with
    /// where to send its one reply.
    Control(Command, std::sync::mpsc::Sender<Reply>),
    /// The control ticker: run the next due piece of a Claude command.
    ControlTick,
}

/// What the native macOS UI (`mac_ui`) asks the App to do. Built on the main
/// thread inside AppKit callbacks and posted through the event loop, so no
/// App state is touched from inside AppKit.
#[derive(Debug, Clone)]
pub(crate) enum NativeEvent {
    /// A menu or toolbar command; runs exactly like a click on the drawn toolbar.
    Action(ToolbarAction),
    /// The native logon sheet was confirmed or cancelled.
    Logon(login_prompt::LoginOutcome),
    /// The native start sheet picked a target or asked to quit.
    Start(host_prompt::PromptOutcome),
    /// Quit from the app menu, the Dock or the system (logout): close any
    /// sheet, release the session, exit.
    Quit,
}

/// Shared framebuffer written by the RDP task, read by the renderer.
#[derive(Default)]
struct SharedFrame {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    /// Monotonic count of graphics updates, for FPS in the toolbar.
    frames: u64,
    /// Inbound bandwidth (bytes/sec) from the last stats tick.
    bytes_per_sec: u64,
    /// Connection-progress message shown until the first frame paints.
    status: String,
    /// Transport-security summary for the toolbar (e.g. "Kerberos · versleuteld").
    security: String,
    /// True when the link is encrypted AND the server is verified (Kerberos) — the
    /// toolbar shows a green lock; otherwise amber/red.
    secure: bool,
    /// True when the link is encrypted (regardless of verification). Drives the
    /// red (unencrypted) vs amber (encrypted-but-unverified) toolbar lock colour
    /// without string-matching the localized security label.
    encrypted: bool,
    /// GPU path: accumulated dirty region since the last paint (union of all
    /// updates). `None` = nothing changed. The CPU path ignores these.
    dirty: Option<UpdateRegion>,
    /// GPU path: a full-frame copy happened since the last paint (first frame,
    /// size change, or periodic resync) — the whole texture must be re-uploaded.
    full_resync: bool,
    /// Graphics updates as seen by Claude's `wait` (control socket).
    watch: sccm_rc_core::agent::wait::FrameWatch,
}

/// The remote cursor shape, drawn client-side at the local mouse position so it
/// tracks instantly (no server round-trip).
#[derive(Default)]
struct CursorState {
    /// True = draw `rgba`; false = no remote cursor (fall back to OS cursor).
    draw: bool,
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    rgba: Vec<u8>, // top-down RGBA
}

/// Sink that copies decoded frames into the shared framebuffer and wakes
/// the UI thread. Copies only the dirty region (the incoming frame is the full
/// accumulated desktop) and throttles wake-ups so a burst of small order updates
/// doesn't trigger a redraw storm.
struct FrameSink {
    shared: Arc<Mutex<SharedFrame>>,
    cursor: Arc<Mutex<CursorState>>,
    proxy: EventLoopProxy<UserEvent>,
    /// Dirty-region copy plus a periodic full resync copy: the incremental
    /// copy is fast but can drift from the (always-correct) composite if a
    /// region is ever under-reported; the full copy self-heals any such drift.
    /// Shared with the headless MCP backend.
    copier: sccm_rc_core::frame::FrameCopier,
}

impl SessionSink for FrameSink {
    fn on_graphics_update(&mut self, image: &dyn FrameView, region: UpdateRegion) {
        let iw = image.width() as u32;
        let ih = image.height() as u32;
        let src = image.data();
        {
            let mut f = self.shared.lock().unwrap();
            let f = &mut *f;
            // Full copy on first frame, size change, or as a periodic resync that
            // heals any drift from the incremental path (cheap at ~3/sec).
            let copied = self.copier.copy(
                &mut f.rgba,
                (f.width, f.height),
                src,
                (iw, ih),
                region,
                std::time::Instant::now(),
            );
            match copied {
                sccm_rc_core::frame::Copied::Full => {
                    f.width = iw;
                    f.height = ih;
                    f.full_resync = true; // GPU path: re-upload the whole texture.
                }
                // GPU path: accumulate the dirty band (union) since the last paint.
                sccm_rc_core::frame::Copied::Region(acc) => {
                    f.dirty = Some(match f.dirty.take() {
                        Some(d) => UpdateRegion {
                            left: d.left.min(acc.left),
                            top: d.top.min(acc.top),
                            right: d.right.max(acc.right),
                            bottom: d.bottom.max(acc.bottom),
                        },
                        None => acc,
                    });
                }
            }
            f.frames = f.frames.wrapping_add(1);
            f.watch.on_update(std::time::Instant::now());
            f.status.clear(); // first/any frame painted — hide the progress text
        }
        // winit coalesces multiple request_redraw() into a single RedrawRequested,
        // so a burst of region updates results in one redraw — no throttle needed.
        let _ = self.proxy.send_event(UserEvent::Frame);
    }
    fn on_pointer(&mut self, update: PointerUpdate) {
        {
            let mut c = self.cursor.lock().unwrap();
            match update {
                PointerUpdate::Bitmap(p) => {
                    c.draw = true;
                    c.width = p.width;
                    c.height = p.height;
                    c.hotspot_x = p.hotspot_x;
                    c.hotspot_y = p.hotspot_y;
                    c.rgba = p.rgba;
                }
                // No remote cursor → fall back to the local OS cursor.
                PointerUpdate::Hidden | PointerUpdate::SystemDefault => c.draw = false,
            }
        }
        let _ = self.proxy.send_event(UserEvent::Frame);
    }
    fn on_stats(&mut self, stats: SessionStats) {
        self.shared.lock().unwrap().bytes_per_sec = stats.bytes_per_sec;
        let _ = self.proxy.send_event(UserEvent::Frame);
    }
    fn on_status(&mut self, phase: rdp::StatusPhase) {
        let status = match phase {
            rdp::StatusPhase::AttachingSession => t!("status.attaching_session"),
            rdp::StatusPhase::DisplayRecovered => t!("status.display_recovered"),
            rdp::StatusPhase::LoadingDesktop => t!("status.loading_desktop"),
        };
        self.shared.lock().unwrap().status = status.to_string();
        let _ = self.proxy.send_event(UserEvent::Frame);
    }
    fn on_terminate(&mut self, reason: String) {
        // Don't close the window — the reconnect loop will resume the session.
        tracing::warn!(%reason, "server ended session; will reconnect");
    }
}

/// Install a panic hook that ALSO writes synchronously to
/// `%LOCALAPPDATA%\sccm-rc\viewer-panic.log` — the non-blocking tracing
/// appender's queue is lost if the process is killed (e.g. Watson closes a
/// hung viewer), so a panic message in tracing alone often never reaches
/// disk. This separate file uses plain blocking `File::write_all`, so the
/// line is durable before the panic propagates.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let dir = paths::data_dir();
        let when = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let bt = std::backtrace::Backtrace::force_capture();
        let line = format!("[{when}] PANIC thread={thread}\n  {info}\n{bt}\n\n");
        let _ = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(dir.join("viewer-panic.log"))
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(line.as_bytes())
            });
        tracing::error!(thread, %info, "viewer panic");
        default(info);
    }));
}

/// Wire up tracing to BOTH the console AND a daily-rotating file at
/// `%LOCALAPPDATA%\sccm-rc\viewer.log` (falls back to `%TEMP%` if LOCALAPPDATA
/// isn't set). The file is what makes a bug-report bundle actually
/// diagnostic — without it, the most useful WARN/ERROR lines (channel-routing
/// failures, reactivation desyncs, …) vanish into stdout that nothing
/// captures. Returns the appender's `WorkerGuard` — its `Drop` flushes the
/// queue, so `main` must keep it alive for the whole program.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // wgpu/naga log Device::maintain etc. at INFO every frame — quiet them
        // by default so the GPU path doesn't flood the log. RUST_LOG overrides.
        EnvFilter::new("info,sccm_rc_core=info,wgpu_core=warn,wgpu_hal=warn,naga=warn")
    });
    let console = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let log_dir = paths::data_dir();

    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("viewer")
        .filename_suffix("log")
        .max_log_files(7) // keep a week of history; bug-report grabs the current + previous
        .build(&log_dir);
    match appender {
        Ok(app) => {
            let (writer, guard) = tracing_appender::non_blocking(app);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false);
            tracing_subscriber::registry()
                .with(filter)
                .with(console)
                .with(file_layer)
                .init();
            Some(guard)
        }
        Err(e) => {
            tracing_subscriber::registry().with(filter).with(console).init();
            tracing::warn!(error = %e, "could not open viewer.log; console-only logging");
            None
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Console + rolling file appender. The file lives at
    // %LOCALAPPDATA%\sccm-rc\viewer.log and rotates daily (so a single
    // file holds today's session, and yesterday's stays available for
    // bug-report bundling). The non-blocking writer needs its WorkerGuard
    // alive for the lifetime of the program — hence the `_log_guard` binding
    // kept in `main`.
    let _log_guard = init_tracing();
    install_panic_hook();
    let cli = Cli::parse();
    init_locale(cli.lang.as_deref());

    // Enable the proven feature set by default so the GUI works out of the box
    // (graphics handshake, take-over, compression, clipboard). Each can still be
    // overridden from the environment for experiments/diagnostics.
    // SCCM_RC_COMPRESS is now ON by default: the MPPC fidelity bug (#79) is fixed
    // — each fast-path fragment is bulk-decompressed independently (shared 64K
    // history) per FreeRDP, instead of reassembling the raw compressed bytes.
    // Verified against a live capture (126 records, 705 orders, 0 desync) and a
    // live render. Compression cuts wire data ~3x = much smoother over the VPN.
    // Disable with SCCM_RC_COMPRESS=0 for diagnostics.
    for (k, v) in [
        ("SCCM_RC_ARB", "1"),
        ("SCCM_RC_WLC", "1"),
        ("SCCM_RC_MSTSC_CAPS", "1"),
        ("SCCM_RC_ORDERS", "1"),
        ("SCCM_RC_ARB_EVENT", "1"),
        ("SCCM_RC_TAKEOVER", "1"),
        ("SCCM_RC_CLIP", "1"),
        ("SCCM_RC_CURTAIN", "1"),
        ("SCCM_RC_COMPRESS", "1"),
    ] {
        if std::env::var(k).is_err() {
            std::env::set_var(k, v);
        }
    }

    // --all-screens opts into SCCM "All Screens" (the combined multi-monitor
    // desktop). sccm-rc-core reads SCCM_RC_ALLMON when building the connect
    // config; the flag just sets it so the feature stays a deliberate opt-in.
    // Set here (before the session thread spawns) so the env read is race-free.
    if cli.all_screens {
        std::env::set_var("SCCM_RC_ALLMON", "1");
    }

    // Target: CLI arg (like CmRcViewer) or, if absent, the in-window host-prompt
    // overlay collects one after the event loop starts. In --demo mode we never
    // connect, so default to a placeholder name instead of prompting.
    // A saved target's name works on the command line too.
    let cli_target = cli
        .target
        .clone()
        .map(|t| servers::resolve(&t).unwrap_or(t));
    let target = match cli_target.as_deref() {
        Some(t) => t.to_string(),
        None if cli.demo => "DEMO-PC".to_string(),
        None => String::new(), // App starts with the overlay; no session yet.
    };

    // Remember this target for next time's dropdown (only when we actually have one).
    if !target.is_empty() {
        recent::add(&target);
    }
    // Wake-on-LAN: seed the MAC cache from --mac, then wake if asked (or --mac given).
    if let Some(m) = cli.mac.as_deref().and_then(wol::parse_mac) {
        wol::cache_mac(&target, m);
    }
    if cli.wake || cli.mac.is_some() {
        match wol::cached_mac(&target) {
            Some(mac) => match wol::send(mac) {
                Ok(()) => info!(mac = %wol::fmt_mac(mac), %target, "sent Wake-on-LAN magic packet"),
                Err(e) => warn!(error = %e, "Wake-on-LAN failed"),
            },
            None => warn!(%target, "no cached MAC — pass --mac AA-BB-.. or connect once first"),
        }
    }

    let shared = Arc::new(Mutex::new(SharedFrame::default()));
    let cursor = Arc::new(Mutex::new(CursorState::default()));

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // Curtain (privacy) desired state — the toolbar sets it, the session thread
    // observes it and sends the enable/disable event.
    let curtain = Arc::new(AtomicBool::new(false));
    // A file the operator picked to push to the remote (Send File button).
    let file_offer: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::new(Mutex::new(None));
    // Parse any advertised monitor layout; the first --monitor is primary. When
    // a layout is given, the requested desktop size becomes its bounding box.
    let monitors: Vec<rdp::Monitor> = cli
        .monitors
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let m = rdp::parse_monitor(s, i == 0);
            if m.is_none() {
                warn!(spec = %s, "ignoring invalid --monitor geometry (want WIDTHxHEIGHT+LEFT+TOP)");
            }
            m
        })
        .collect();
    let (w, h) = match rdp::monitors_bounding_size(&monitors) {
        Some((bw, bh)) => {
            info!(
                count = monitors.len(),
                width = bw,
                height = bh,
                "advertising multi-monitor layout"
            );
            (bw, bh)
        }
        None => (cli.width, cli.height),
    };

    // Start the first session. `spawn_session` owns the per-host reconnect loop
    // and returns the `running` flag + input sender; the Disconnect button stops
    // it and spawns a fresh one for another host. In --demo mode we skip the
    // network entirely and paint a synthetic multi-monitor desktop instead.
    // In initial-prompt mode (no CLI target) we skip it too — the in-window
    // overlay collects the target and calls `start_session` once the user
    // confirms.
    // Shared counter for the UAC-paste fallback: bumped by CliprdrChannel on
    // every FormatDataRequest, sampled by the App at Ctrl+V key-down / V-up
    // to decide whether the paste reached a clipboard-accepting app.
    let cliprdr_req_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Explicit credentials can come from SCCM_USER / SCCM_PASS (scripted use);
    // otherwise, with a CLI target, the logon overlay asks for them first.
    let initial_creds: Option<LogonCredentials> = if NEEDS_EXPLICIT_LOGON {
        LogonCredentials::from_env().ok()
    } else {
        None
    };
    let login_first =
        NEEDS_EXPLICIT_LOGON && cli_target.is_some() && !cli.demo && initial_creds.is_none();
    let no_initial_session = cli.demo || cli_target.is_none() || login_first;
    let (running, input_tx, done_rx) = if no_initial_session {
        if cli.demo {
            demo_frame(&shared);
        }
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<FastPathInputEvent>>(1);
        let (_done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        (Arc::new(AtomicBool::new(false)), tx, done_rx)
    } else {
        spawn_session(
            target.clone(),
            w,
            h,
            shared.clone(),
            cursor.clone(),
            proxy.clone(),
            curtain.clone(),
            file_offer.clone(),
            monitors.clone(),
            cliprdr_req_counter.clone(),
            initial_creds.clone(),
        )
    };

    // Install the low-level keyboard hook ONCE — it's a process-global
    // resource, so we wire it to the initial input channel. Reconnects swap
    // in a new sender via `App.input_tx`, but the hook keeps using this
    // original handle: the channel only changes when run_active_session
    // restarts, and that path goes through App.input_tx for forwarding too.
    // (A future refactor could expose `winhook::update_tx(...)`; not needed
    // until the hook's stale channel actually causes a dropped Win-key.)
    #[cfg(windows)]
    winhook::install(input_tx.clone());

    let mut app = App {
        shared,
        input_tx: Some(input_tx),
        running,
        proxy,
        width: w,
        height: h,
        monitors,
        done_rx,
        window: None,
        surface: None,
        gpu: None,
        gpu_dump: std::env::var("SCCM_RC_GPU_DUMP").ok(),
        title: window_title(&target, &servers::display(&target)),
        last_cursor: (0, 0),
        buttons_down: 0,
        last_move: std::time::Instant::now(),
        cursor,
        mouse_win: (0.0, 0.0),
        cursor_inside: false,
        closed: None,
        modifiers: ModifiersState::empty(),
        host: target.clone(),
        fullscreen: false,
        view_only: false,
        view: MonitorView::All,
        switch_flash: 0,
        about_open: false,
        fps: 0,
        fps_base: 0,
        fps_t: std::time::Instant::now(),
        recorder: None,
        curtain: curtain.clone(),
        file_offer: file_offer.clone(),
        font: text::TextRenderer::load(),
        connect_start: std::time::Instant::now(),
        rprof_accum: std::time::Duration::ZERO,
        rprof_n: 0,
        rprof_t: std::time::Instant::now(),
        last_paint: std::time::Instant::now(),
        redraw_pending: false,
        last_heartbeat: std::time::Instant::now(),
        host_prompt: if cli_target.is_none() && !cli.demo {
            Some(host_prompt::HostPromptOverlay::new())
        } else {
            None
        },
        type_prompt: None,
        login_prompt: if login_first {
            Some(login_overlay(target.clone(), initial_login_user(), None, true, None))
        } else {
            None
        },
        creds: initial_creds,
        keychain_save: false,
        creds_in_keychain: false,
        remember_choice: false,
        cliprdr_req_counter,
        paste_pending: None,
        locks_sent: None,
        native: None,
        control: None,
        arbiter: control::Arbiter::default(),
        control_job: None,
        control_shot: None,
        control_seen: None,
        control_last_cmd: None,
        control_error: None,
        user_input_at: std::cell::Cell::new(None),
    };
    event_loop.run_app(&mut app)?;
    // Window closed: stop the session and close the input channel, which unblocks
    // run_active_session so it sends the graceful disconnect (releasing the host
    // on the server). Then WAIT for the thread to confirm teardown is done — so we
    // don't exit mid-disconnect and leave the host stuck for the next connect
    // ("existing session"). Bounded, so a mid-connect thread can't block exit.
    app.release_held_buttons(u8::MAX);
    app.running.store(false, Ordering::Relaxed);
    app.input_tx = None;
    let _ = app.done_rx.recv_timeout(std::time::Duration::from_secs(3));
    Ok(())
}

/// Paint a synthetic multi-monitor desktop into the shared frame for `--demo`
/// mode: two side-by-side 1920×1080 "screens" with distinct tinted gradients and a
/// divider, plus a connected/secure status. The frame is two monitors wide, so the
/// full UI renders — toolbar, security lock, and the monitor switcher — for a
/// privacy-safe screenshot without contacting any real machine.
fn demo_frame(shared: &Arc<Mutex<SharedFrame>>) {
    let (w, h) = (3840u32, 1080u32);
    let half = w / 2;
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let on_right = x >= half;
            let gx = (if on_right { x - half } else { x }) as f32 / half as f32;
            let gy = y as f32 / h as f32;
            let (r, g, b) = if x >= half.saturating_sub(1) && x <= half {
                (16.0, 16.0, 18.0) // 2px divider between the two screens
            } else if on_right {
                (40.0 + gx * 90.0, 70.0 + gy * 70.0, 130.0 + gx * 90.0) // screen 2
            } else {
                (30.0 + gx * 70.0, 90.0 + gy * 90.0, 110.0 + gx * 60.0) // screen 1
            };
            rgba[i] = r as u8;
            rgba[i + 1] = g as u8;
            rgba[i + 2] = b as u8;
            rgba[i + 3] = 255;
        }
    }
    let mut f = shared.lock().unwrap();
    f.rgba = rgba;
    f.width = w;
    f.height = h;
    f.frames = 1;
    f.full_resync = true;
    f.status = t!("status.connected").to_string();
    f.secure = true;
    f.encrypted = true;
    f.security = format!("Kerberos \u{00b7} {}", t!("security.encrypted"));
}

/// Spawn the dedicated tokio-runtime thread that drives one host's session, with
/// the auto-reconnect loop. Returns the session's `running` flag (clear it to
/// stop) and the input sender (drop it to unblock the active session). Called for
/// the initial host and again whenever the operator picks another host.
fn spawn_session(
    target: String,
    w: u16,
    h: u16,
    shared: Arc<Mutex<SharedFrame>>,
    cursor: Arc<Mutex<CursorState>>,
    proxy: EventLoopProxy<UserEvent>,
    curtain: Arc<AtomicBool>,
    file_offer: Arc<Mutex<Option<std::path::PathBuf>>>,
    monitors: Vec<rdp::Monitor>,
    cliprdr_req_counter: Arc<std::sync::atomic::AtomicU64>,
    creds: Option<LogonCredentials>,
) -> (Arc<AtomicBool>, InputSender, std::sync::mpsc::Receiver<()>) {
    let running = Arc::new(AtomicBool::new(true));
    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<Vec<FastPathInputEvent>>(256);
    // Signals when the thread has fully wound down — i.e. the graceful disconnect
    // (MCS Disconnect-Provider-Ultimatum) has been sent so the SCCM server
    // releases the host. The UI waits on this before exiting / reconnecting, so
    // the next connect doesn't hit "existing session".
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let running_thread = running.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async move {
            // Auto-reconnect: a transport desync, server reset, or a dropped link
            // (detected via TCP keepalive) ends the session; rather than freezing
            // the window, reconnect and resume — the status overlay shows progress.
            let mut input_rx = input_rx;
            let mut backoff = RECONNECT_MIN;
            while running_thread.load(Ordering::Relaxed) {
                let started = std::time::Instant::now();
                match run_session(
                    &target,
                    w,
                    h,
                    shared.clone(),
                    cursor.clone(),
                    proxy.clone(),
                    &mut input_rx,
                    curtain.clone(),
                    file_offer.clone(),
                    &monitors,
                    cliprdr_req_counter.clone(),
                    creds.as_ref(),
                    &running_thread,
                )
                .await
                {
                    Ok(()) => info!("session ended"),
                    Err(e) => {
                        warn!(error = %e, "session error");
                        // A rejected explicit logon must NOT be retried: every
                        // reconnect is another failed logon and would soon lock
                        // the account. Hand it back to the UI, tagged with this
                        // thread's `running` flag so a stale report from a
                        // session the user already left is ignored there.
                        if is_logon_failure(&e) {
                            let _ = proxy.send_event(UserEvent::LoginFailed(
                                e.to_string(),
                                running_thread.clone(),
                            ));
                            break;
                        }
                    }
                }
                if !running_thread.load(Ordering::Relaxed) {
                    break;
                }
                backoff = next_backoff(backoff, started.elapsed());
                // Sleep in short slices so Disconnect / window close still
                // ends the thread promptly during a long back-off.
                let until = std::time::Instant::now() + backoff;
                while running_thread.load(Ordering::Relaxed) && std::time::Instant::now() < until {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        });
        // run_session() ran session.disconnect() before returning, so by here the
        // host has been released. Tell the UI it's safe to exit / reconnect.
        let _ = done_tx.send(());
    });
    (running, input_tx, done_rx)
}

/// Integer UI scale for the toolbar and overlays: 2 on a Retina display, 1 on
/// a standard one. Those are drawn at logical size and pixel-doubled, so they
/// keep their designed size; the remote desktop itself stays at full physical
/// resolution. `SCCM_RC_UI_SCALE` overrides it (1–4).
///
/// The factor is rounded down, so a fractional Windows scale (125 %–175 %)
/// keeps the 1× UI it always had instead of jumping to 200 %. Called on every
/// pointer move and paint, so the override is read once.
fn ui_scale_of(w: &Window) -> u32 {
    static OVERRIDE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    let forced = *OVERRIDE.get_or_init(|| {
        std::env::var("SCCM_RC_UI_SCALE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| v.clamp(1, 4))
    });
    forced.unwrap_or_else(|| (w.scale_factor().floor() as u32).clamp(1, 4))
}

/// Largest rect with the desktop's aspect ratio that fits (centred) inside the
/// `area_w × area_h` area. Returns `(x, y, w, h)` relative to the area; the rest
/// of the area is letterbox/pillarbox bars.
fn fit_rect(desk_w: u32, desk_h: u32, area_w: u32, area_h: u32) -> (u32, u32, u32, u32) {
    let (dw, dh) = (desk_w.max(1) as u64, desk_h.max(1) as u64);
    let (aw, ah) = (area_w.max(1) as u64, area_h.max(1) as u64);
    let (w, h) = if aw * dh > dw * ah {
        // Area is wider than the desktop: height-limited.
        (((dw * ah + dh / 2) / dh).clamp(1, aw), ah)
    } else {
        (aw, ((dh * aw + dw / 2) / dw).clamp(1, ah))
    };
    (((aw - w) / 2) as u32, ((ah - h) / 2) as u32, w as u32, h as u32)
}

/// Window-space rect (below the toolbar strip) the shown desktop region occupies.
fn desktop_rect(
    crop: (u32, u32, u32, u32),
    win_w: u32,
    win_h: u32,
    bar_h: u32,
) -> (u32, u32, u32, u32) {
    let (x, y, w, h) = fit_rect(crop.2, crop.3, win_w, win_h.saturating_sub(bar_h));
    (x, y + bar_h, w, h)
}

/// Bit used to track a held mouse button in `App::buttons_down`.
fn button_bit(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        _ => 4,
    }
}

/// The release events for every button held in `held` (bit 1 left, 2 right; other
/// buttons carry no pointer flag and need none); clears `held`.
fn release_pdus(held: &mut u8, x: u16, y: u16) -> Vec<MousePdu> {
    let mut out = Vec::new();
    for (bit, flag) in [(1u8, PointerFlags::LEFT_BUTTON), (2, PointerFlags::RIGHT_BUTTON)] {
        if *held & bit != 0 {
            out.push(MousePdu {
                flags: flag,
                number_of_wheel_rotation_units: 0,
                x_position: x,
                y_position: y,
            });
        }
    }
    *held = 0;
    out
}

/// The point lies inside `rect` (`x, y, w, h` in window pixels).
fn mouse_in_rect((mx, my): (f64, f64), rect: (u32, u32, u32, u32)) -> bool {
    mx >= rect.0 as f64
        && my >= rect.1 as f64
        && mx < (rect.0 + rect.2) as f64
        && my < (rect.1 + rect.3) as f64
}

/// Background fill for the letterbox bars on the CPU path (GPU: `BAR_CLEAR`).
const CPU_BAR_FILL: u32 = 0x0007_0708;

/// Map a window-space point through `rect` (see [`desktop_rect`]) into desktop
/// coordinates of the `crop` sub-rect. `None` when the point is in the bars.
fn map_to_desktop(
    x: f64,
    y: f64,
    rect: (u32, u32, u32, u32),
    crop: (u32, u32, u32, u32),
    fb: (u32, u32),
) -> Option<(u16, u16)> {
    let (rx, ry, rw, rh) = (rect.0 as f64, rect.1 as f64, rect.2 as f64, rect.3 as f64);
    if x < rx || y < ry || x >= rx + rw || y >= ry + rh {
        return None;
    }
    let (cx, cy, cw, ch) = (crop.0 as f64, crop.1 as f64, crop.2 as f64, crop.3 as f64);
    let dx = (cx + (x - rx) * cw / rw).clamp(0.0, fb.0.saturating_sub(1) as f64);
    let dy = (cy + (y - ry) * ch / rh).clamp(0.0, fb.1.saturating_sub(1) as f64);
    Some((dx as u16, dy as u16))
}

/// Nearest-neighbour upscale of a logical `lw × lh` buffer by `k` into a
/// `pw × ph` physical buffer. Rows/columns past `lw*k` / `lh*k` (when the
/// physical size is not a multiple of `k`) repeat the last logical pixel.
fn upscale_into(src: &[u32], lw: u32, lh: u32, k: u32, dst: &mut [u32], pw: u32, ph: u32) {
    if lw == 0 || lh == 0 {
        return;
    }
    for py in 0..ph {
        let ly = (py / k).min(lh - 1);
        let srow = &src[(ly * lw) as usize..((ly + 1) * lw) as usize];
        let drow = &mut dst[(py * pw) as usize..((py + 1) * pw) as usize];
        for (px, d) in drow.iter_mut().enumerate() {
            *d = srow[((px as u32) / k).min(lw - 1) as usize];
        }
    }
}

/// PS/2 set-1 `(scancode, extended)` to send to the remote for a physical key.
///
/// Windows: winit's hardware scancode already is set 1 (0xE000 = extended).
/// Elsewhere winit reports native keycodes (Apple kVK_* on macOS), so the key
/// is mapped by position via `keymap`. On macOS Cmd is sent as Ctrl, so
/// Cmd+C / Cmd+V / Cmd+A do what a Mac user expects on the remote; set
/// `SCCM_RC_CMD_AS_WIN=1` to send Cmd as the Windows key instead.
fn remote_scancode(key: &PhysicalKey) -> Option<(u8, bool)> {
    if cfg!(windows) {
        let sc = key.to_scancode()?;
        return Some(((sc & 0xFF) as u8, sc & 0xE000 == 0xE000 || sc > 0xFF));
    }
    #[cfg(target_os = "macos")]
    if let Some(sc) = keymap::mac_iso_aware(key) {
        return Some(sc);
    }
    let PhysicalKey::Code(code) = key else {
        return None;
    };
    let code = *code;
    if cfg!(target_os = "macos") && !cmd_as_win() {
        match code {
            KeyCode::SuperLeft => return keymap::set1(KeyCode::ControlLeft),
            KeyCode::SuperRight => return keymap::set1(KeyCode::ControlRight),
            _ => {}
        }
    }
    keymap::set1(code)
}

const RECONNECT_MIN: std::time::Duration = std::time::Duration::from_secs(1);
const RECONNECT_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// Delay before the next reconnect: reset to the minimum after a session that
/// stayed up for a while, otherwise double (capped). Without this a dead
/// target or tunnel got hammered once a second indefinitely.
fn next_backoff(
    current: std::time::Duration,
    session_lasted: std::time::Duration,
) -> std::time::Duration {
    if session_lasted >= RECONNECT_MAX {
        RECONNECT_MIN
    } else {
        (current * 2).min(RECONNECT_MAX)
    }
}

/// A bring-up error with explicit credentials that means "these credentials
/// (or this user) won't work here": the target refused the logon, or SSPI
/// failed during the handshake. Raised only by `run_session`'s connect step,
/// so the same error kinds later in a session (a sealed-stream desync is an
/// `Error::Sspi` too) still go through the normal reconnect.
#[derive(Debug)]
struct LogonFailure(sccm_rc_core::Error);

impl std::fmt::Display for LogonFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for LogonFailure {}

/// Errors that must not be retried with the same credentials, as opposed to
/// transient transport trouble worth a reconnect.
fn is_logon_failure(e: &anyhow::Error) -> bool {
    e.downcast_ref::<LogonFailure>().is_some()
}

async fn run_session(
    target: &str,
    w: u16,
    h: u16,
    shared: Arc<Mutex<SharedFrame>>,
    cursor: Arc<Mutex<CursorState>>,
    proxy: EventLoopProxy<UserEvent>,
    input_rx: &mut rdp::InputReceiver,
    curtain: Arc<AtomicBool>,
    file_offer: Arc<Mutex<Option<std::path::PathBuf>>>,
    monitors: &[rdp::Monitor],
    cliprdr_req_counter: Arc<std::sync::atomic::AtomicU64>,
    creds: Option<&LogonCredentials>,
    session_flag: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    shared.lock().unwrap().status =
        t!("status.connecting_to", target => servers::display(target)).to_string();
    let _ = proxy.send_event(UserEvent::Frame);
    // Bound the WHOLE bring-up (TCP connect + SSPI handshake + grant), not just
    // the TCP connect: a peer that completes the TCP handshake but then stalls
    // mid-greeting/SSPI would otherwise hang this session thread indefinitely and
    // leak it past a host-switch. 20 s is far above a healthy sub-second connect.
    let mut session = match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        SccmSession::connect_with(target, creds),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            // Show a clear, actionable status for the common "existing session"
            // case (usually our own session that wasn't released) instead of
            // silently re-showing "Verbinden..." on every retry.
            if matches!(e, sccm_rc_core::Error::ExistingSession) {
                shared.lock().unwrap().status =
                    t!("status.existing_session", target => target).to_string();
                let _ = proxy.send_event(UserEvent::Frame);
            }
            if creds.is_some()
                && matches!(
                    e,
                    sccm_rc_core::Error::LogonRejected(_) | sccm_rc_core::Error::Sspi(_)
                )
            {
                return Err(LogonFailure(e).into());
            }
            return Err(e.into());
        }
        Err(_) => anyhow::bail!("{}", t!("status.connect_timeout", target => target)),
    };
    shared.lock().unwrap().status = t!("status.setting_up_display").to_string();
    let _ = proxy.send_event(UserEvent::Frame);
    // Best-effort: remember this host's MAC (from the ARP table now that we've
    // contacted it) so a later Wake-on-LAN can boot it if it's powered off.
    // Fire-and-forget — do NOT await: the ARP lookup shells out (~0.6s measured)
    // and must not delay the RDP negotiation / first paint.
    {
        let t = target.to_string();
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || wol::lookup_and_cache(&t)).await;
        });
    }
    let grant = format!("{:?}", session.grant());
    info!(grant = %grant, "session established");
    audit::log_event(target, &grant, "connect", None);
    // Transport-security summary for the toolbar.
    {
        let (encrypted, verified, package) = session.security();
        info!(encrypted, verified, package = ?package, "transport security");
        let mut f = shared.lock().unwrap();
        f.secure = encrypted && verified;
        f.encrypted = encrypted;
        f.security = match (encrypted, package) {
            (true, Some(p)) => format!("{p} \u{00b7} {}", t!("security.encrypted")),
            (true, None) => t!("security.encrypted").to_string(),
            (false, _) => t!("security.unencrypted").to_string(),
        };
    }
    let started = std::time::Instant::now();
    // From here the server has granted an RC session, so we MUST disconnect on
    // every exit path (including a failed RDP negotiation) — otherwise the host is
    // left occupied and the next connect trips "existing session".
    let res = async {
        let (result, initial_buf, share_id) =
            rdp::connect_rdp(&mut session, w, h, monitors, cliprdr_req_counter.clone()).await?;
        info!("RDP active — streaming");
        let _ = proxy.send_event(UserEvent::SessionActive(session_flag.clone()));
        let mut sink = FrameSink {
            shared,
            cursor,
            proxy,
            copier: sccm_rc_core::frame::FrameCopier::new(),
        };
        rdp::run_active_session(
            &mut session,
            result,
            initial_buf,
            share_id,
            &mut sink,
            input_rx,
            curtain,
            file_offer,
            cliprdr_req_counter,
        )
        .await
    }
    .await;
    // Graceful teardown so the server releases the shadow/host before we reconnect.
    session.disconnect().await;
    audit::log_event(
        target,
        &grant,
        "disconnect",
        Some(started.elapsed().as_secs()),
    );
    res?;
    Ok(())
}

/// Which monitor of a multi-monitor (All-Screens) target the viewer shows.
/// `All` = the whole combined framebuffer; `Screen(k)` = crop to monitor `k`
/// (0-based). This is a purely client-side crop of the already-received combined
/// desktop — switching is instant and needs no reconnect (RRCV-20), unlike the
/// original CmRcViewer which can only ever show the full stitched image.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MonitorView {
    All,
    Screen(u32),
}

struct App {
    shared: Arc<Mutex<SharedFrame>>,
    input_tx: Option<InputSender>,
    running: Arc<AtomicBool>,
    /// Ingredients to (re)spawn a session thread when the user switches host.
    proxy: EventLoopProxy<UserEvent>,
    width: u16,
    height: u16,
    /// Monitor layout to advertise on (re)connect; empty = single monitor.
    monitors: Vec<rdp::Monitor>,
    /// Signalled when the current session thread has finished its graceful
    /// disconnect; the UI waits on it before exiting / reconnecting.
    done_rx: std::sync::mpsc::Receiver<()>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    /// GPU renderer (wgpu), used by default. `None` = the softbuffer CPU fallback
    /// (forced with SCCM_RC_GPU=0, or when GPU init fails).
    gpu: Option<gpu::GpuRenderer>,
    /// Debug: SCCM_RC_GPU_DUMP=<path> dumps the first connected GPU frame to PNG
    /// (GDI can't screenshot a Vulkan surface). Cleared after one dump.
    gpu_dump: Option<String>,
    title: String,
    last_cursor: (u16, u16),
    /// Mouse buttons the viewer has sent as down (bit 1 left, 2 right, 4 other).
    buttons_down: u8,
    last_move: std::time::Instant,
    cursor: Arc<Mutex<CursorState>>,
    mouse_win: (f64, f64),
    cursor_inside: bool,
    closed: Option<String>,
    modifiers: ModifiersState,
    host: String,
    fullscreen: bool,
    /// Local view-only lock: when true, suppress all input to the remote.
    view_only: bool,
    /// Which monitor of an All-Screens target is shown (client-side crop).
    view: MonitorView,
    /// Frames left to show the transient "Switching…" indicator in the toolbar
    /// state line after a monitor-view change (RRCV-20 feedback).
    switch_flash: u32,
    /// True while the in-app About overlay is shown (toggled by the toolbar
    /// button; dismissed by a click anywhere or Esc).
    about_open: bool,
    fps: u32,
    fps_base: u64,
    fps_t: std::time::Instant,
    recorder: Option<record::Recorder>,
    /// Curtain (privacy) desired state, shared with the session thread.
    curtain: Arc<AtomicBool>,
    /// File the operator picked to push to the remote, shared with the session.
    file_offer: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// Anti-aliased UI font (None → 8x8 bitmap fallback).
    font: Option<text::TextRenderer>,
    /// When the current connection attempt started, for the spinner animation.
    connect_start: std::time::Instant,
    /// Viewer-side render profiling (SCCM_RC_PROFILE=1): accumulated paint time +
    /// count, logged ~1x/s. This is the blind spot the core profile misses.
    rprof_accum: std::time::Duration,
    rprof_n: u32,
    rprof_t: std::time::Instant,
    /// Paint-rate cap: a flood of update events would otherwise trigger a full
    /// rescale on each (~300/s measured). We repaint at most ~60 fps and defer
    /// extra requests, scheduling a trailing paint so the latest frame still lands.
    last_paint: std::time::Instant,
    redraw_pending: bool,
    /// Last time the winit (main) thread emitted a heartbeat log line. If the
    /// app hangs, this is the timestamp BEFORE the freeze — comparing it to
    /// the panic/Watson timestamp tells you whether the UI thread was the one
    /// stuck. Bumped from `about_to_wait` every ~5 s.
    last_heartbeat: std::time::Instant,
    /// In-window host-picker overlay. Some = the overlay is visible and
    /// consumes keyboard input; no session is running yet. Set at startup
    /// when no CLI target was given, and by Disconnect to pick another host.
    host_prompt: Option<host_prompt::HostPromptOverlay>,
    /// Send-keystrokes overlay. Some = the operator is typing text that
    /// will be replayed as scancodes on the remote (used to enter
    /// credentials into a UAC prompt where cliprdr is blocked).
    type_prompt: Option<type_text::TypeTextOverlay>,
    /// Logon overlay (non-Windows): Some = collecting DOMAIN\user + password
    /// for the chosen host; no session is running yet.
    login_prompt: Option<login_prompt::LoginPromptOverlay>,
    /// Save `creds` to the Keychain once the session is up (box ticked).
    keychain_save: bool,
    /// The Keychain holds exactly `creds` (filled in from it, or saved after
    /// this logon), so a rejection must delete it.
    creds_in_keychain: bool,
    /// The checkbox choice of the last logon, offered again after a rejection.
    remember_choice: bool,
    /// Explicit logon credentials for the current host (non-Windows), held in
    /// memory only and reused by the auto-reconnect loop.
    creds: Option<LogonCredentials>,
    /// Shared with the session thread's CliprdrChannel: bumped on every
    /// inbound FormatDataRequest. Zero-cost sampling from the UI thread.
    cliprdr_req_counter: Arc<std::sync::atomic::AtomicU64>,
    /// State for the Ctrl+V → UAC fallback: on plain Ctrl+V key-down we
    /// snapshot the local clipboard + the current cliprdr counter. On V
    /// key-up (with a 100 ms floor so cliprdr has fair chance to fire),
    /// if the counter didn't tick we type the clipboard as scancodes —
    /// otherwise a normal cliprdr paste happened and we do nothing.
    paste_pending: Option<PastePending>,
    /// Lock-key state last sent to the remote via an RDP sync event (macOS
    /// only, see `keymap::local_lock_flags`). `None` = not synced yet.
    locks_sent: Option<sccm_rc_core::rdp::SynchronizeFlags>,
    /// Native macOS chrome (menu bar + title-bar toolbar); `None` elsewhere or
    /// with `SCCM_RC_NATIVE_UI=0`, where the drawn toolbar strip is used.
    native: Option<mac_ui::NativeUi>,
    /// "Allow control by Claude" (native macOS chrome only): the control
    /// socket and its ticker while the toggle is on. Off on every start.
    control: Option<(control::Server, control::Ticker)>,
    arbiter: control::Arbiter,
    /// The Claude command running now and where its reply goes.
    control_job: Option<(control::Job, std::sync::mpsc::Sender<Reply>)>,
    /// Geometry of the last screenshot Claude got (maps its coordinates).
    control_shot: Option<ShotSpace>,
    /// When a Claude command last ran, and whether the user took over from
    /// it: drives the subtitle for [`CONTROL_SHOWN`].
    control_seen: Option<(std::time::Instant, bool)>,
    /// The previous Claude command, for `status`'s idle seconds.
    control_last_cmd: Option<std::time::Instant>,
    /// Why switching control on failed, shown in the subtitle for a while.
    control_error: Option<(std::time::Instant, String)>,
    /// Set when the user's own input went to the remote; picked up by
    /// `control_poll_user` (input handlers only have `&self`).
    user_input_at: std::cell::Cell<Option<std::time::Instant>>,
}

/// How long the subtitle says "Controlled by Claude" after a command.
const CONTROL_SHOWN: std::time::Duration = std::time::Duration::from_secs(5);

/// The viewer session as a Claude command sees it.
struct LiveTarget<'a> {
    tx: Option<&'a InputSender>,
    running: bool,
    view_only: bool,
    shared: &'a Mutex<SharedFrame>,
    shot: Option<ShotSpace>,
    host: &'a str,
}

impl control::Target for LiveTarget<'_> {
    fn desk_size(&self) -> (u16, u16) {
        let f = self.shared.lock().unwrap();
        if f.rgba.is_empty() {
            return (0, 0);
        }
        (
            u16::try_from(f.width).unwrap_or(0),
            u16::try_from(f.height).unwrap_or(0),
        )
    }

    fn shot(&self) -> Option<ShotSpace> {
        self.shot
    }

    fn alive(&self) -> Result<(), Reply> {
        if self.tx.is_some() && self.running {
            Ok(())
        } else {
            Err(exec::err(
                ErrorCode::NotConnected,
                "the viewer's session has ended",
            ))
        }
    }

    fn send(&mut self, events: Vec<FastPathInputEvent>) -> Result<(), Reply> {
        if self.view_only {
            return Err(exec::view_only());
        }
        let tx = self
            .tx
            .ok_or_else(|| exec::err(ErrorCode::NotConnected, "the viewer's session has ended"))?;
        tx.try_send(events)
            .map_err(|_| exec::session_err("could not hand the input to the session"))
    }

    fn release(&mut self, events: Vec<FastPathInputEvent>) {
        if let Some(tx) = self.tx {
            let _ = tx.try_send(events);
        }
    }

    fn watch(&self) -> sccm_rc_core::agent::wait::FrameWatch {
        self.shared.lock().unwrap().watch.clone()
    }

    fn audit(&mut self, action: &str) {
        audit::log_agent(self.host, action);
    }
}

struct PastePending {
    started: std::time::Instant,
    baseline: u64,
    clip: String,
    v_released: bool,
}

impl App {
    /// Mirror the local Caps Lock state onto the remote (Num Lock on) with an
    /// RDP sync event. macOS only: winit never reports Caps Lock there. Sent
    /// when the state differs from the last sync, or always when `force`.
    fn sync_locks(&mut self, force: bool) {
        if self.view_only || self.input_tx.is_none() {
            return;
        }
        let Some(flags) = keymap::local_lock_flags() else {
            return;
        };
        if !force && self.locks_sent == Some(flags) {
            return;
        }
        if let Some(tx) = &self.input_tx {
            let _ = tx.try_send(vec![FastPathInputEvent::SyncEvent(flags)]);
        }
        self.locks_sent = Some(flags);
        tracing::debug!(
            caps = flags.contains(sccm_rc_core::rdp::SynchronizeFlags::CAPS_LOCK),
            num = flags.contains(sccm_rc_core::rdp::SynchronizeFlags::NUM_LOCK),
            force,
            "lock-key sync"
        );
    }

    /// The user's input can reach the remote (not view-only, channel present).
    fn can_send_input(&self) -> bool {
        !self.view_only && self.input_tx.is_some()
    }

    /// Send releases (at `last_cursor`) for the held buttons in `mask`, and
    /// clear them from `buttons_down`.
    fn release_held_buttons(&mut self, mask: u8) {
        let mut held = self.buttons_down & mask;
        self.buttons_down &= !mask;
        let (x, y) = self.last_cursor;
        for pdu in release_pdus(&mut held, x, y) {
            self.send_input(FastPathInputEvent::MouseEvent(pdu));
        }
    }

    /// Forward the user's own input to the remote.
    fn send_input(&self, ev: FastPathInputEvent) {
        if self.view_only {
            return; // local view-only lock — don't control the remote
        }
        if let Some(tx) = &self.input_tx {
            // A plain pointer move does not take over from Claude.
            if control::counts_as_takeover(&ev) {
                self.mark_user_input();
            }
            let _ = tx.try_send(vec![ev]);
        }
    }

    /// The user's own input reached the remote: a running Claude command
    /// must stop (see `control_poll_user`).
    fn mark_user_input(&self) {
        self.user_input_at.set(Some(std::time::Instant::now()));
    }

    /// Inject the Secure Attention Sequence (Ctrl+Alt+Del) to the remote as one
    /// batch: Ctrl↓ Alt↓ Del↓ Del↑ Alt↑ Ctrl↑. Set-1 scancodes: LCtrl=0x1D,
    /// LAlt=0x38, dedicated Delete=0x53 (extended). Sent together so the chord
    /// registers regardless of the local modifier keys the user is holding.
    fn send_ctrl_alt_del(&self) {
        if self.view_only {
            return;
        }
        let down = KeyboardFlags::empty();
        let up = KeyboardFlags::RELEASE;
        let ext = KeyboardFlags::EXTENDED;
        let seq = vec![
            FastPathInputEvent::KeyboardEvent(down, 0x1D), // Ctrl down
            FastPathInputEvent::KeyboardEvent(down, 0x38), // Alt down
            FastPathInputEvent::KeyboardEvent(ext, 0x53),  // Del down (extended)
            FastPathInputEvent::KeyboardEvent(ext | up, 0x53), // Del up
            FastPathInputEvent::KeyboardEvent(up, 0x38),   // Alt up
            FastPathInputEvent::KeyboardEvent(up, 0x1D),   // Ctrl up
        ];
        if let Some(tx) = &self.input_tx {
            self.mark_user_input();
            let _ = tx.try_send(seq);
        }
        info!("sent Ctrl+Alt+Del (SAS) to remote");
    }

    /// Inject a Windows-key tap (press+release) to the remote — the windowed-mode
    /// equivalent of the Win key, which the local OS otherwise steals to open the
    /// local Start menu. Set-1 scancode for the Left-Win key is 0xE05B (extended).
    /// Triggered by the toolbar button or by Ctrl+Esc.
    fn send_win_key(&self) {
        if self.view_only {
            return;
        }
        let ext = KeyboardFlags::EXTENDED;
        let up = KeyboardFlags::RELEASE;
        let seq = vec![
            FastPathInputEvent::KeyboardEvent(ext, 0x5B),      // Win down (extended)
            FastPathInputEvent::KeyboardEvent(ext | up, 0x5B), // Win up
        ];
        if let Some(tx) = &self.input_tx {
            self.mark_user_input();
            let _ = tx.try_send(seq);
        }
        info!("sent Windows-key tap to remote");
    }

    /// Toolbar height in physical pixels (logical height × UI scale).
    fn bar_h_phys(&self) -> u32 {
        self.bar_h_logical() * self.window.as_deref().map(ui_scale_of).unwrap_or(1)
    }

    /// Height of the drawn toolbar strip in logical pixels: 0 when the native
    /// macOS toolbar replaces it (the desktop then uses the full height).
    fn bar_h_logical(&self) -> u32 {
        if self.native.is_some() {
            0
        } else {
            toolbar::TOOLBAR_H
        }
    }

    /// A confirmed or cancelled logon, from the drawn overlay or the native
    /// sheet: store the credentials and connect, or go back to the start screen.
    fn on_login_outcome(&mut self, outcome: login_prompt::LoginOutcome) {
        match outcome {
            login_prompt::LoginOutcome::Confirmed {
                target,
                creds,
                remember,
                from_keychain,
            } => {
                recent::set_last_user(&creds.display_name());
                // Unticked box: the user doesn't want it saved (any more).
                if keychain::AVAILABLE && !remember {
                    keychain::delete(&creds.display_name());
                }
                // Save only after a successful logon (SessionActive), and
                // only if it is not already the saved one.
                self.keychain_save = remember && !from_keychain;
                self.creds_in_keychain = remember && from_keychain;
                self.remember_choice = remember;
                self.creds = Some(creds);
                self.start_session(target);
            }
            login_prompt::LoginOutcome::Cancelled => self.open_start(),
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// A target picked (or quit) on the start screen, from the drawn overlay
    /// or the native sheet. Returns false when the app is exiting.
    fn on_host_outcome(
        &mut self,
        outcome: host_prompt::PromptOutcome,
        event_loop: &ActiveEventLoop,
    ) -> bool {
        match outcome {
            host_prompt::PromptOutcome::Confirmed(target) => {
                self.host_prompt = None;
                if NEEDS_EXPLICIT_LOGON && self.creds.is_none() {
                    self.open_logon(target, initial_login_user(), None, true, None);
                } else {
                    self.start_session(target);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                true
            }
            host_prompt::PromptOutcome::Cancelled => {
                self.closed = Some(t!("status.disconnected").to_string());
                event_loop.exit();
                false
            }
        }
    }

    /// Show the start screen: the native sheet on macOS, else the overlay.
    fn open_start(&mut self) {
        if let Some(native) = self.native.as_ref() {
            self.host_prompt = None;
            native.show_start();
        } else {
            self.host_prompt = Some(host_prompt::HostPromptOverlay::new());
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Ask for the logon: the native sheet on macOS, else the drawn overlay.
    /// `fill`: fill in a saved password; `remember`: earlier checkbox choice.
    fn open_logon(
        &mut self,
        target: String,
        user: Option<String>,
        error: Option<String>,
        fill: bool,
        remember: Option<bool>,
    ) {
        if let Some(native) = self.native.as_ref() {
            self.login_prompt = None;
            native.show_logon(&target, user, error, fill, remember);
        } else {
            self.login_prompt = Some(login_overlay(target, user, error, fill, remember));
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// The window's real full-screen state: on macOS the user can also leave
    /// (or enter) full screen with the green title-bar button, which
    /// `self.fullscreen` does not see.
    fn is_fullscreen(&self) -> bool {
        self.window
            .as_ref()
            .map_or(self.fullscreen, |w| w.fullscreen().is_some())
    }

    /// A native sheet covers the window (macOS): draw a calm background only.
    fn native_sheet_open(&self) -> bool {
        self.native.as_ref().is_some_and(|n| n.sheet_open())
    }

    /// Push the session state into the native chrome (no-op without it).
    fn update_native_chrome(&self) {
        let Some(native) = self.native.as_ref() else {
            return;
        };
        let (connected, status_msg, security) = {
            let f = self.shared.lock().unwrap();
            (
                f.width != 0 && f.height != 0 && !f.rgba.is_empty(),
                f.status.clone(),
                f.security.clone(),
            )
        };
        let has_session = self.input_tx.is_some() && self.running.load(Ordering::Relaxed);
        let subtitle = if let Some(reason) = &self.closed {
            t!("status.disconnected_reason", reason => reason).to_string()
        } else if connected {
            let mode = if self.view_only {
                t!("mode.view_only")
            } else {
                t!("mode.control")
            };
            if security.is_empty() {
                mode.to_string()
            } else {
                format!("{security} \u{00b7} {mode}")
            }
        } else if has_session {
            if status_msg.is_empty() {
                t!("status.connecting").to_string()
            } else {
                status_msg
            }
        } else {
            String::new()
        };
        let subtitle = match self.control_label() {
            Some(c) if subtitle.is_empty() => c,
            Some(c) => format!("{subtitle} \u{00b7} {c}"),
            None => subtitle,
        };
        native.update(&mac_ui::Chrome {
            subtitle,
            connected,
            has_session,
            view_only: self.view_only,
            curtain: self.curtain.load(Ordering::Relaxed),
            recording: self.recorder.is_some(),
            fullscreen: self.is_fullscreen(),
            monitor: if connected { self.monitor_label() } else { None },
            agent_control: self.control.is_some(),
        });
    }

    /// Subtitle note while Claude controls (or the user just took over).
    fn control_label(&self) -> Option<String> {
        if let Some((at, reason)) = &self.control_error {
            if at.elapsed() < CONTROL_SHOWN {
                return Some(t!("control.unavailable", reason => reason).to_string());
            }
        }
        self.control.as_ref()?;
        if self.control_job.is_some() {
            return Some(t!("control.active").to_string());
        }
        let (at, took_over) = self.control_seen?;
        (at.elapsed() < CONTROL_SHOWN).then(|| {
            if took_over {
                t!("control.user_took_over").to_string()
            } else {
                t!("control.active").to_string()
            }
        })
    }

    /// "Allow control by Claude" on/off. Off aborts a running command
    /// (keys and buttons released) and closes the socket.
    fn toggle_agent_control(&mut self) {
        if self.control.is_some() {
            self.control_abort(
                ErrorCode::ControlDisabled,
                "control by Claude was switched off in the viewer",
            );
            if let Some((mut server, _ticker)) = self.control.take() {
                server.stop();
            }
            audit::log_agent(&self.host, "control by Claude switched off");
            info!("control by Claude switched off");
            return;
        }
        match control::Server::start(&paths::data_dir(), self.proxy.clone()) {
            Ok(server) => {
                let proxy = self.proxy.clone();
                let ticker = control::Ticker::new(move || {
                    let _ = proxy.send_event(UserEvent::ControlTick);
                });
                self.control = Some((server, ticker));
                self.control_seen = None;
                self.control_error = None;
                audit::log_agent(&self.host, "control by Claude allowed");
                info!("control by Claude allowed");
            }
            Err(e) => {
                warn!(error = %e, "could not open the control socket");
                self.control_error = Some((std::time::Instant::now(), e.to_string()));
                // Wake the loop once the note has expired, to clear it.
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(CONTROL_SHOWN);
                    let _ = proxy.send_event(UserEvent::Frame);
                });
            }
        }
    }

    /// Pick up the user's input since the last call: it starts the
    /// cooldown and aborts a running Claude command.
    fn control_poll_user(&mut self) {
        let Some(at) = self.user_input_at.take() else {
            return;
        };
        self.arbiter.on_user_input(at);
        if self.arbiter.should_abort(std::time::Instant::now()) {
            info!("user input during a Claude command: aborting it");
            self.control_abort(ErrorCode::UserTookOver, control::MSG_TOOK_OVER);
            self.control_seen = Some((std::time::Instant::now(), true));
        }
    }

    /// Stop the running Claude command, if any, and answer it with `code`.
    fn control_abort(&mut self, code: ErrorCode, message: &str) {
        let Some((mut job, reply)) = self.control_job.take() else {
            return;
        };
        let mut t = LiveTarget {
            tx: self.input_tx.as_ref(),
            running: self.running.load(Ordering::Relaxed),
            view_only: self.view_only,
            shared: &self.shared,
            shot: self.control_shot,
            host: &self.host,
        };
        let r = job.abort(code, message, &mut t);
        let _ = reply.send(r);
        self.arbiter.end_command();
    }

    /// Run the due part of the Claude command; reply once it is done.
    fn control_run(&mut self) {
        let now = std::time::Instant::now();
        let Some((job, _)) = self.control_job.as_mut() else {
            return;
        };
        let mut t = LiveTarget {
            tx: self.input_tx.as_ref(),
            running: self.running.load(Ordering::Relaxed),
            view_only: self.view_only,
            shared: &self.shared,
            shot: self.control_shot,
            host: &self.host,
        };
        match job.poll(now, &mut t) {
            control::Poll::Pending(at) => {
                if let Some((_, ticker)) = &self.control {
                    ticker.schedule(at);
                }
            }
            control::Poll::Done(r) => {
                if let Some((_, reply)) = self.control_job.take() {
                    let _ = reply.send(r);
                }
                self.arbiter.end_command();
                self.control_seen = Some((now, false));
            }
        }
    }

    /// A request from the control socket. Commands run one at a time on
    /// this thread; long ones are paced by the ticker.
    fn control_command(&mut self, cmd: Command, reply: std::sync::mpsc::Sender<Reply>) {
        let now = std::time::Instant::now();
        self.control_poll_user();
        if self.control.is_none() {
            let _ = reply.send(exec::err(
                ErrorCode::ControlDisabled,
                "control by Claude is switched off in the viewer",
            ));
            return;
        }
        if self.control_job.is_some() {
            let _ = reply.send(exec::session_err(
                "busy: the previous command is still running",
            ));
            return;
        }
        let idle_s = self
            .control_last_cmd
            .map_or(0, |t| now.saturating_duration_since(t).as_secs());
        self.control_last_cmd = Some(now);
        let has_session = self.input_tx.is_some() && self.running.load(Ordering::Relaxed);
        let (desk_w, desk_h) = {
            let t = LiveTarget {
                tx: self.input_tx.as_ref(),
                running: has_session,
                view_only: self.view_only,
                shared: &self.shared,
                shot: None,
                host: &self.host,
            };
            control::Target::desk_size(&t)
        };
        let r = match cmd {
            Command::Disconnect => exec::ok("control handed back"),
            _ if !has_session => exec::err(
                ErrorCode::NotConnected,
                "the viewer is not connected to a till; connect in the viewer first",
            ),
            Command::Status => Reply::Status {
                connected: true,
                target: Some(self.host.clone()),
                desk_w,
                desk_h,
                mode: "live".into(),
                idle_s,
            },
            _ if !self.arbiter.accepts_commands(now) => {
                exec::err(ErrorCode::UserTookOver, control::MSG_COOLDOWN)
            }
            Command::Screenshot { region, max_width } => self.control_screenshot(region, max_width),
            // Waiting sends nothing, so it works in view-only mode too.
            ref c if self.view_only && exec::sends_input(c) => exec::view_only(),
            cmd => {
                self.arbiter.begin_command();
                self.control_job = Some((control::Job::new(cmd), reply));
                self.control_seen = Some((now, false));
                self.control_run();
                return;
            }
        };
        let _ = reply.send(r);
    }

    /// Claude's screenshot of the viewer's framebuffer.
    fn control_screenshot(&mut self, region: Option<shot::Rect>, max_width: Option<u32>) -> Reply {
        let (rgba, w, h) = {
            let f = self.shared.lock().unwrap();
            match (u16::try_from(f.width), u16::try_from(f.height)) {
                (Ok(w), Ok(h)) if w > 0 && h > 0 && !f.rgba.is_empty() => (f.rgba.clone(), w, h),
                _ => return exec::no_image(),
            }
        };
        audit::log_agent(&self.host, &exec::screenshot_audit(region));
        let mw = max_width.unwrap_or(exec::DEFAULT_MAX_WIDTH);
        match shot::take(&rgba, w, h, region, mw) {
            Ok((png, space)) => {
                use base64::Engine as _;
                self.control_shot = Some(space);
                Reply::Image {
                    png_base64: base64::engine::general_purpose::STANDARD.encode(png),
                    desk_w: w,
                    desk_h: h,
                    scale: space.scale(),
                }
            }
            Err(e @ shot::ShotError::BadRegion) => exec::bad_args(e.to_string()),
            Err(e) => exec::session_err(format!("screenshot failed: {e}")),
        }
    }

    /// Window size in logical (UI-scale) pixels plus the scale itself, for
    /// hit-testing the overlays, which are drawn at logical size.
    fn logical_size(&self) -> (u32, u32, f64) {
        self.window
            .as_ref()
            .map(|w| {
                let s = w.inner_size();
                let k = ui_scale_of(w);
                (s.width / k, s.height / k, k as f64)
            })
            .unwrap_or((1, 1, 1.0))
    }

    /// Release every modifier on the remote. macOS sends no key-up for keys
    /// still held when the window loses focus, so Cmd+Tab (Cmd goes out as
    /// Ctrl) would leave Ctrl stuck down on the remote. Windows and X11
    /// synthesise those releases themselves. Extra releases are harmless.
    #[cfg(not(windows))]
    fn release_remote_modifiers(&self) {
        const MODS: [(u8, bool); 8] = [
            (0x1D, false), // Ctrl left
            (0x1D, true),  // Ctrl right
            (0x2A, false), // Shift left
            (0x36, false), // Shift right
            (0x38, false), // Alt left
            (0x38, true),  // Alt right (AltGr)
            (0x5B, true),  // Win left
            (0x5C, true),  // Win right
        ];
        let events = MODS
            .iter()
            .map(|&(sc, ext)| {
                let mut flags = KeyboardFlags::RELEASE;
                if ext {
                    flags |= KeyboardFlags::EXTENDED;
                }
                FastPathInputEvent::KeyboardEvent(flags, sc)
            })
            .collect();
        if let Some(tx) = &self.input_tx {
            let _ = tx.try_send(events);
        }
    }

    /// Signal the session thread to stop and disconnect gracefully: clear
    /// `running` and drop the input sender (unblocks run_active_session).
    fn begin_shutdown(&mut self) {
        self.release_held_buttons(u8::MAX);
        self.running.store(false, Ordering::Relaxed);
        self.input_tx = None;
    }

    /// Disconnect the current host and show the in-window host-prompt overlay
    /// so the operator can pick another target. If the operator cancels the
    /// overlay (Esc) the app exits — matches the old prompt-cancelled path.
    fn switch_host(&mut self, _event_loop: &ActiveEventLoop) {
        self.locks_sent = None;
        self.control_shot = None;
        self.begin_shutdown();
        // Let the old session confirm teardown before we reconnect — otherwise
        // reconnecting (especially to the same host) trips "existing session".
        let _ = self.done_rx.recv_timeout(std::time::Duration::from_secs(3));
        self.input_tx = None;
        #[cfg(windows)]
        winhook::set_tx(None);
        {
            let mut f = self.shared.lock().unwrap();
            *f = SharedFrame::default();
        }
        self.open_start();
        self.login_prompt = None;
        self.creds = None; // a new host asks for the password again
        self.keychain_save = false;
        self.creds_in_keychain = false;
        self.remember_choice = false;
        self.closed = None;
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Spawn a fresh session for `new_target`. Extracted from the old
    /// `switch_host` so the initial-prompt path (main() with no CLI target)
    /// can also kick off a session from inside the event loop.
    fn start_session(&mut self, new_target: String) {
        self.locks_sent = None;
        self.control_shot = None;
        recent::add(&new_target);
        self.host = new_target.clone();
        let shown = servers::display(&new_target);
        self.title = window_title(&new_target, &shown);
        if let Some(w) = &self.window {
            w.set_title(&self.title);
        }
        {
            let mut f = self.shared.lock().unwrap();
            *f = SharedFrame::default();
            f.status = t!("status.connecting_to", target => shown).to_string();
        }
        self.closed = None;
        self.connect_start = std::time::Instant::now();
        let (running, input_tx, done_rx) = spawn_session(
            new_target,
            self.width,
            self.height,
            self.shared.clone(),
            self.cursor.clone(),
            self.proxy.clone(),
            self.curtain.clone(),
            self.file_offer.clone(),
            self.monitors.clone(),
            self.cliprdr_req_counter.clone(),
            self.creds.clone(),
        );
        self.running = running;
        self.buttons_down = 0;
        #[cfg(windows)]
        winhook::set_tx(Some(input_tx.clone()));
        self.input_tx = Some(input_tx);
        self.done_rx = done_rx;
        self.host_prompt = None;
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Number of equal-width monitors in a combined All-Screens framebuffer,
    /// inferred from the aspect ratio assuming standard ~16:9 monitors side by
    /// side (e.g. 3840×1080 → 2). This is a heuristic for the common case; a
    /// future refinement could use real per-monitor rects from the protocol.
    /// Returns 1 (no split) for a single-monitor framebuffer.
    fn monitor_count(fb_w: u32, fb_h: u32) -> u32 {
        if fb_h == 0 {
            return 1;
        }
        let n = ((fb_w as f32 / fb_h as f32) / (16.0 / 9.0)).round() as i32;
        n.max(1) as u32
    }

    /// Source crop rect (in framebuffer pixels) for the current view: `All` =
    /// the whole framebuffer; `Screen(k)` = the k-th equal-width column. A stale
    /// `Screen(k)` (k ≥ count, e.g. after reconnecting to a single monitor)
    /// falls back to the full framebuffer.
    fn crop_rect(view: MonitorView, fb_w: u32, fb_h: u32) -> (u32, u32, u32, u32) {
        match view {
            MonitorView::Screen(k) => {
                let n = Self::monitor_count(fb_w, fb_h);
                if n > 1 && k < n {
                    let colw = fb_w / n;
                    (k * colw, 0, colw, fb_h)
                } else {
                    (0, 0, fb_w, fb_h)
                }
            }
            MonitorView::All => (0, 0, fb_w, fb_h),
        }
    }

    /// Toolbar label for the monitor switcher, or `None` when the target is
    /// single-monitor (button hidden). E.g. "Screen: All" / "Screen: 1".
    fn monitor_label(&self) -> Option<String> {
        let (fb_w, fb_h) = {
            let f = self.shared.lock().unwrap();
            (f.width, f.height)
        };
        Self::monitor_label_for(self.view, fb_w, fb_h)
    }

    /// As [`monitor_label`], but takes `view`/dims directly so callers that hold
    /// the frame lock or the surface `&mut` don't re-borrow `*self`.
    fn monitor_label_for(view: MonitorView, fb_w: u32, fb_h: u32) -> Option<String> {
        let n = Self::monitor_count(fb_w, fb_h);
        if n <= 1 {
            return None;
        }
        let which = match view {
            MonitorView::All => t!("monitor.all").to_string(),
            MonitorView::Screen(k) if k < n => (k + 1).to_string(),
            MonitorView::Screen(_) => t!("monitor.all").to_string(),
        };
        Some(t!("monitor.label", which => which).to_string())
    }

    /// Cycle the monitor view: All → Screen(0) → … → Screen(n-1) → All. No-op
    /// when the target is single-monitor.
    fn cycle_view(&mut self) {
        let (fb_w, fb_h) = {
            let f = self.shared.lock().unwrap();
            (f.width, f.height)
        };
        let n = Self::monitor_count(fb_w, fb_h);
        if n <= 1 {
            return;
        }
        self.view = match self.view {
            MonitorView::All => MonitorView::Screen(0),
            MonitorView::Screen(k) if k + 1 < n => MonitorView::Screen(k + 1),
            MonitorView::Screen(_) => MonitorView::All,
        };
        // Show a brief "Switching…" flash in the toolbar state line (~12 frames);
        // the new view's pixels are already in the framebuffer so the crop itself
        // is instant — this is purely visible feedback that the view changed.
        self.switch_flash = 12;
        info!(view = ?self.monitor_label(), "switched monitor view");
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Map a window-space cursor position to desktop coordinates, honouring the
    /// active monitor crop (so a click while viewing Screen 2 lands on the
    /// right-hand monitor's true desktop coordinate). `None` in the bars.
    fn map_cursor(&self, x: f64, y: f64) -> Option<(u16, u16)> {
        let (fb_w, fb_h) = {
            let f = self.shared.lock().unwrap();
            (f.width.max(1), f.height.max(1))
        };
        let crop = Self::crop_rect(self.view, fb_w, fb_h);
        let crop = (crop.0, crop.1, crop.2.max(1), crop.3.max(1));
        let (win_w, win_h) = self
            .window
            .as_ref()
            .map(|w| {
                let s = w.inner_size();
                (s.width.max(1), s.height.max(1))
            })
            .unwrap_or((1, 1));
        // The desktop keeps its aspect ratio inside the window below the toolbar
        // strip and shows only the crop sub-rect; points in the bars are outside.
        let rect = desktop_rect(crop, win_w, win_h, self.bar_h_phys().min(win_h));
        map_to_desktop(x, y, rect, crop, (fb_w, fb_h))
    }

    /// Run a toolbar button action.
    fn run_toolbar_action(&mut self, action: ToolbarAction, event_loop: &ActiveEventLoop) {
        match action {
            ToolbarAction::CtrlAltDel => self.send_ctrl_alt_del(),
            ToolbarAction::SendWin => self.send_win_key(),
            ToolbarAction::SendKeys => {
                self.type_prompt = Some(type_text::TypeTextOverlay::new());
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            ToolbarAction::SendFile => {
                // Spawn the picker off the UI thread — powershell startup + the
                // OpenFileDialog take 5-10 s during which .output() would freeze
                // winit long enough for Windows Watson to declare the viewer
                // "Not Responding" and kill it (application-hang 1002). The
                // file_offer Arc<Mutex<…>> the session polls already lives on
                // its own thread, so dropping the picked path in from any
                // background thread is safe.
                let file_offer = self.file_offer.clone();
                std::thread::spawn(move || {
                    if let Some(path) = pick_file() {
                        info!(file = %path.display(), "queued file to push to remote (paste there)");
                        *file_offer.lock().unwrap() = Some(path);
                    }
                });
            }
            ToolbarAction::ToggleCurtain => {
                let new = !self.curtain.load(Ordering::Relaxed);
                self.curtain.store(new, Ordering::Relaxed);
                info!(curtain = new, "toggled curtain (privacy screen)");
            }
            ToolbarAction::ToggleViewOnly => {
                self.view_only = !self.view_only;
                info!(view_only = self.view_only, "toggled view-only");
            }
            ToolbarAction::ToggleAgentControl => self.toggle_agent_control(),
            ToolbarAction::MonitorCycle => self.cycle_view(),
            ToolbarAction::ToggleRecord => {
                if self.recorder.is_some() {
                    let frames = self.recorder.as_ref().map(|r| r.frame_count()).unwrap_or(0);
                    self.recorder = None; // drop stops the writer + flushes
                    info!(frames, "recording stopped");
                } else {
                    self.recorder = record::Recorder::start();
                    match &self.recorder {
                        Some(r) => info!(dir = %r.dir().display(), "recording started"),
                        None => warn!("could not start recording (output dir)"),
                    }
                }
            }
            ToolbarAction::ToggleFullscreen => {
                self.fullscreen = !self.is_fullscreen();
                if let Some(w) = &self.window {
                    let fs = self
                        .fullscreen
                        .then_some(winit::window::Fullscreen::Borderless(None));
                    w.set_fullscreen(fs);
                }
            }
            ToolbarAction::Disconnect => {
                // Disconnect from the current host and pick another one (keeps the
                // app open). Closing the window (X) still exits entirely.
                self.switch_host(event_loop);
            }
            ToolbarAction::BugReport => {
                // Snapshot the current framebuffer under the lock, then write
                // the report dir outside it so PNG-encoding can't stall paint.
                let (w, h, rgba) = {
                    let f = self.shared.lock().unwrap();
                    (f.width, f.height, f.rgba.clone())
                };
                match report::generate(report::Snapshot {
                    target: &self.host,
                    width: w,
                    height: h,
                    rgba: &rgba,
                }) {
                    Ok(dir) => {
                        info!(dir = %dir.display(), "bug report bundle written");
                        self.shared.lock().unwrap().status =
                            format!("{} {}", t!("status.report_saved"), dir.display());
                        // Modal confirmation explains what was saved and asks
                        // whether to open the folder. MessageBoxW BLOCKS the
                        // calling thread until dismissed; spawn it off the UI
                        // thread so paint + remote input keep flowing while
                        // the dialog is up.
                        #[cfg(windows)]
                        {
                            let title = t!("report.dialog_title").to_string();
                            let body = t!("report.dialog_body").to_string();
                            let dir_for_dialog = dir.clone();
                            std::thread::spawn(move || {
                                if report::confirm_and_open(
                                    &dir_for_dialog,
                                    &title,
                                    &body,
                                ) {
                                    report::open_in_explorer(&dir_for_dialog);
                                }
                            });
                        }
                        #[cfg(not(windows))]
                        report::open_in_explorer(&dir);
                    }
                    Err(e) => warn!(error = %e, "could not write bug report"),
                }
            }
            ToolbarAction::About => {
                self.about_open = !self.about_open;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(&self.title)
            .with_window_icon(load_window_icon())
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        #[cfg(target_os = "macos")]
        set_dock_icon();
        let proxy = self.proxy.clone();
        self.native = mac_ui::NativeUi::install(
            &window,
            Box::new(move |ev| {
                let _ = proxy.send_event(UserEvent::Native(ev));
            }),
        );
        if self.native.is_some() {
            info!("native macOS chrome active (menu bar + title-bar toolbar)");
            // The first overlay was set up before the window existed; show
            // the matching native sheet instead.
            if self.login_prompt.take().is_some() {
                self.window = Some(window.clone());
                self.open_logon(self.host.clone(), initial_login_user(), None, true, None);
            } else if self.host_prompt.take().is_some() {
                self.window = Some(window.clone());
                self.open_start();
            }
        }
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        self.surface = Some(surface);
        // GPU renderer (wgpu) by default; the softbuffer CPU path stays as the
        // fallback. Force CPU with SCCM_RC_GPU=0 (jump-boxes / nested RDP / weak GPU);
        // on any GPU-init failure we fall back to CPU automatically.
        if std::env::var("SCCM_RC_GPU").as_deref() != Ok("0") {
            let sz = window.inner_size();
            match gpu::GpuRenderer::new(window.clone(), sz.width.max(1), sz.height.max(1)) {
                Ok(g) => {
                    info!(backend = g.backend(), "GPU renderer (wgpu) active");
                    g.set_error_handler();
                    self.gpu = Some(g);
                }
                Err(e) => warn!(error = %e, "GPU init failed — using softbuffer (CPU)"),
            }
        } else {
            info!("GPU disabled (SCCM_RC_GPU=0) — using softbuffer (CPU)");
        }
        self.window = Some(window);
    }

    /// Process is about to end — on macOS this is also Cmd+Q / "Quit" from the
    /// app menu, which terminates without a CloseRequested. Run the same
    /// graceful teardown so the SCCM host is released (no stale session).
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some((mut server, _ticker)) = self.control.take() {
            self.control_abort(ErrorCode::ControlDisabled, "the viewer is quitting");
            server.stop();
        }
        if self.running.load(Ordering::Relaxed) || self.input_tx.is_some() {
            self.begin_shutdown();
            let _ = self.done_rx.recv_timeout(std::time::Duration::from_secs(3));
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Frame => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::SessionActive(session) => {
                // A late report from a session the user already left must not
                // save the password typed for its successor.
                if !Arc::ptr_eq(&session, &self.running) {
                    return;
                }
                self.sync_locks(true);
                // The logon worked: now the password is worth keeping.
                if std::mem::take(&mut self.keychain_save) {
                    if let Some(c) = &self.creds {
                        keychain::save(&c.display_name(), &c.password);
                        self.creds_in_keychain = true;
                    }
                }
            }
            UserEvent::LoginFailed(reason, session) => {
                // Only the session the UI still runs may reopen the logon
                // overlay. One the user already left (host switch, Disconnect)
                // must neither tear down its successor nor cover the picker.
                if !Arc::ptr_eq(&session, &self.running) || !session.load(Ordering::Relaxed) {
                    info!(%reason, "ignoring logon failure from a stopped session");
                    return;
                }
                warn!(%reason, "logon failed; not retrying");
                self.locks_sent = None;
                // The session thread already stopped; make sure nothing restarts it.
                self.begin_shutdown();
                // Re-offer the user who just failed (the typo is usually in the
                // password), falling back to SCCM_USER / the last one used.
                let failed_user = self.creds.take().map(|c| c.display_name());
                // A saved password the server rejects is stale: drop it, so it
                // can't cause another failed logon (lockout) next time.
                if std::mem::take(&mut self.creds_in_keychain) {
                    if let Some(u) = &failed_user {
                        keychain::delete(u);
                    }
                }
                self.keychain_save = false;
                let remember = self.remember_choice;
                {
                    let mut f = self.shared.lock().unwrap();
                    *f = SharedFrame::default();
                }
                let msg = t!("login.rejected", reason => reason).to_string();
                self.open_logon(
                    self.host.clone(),
                    failed_user.or_else(initial_login_user),
                    Some(msg),
                    false,
                    Some(remember),
                );
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            // Only the sheet that is still open may answer: a late second
            // event (double click, Enter twice) must not close the sheet
            // that replaced it, or act on a session already started.
            UserEvent::Native(NativeEvent::Quit) => {
                if let Some(native) = self.native.as_ref() {
                    native.close_sheet();
                }
                // Same graceful teardown as closing the window.
                self.begin_shutdown();
                let _ = self.done_rx.recv_timeout(std::time::Duration::from_secs(3));
                event_loop.exit();
            }
            UserEvent::Native(NativeEvent::Start(outcome)) => {
                let Some(native) = self.native.as_ref().filter(|n| n.start_open()) else {
                    return;
                };
                native.close_sheet();
                self.on_host_outcome(outcome, event_loop);
            }
            UserEvent::Native(NativeEvent::Logon(outcome)) => {
                let Some(native) = self.native.as_ref().filter(|n| n.logon_open()) else {
                    return;
                };
                native.close_sheet();
                self.on_login_outcome(outcome);
            }
            UserEvent::Native(NativeEvent::Action(action)) => {
                self.run_toolbar_action(action, event_loop);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Control(cmd, reply) => self.control_command(cmd, reply),
            UserEvent::ControlTick => {
                self.control_poll_user();
                self.control_run();
            }
            UserEvent::Closed(reason) => {
                warn!(%reason, "session closed");
                self.closed = Some(reason);
                if let Some(w) = &self.window {
                    w.request_redraw(); // paint the closed banner once
                }
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // The user's input wins over a running Claude command.
        self.control_poll_user();
        // Wake again when the "Controlled by Claude" subtitle expires.
        if let (Some((_, ticker)), Some((at, _))) = (&self.control, self.control_seen) {
            if at.elapsed() < CONTROL_SHOWN {
                ticker.schedule(at + CONTROL_SHOWN);
            }
        }
        // Native macOS chrome mirrors the session state (cheap when unchanged).
        self.update_native_chrome();
        // UI-thread heartbeat: write a log line every ~5s. If a hang
        // happens, the last heartbeat timestamp is the moment the main
        // thread last responded — the window between it and the Watson
        // "Application Hang" event is exactly where the freeze started.
        if self.last_heartbeat.elapsed() >= std::time::Duration::from_secs(5) {
            self.last_heartbeat = std::time::Instant::now();
            tracing::info!(
                fps = self.fps,
                pending_paint = self.redraw_pending,
                "ui heartbeat"
            );
        }
        // Drain any pending type-text outcome (Enter → send, Esc → cancel).
        if let Some(outcome) = self
            .type_prompt
            .as_mut()
            .and_then(|p| p.take_outcome())
        {
            match outcome {
                type_text::TypeOutcome::Confirmed(events) => {
                    if let Some(tx) = &self.input_tx {
                        self.mark_user_input();
                        let _ = tx.try_send(events);
                    }
                }
                type_text::TypeOutcome::Cancelled => {}
            }
            self.type_prompt = None;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        // Drain any pending host-prompt outcome (Enter → connect, Esc → exit).
        if let Some(outcome) = self
            .host_prompt
            .as_mut()
            .and_then(|p| p.take_outcome())
        {
            if !self.on_host_outcome(outcome, event_loop) {
                return;
            }
        }
        // Drain any pending logon outcome (Enter → connect, Esc → host picker).
        if let Some(outcome) = self.login_prompt.as_mut().and_then(|p| p.take_outcome()) {
            self.login_prompt = None;
            self.on_login_outcome(outcome);
        }
        // Ctrl+V → UAC fallback: if V has been released AND at least
        // 100 ms elapsed since key-down (floor so a fast tap still gives
        // cliprdr fair chance), check the counter. Unchanged = paste
        // was silently dropped (Secure Desktop / UAC) → type the
        // snapshotted clipboard as scancodes. Ticked = a normal
        // cliprdr paste happened → drop the pending, do nothing.
        // Also age out after 1500 ms in case V-release was missed
        // (e.g. focus stolen before key-up arrived).
        if let Some(p) = self.paste_pending.as_ref() {
            const FLOOR: std::time::Duration = std::time::Duration::from_millis(100);
            const AGE_OUT: std::time::Duration = std::time::Duration::from_millis(1500);
            let elapsed = p.started.elapsed();
            let ready = (p.v_released && elapsed >= FLOOR) || elapsed >= AGE_OUT;
            if ready {
                let cur = self
                    .cliprdr_req_counter
                    .load(std::sync::atomic::Ordering::Relaxed);
                if cur == p.baseline {
                    let events = type_text::encode_string(&p.clip);
                    if !events.is_empty() {
                        if let Some(tx) = &self.input_tx {
                            self.mark_user_input();
                            let _ = tx.try_send(events);
                        }
                        info!(
                            len = p.clip.chars().count(),
                            elapsed_ms = elapsed.as_millis() as u64,
                            "Ctrl+V fallback — no cliprdr ack, typed clipboard as scancodes"
                        );
                    }
                }
                self.paste_pending = None;
            } else {
                // Pending but not yet ready — wake at the floor to fire
                // the decision without stalling for user input.
                let remaining = FLOOR
                    .checked_sub(elapsed)
                    .unwrap_or(std::time::Duration::from_millis(10));
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    std::time::Instant::now() + remaining,
                ));
            }
        }
        // While either overlay is up, tick the caret blink by scheduling a
        // repaint every ~500ms. Cheaper than the general connect-spinner
        // branch below (60ms) and gives the caret a proper on/off cadence.
        if self.host_prompt.is_some() || self.type_prompt.is_some() || self.login_prompt.is_some() {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + std::time::Duration::from_millis(500),
            ));
            return;
        }
        // While still connecting (nothing painted yet), keep the spinner animating
        // by redrawing ~16x/s. Once the desktop paints, go back to event-driven.
        let painted = {
            let f = self.shared.lock().unwrap();
            f.width != 0 && !f.rgba.is_empty()
        };
        if !painted && self.closed.is_none() && !self.native_sheet_open() {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + std::time::Duration::from_millis(60),
            ));
        } else if self.redraw_pending {
            // A repaint was deferred by the 60 fps cap — paint it once the frame
            // interval has elapsed (the trailing paint), so the latest update lands.
            let next = self.last_paint + std::time::Duration::from_millis(16);
            if std::time::Instant::now() >= next {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::Wait);
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(next));
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // Stop the session and WAIT for its graceful disconnect (the MCS
                // Disconnect-Provider-Ultimatum, signalled via done_rx) before
                // exiting — otherwise the SCCM host keeps the session and the next
                // connect trips ERROR_EXISTING_SESSION. Same teardown contract as
                // switch_host; bounded so a wedged session thread can't hang exit.
                self.begin_shutdown();
                let _ = self.done_rx.recv_timeout(std::time::Duration::from_secs(3));
                event_loop.exit();
            }
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_win = (position.x, position.y);
                // Type-text overlay: swallow move events so we don't sneak
                // pointer moves onto the remote behind the modal.
                if self.type_prompt.is_some() {
                    if let Some(w) = &self.window {
                        w.set_cursor_visible(true);
                        w.request_redraw();
                    }
                    return;
                }
                // Logon overlay: the pointer only focuses fields on click.
                if self.login_prompt.is_some() {
                    return;
                }
                // Host-prompt overlay owns the pointer while it's up: hover
                // moves the recents-selection, and nothing goes to the remote.
                if self.host_prompt.is_some() {
                    let (w_w, w_h, k) = self.logical_size();
                    if let Some(prompt) = self.host_prompt.as_mut() {
                        prompt.on_mouse_move(position.x / k, position.y / k, w_w, w_h);
                    }
                    if let Some(w) = &self.window {
                        w.set_cursor_visible(true);
                        w.request_redraw();
                    }
                    return;
                }
                // Over the toolbar: keep the OS cursor for clicking buttons and
                // don't forward the move to the remote.
                if position.y < self.bar_h_phys() as f64 {
                    if let Some(w) = &self.window {
                        w.set_cursor_visible(true);
                        w.request_redraw();
                    }
                    return;
                }
                let Some((x, y)) = self.map_cursor(position.x, position.y) else {
                    // In the letterbox bars: nothing to forward.
                    if let Some(w) = &self.window {
                        w.set_cursor_visible(true);
                        w.request_redraw();
                    }
                    return;
                };
                self.last_cursor = (x, y);
                // Redraw so the client-side cursor follows the mouse instantly
                // (winit coalesces these into one redraw per frame).
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                // Coalesce mouse moves to ~60/s. winit emits a CursorMoved per
                // movement (hundreds/s); sending each as a sealed input frame
                // saturates the session thread and starves graphics updates.
                if self.last_move.elapsed() >= std::time::Duration::from_millis(15) {
                    self.last_move = std::time::Instant::now();
                    self.send_input(FastPathInputEvent::MouseEvent(MousePdu {
                        flags: PointerFlags::MOVE,
                        number_of_wheel_rotation_units: 0,
                        x_position: x,
                        y_position: y,
                    }));
                }
            }
            WindowEvent::CursorEntered { .. } => {
                self.cursor_inside = true;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_inside = false;
                if let Some(w) = &self.window {
                    w.set_cursor_visible(true);
                    w.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // A release for a button we sent as down always goes out, wherever
                // the pointer is now (bar, toolbar, overlay), so the remote never
                // keeps a button held.
                let bit = button_bit(button);
                if state == ElementState::Released && self.buttons_down & bit != 0 {
                    let at = self
                        .map_cursor(self.mouse_win.0, self.mouse_win.1)
                        .unwrap_or(self.last_cursor);
                    self.last_cursor = at;
                    self.release_held_buttons(bit);
                    return;
                }
                // While the About overlay is open, a left click anywhere closes it
                // and is neither hit-tested against the toolbar nor forwarded.
                if self.about_open {
                    if state == ElementState::Pressed && button == MouseButton::Left {
                        self.about_open = false;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                // Modal overlays own the pointer; clicks are hit-tested in
                // logical (UI-scale) pixels, the size they are drawn at.
                //  * type-text: left-click on Send/Cancel confirms/dismisses;
                //  * logon: left-click focuses the user / password field;
                //  * host prompt: left-click on a recent row confirms it.
                if self.type_prompt.is_some()
                    || self.login_prompt.is_some()
                    || self.host_prompt.is_some()
                {
                    if state == ElementState::Pressed && button == MouseButton::Left {
                        let (w_w, w_h, k) = self.logical_size();
                        let (x, y) = (self.mouse_win.0 / k, self.mouse_win.1 / k);
                        if let Some(prompt) = self.type_prompt.as_mut() {
                            prompt.on_click(x, y, w_w, w_h);
                        } else if let Some(prompt) = self.login_prompt.as_mut() {
                            prompt.on_click(x, y, w_w, w_h);
                        } else if let Some(prompt) = self.host_prompt.as_mut() {
                            prompt.on_click(x, y, w_w, w_h);
                        }
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                // Clicks on the toolbar strip are handled locally, not forwarded.
                if self.mouse_win.1 < self.bar_h_phys() as f64 {
                    if state == ElementState::Pressed && button == MouseButton::Left {
                        let (win_w, k) = self
                            .window
                            .as_ref()
                            .map(|w| (w.inner_size().width.max(1), ui_scale_of(w)))
                            .unwrap_or((1, 1));
                        let monitor = self.monitor_label();
                        if let Some(action) = toolbar::hit_test(
                            self.mouse_win.0 / k as f64,
                            self.mouse_win.1 / k as f64,
                            (win_w / k).max(1),
                            self.font.as_ref(),
                            monitor.as_deref(),
                        ) {
                            self.run_toolbar_action(action, event_loop);
                        }
                    }
                    return;
                }
                let down = state == ElementState::Pressed;
                // Only presses reach here; they start on the desktop rect, at the
                // freshly mapped position.
                let Some((x, y)) = self.map_cursor(self.mouse_win.0, self.mouse_win.1) else {
                    return;
                };
                if !down || !self.can_send_input() {
                    return;
                }
                self.last_cursor = (x, y);
                self.buttons_down |= bit;
                let mut flags = match button {
                    MouseButton::Left => PointerFlags::LEFT_BUTTON,
                    MouseButton::Right => PointerFlags::RIGHT_BUTTON,
                    _ => PointerFlags::empty(),
                };
                if down {
                    flags |= PointerFlags::DOWN;
                }
                self.send_input(FastPathInputEvent::MouseEvent(MousePdu {
                    flags,
                    number_of_wheel_rotation_units: 0,
                    x_position: x,
                    y_position: y,
                }));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Wheel over a letterbox bar (or the toolbar) is not forwarded.
                if self.map_cursor(self.mouse_win.0, self.mouse_win.1).is_none() {
                    return;
                }
                // MS-RDPBCGR: a wheel event needs PTRFLAGS_VERTICAL_WHEEL
                // (0x0200) or PTRFLAGS_HORIZONTAL_WHEEL (0x0400), otherwise
                // the server ignores the rotation units field. IronRDP adds
                // the WHEEL_NEGATIVE bit automatically based on the sign of
                // number_of_wheel_rotation_units — but it does NOT set the
                // wheel-direction flag, that's on us. Without this fix
                // scrolling in the remote is a silent no-op.
                //
                // The rotation-units field is 8-bit two's complement on the
                // wire (see ironrdp-pdu Encode: `as u8`), so anything
                // outside -127..127 wraps around and points the wheel the
                // wrong way. Clamp defensively — a single click on a
                // Windows mouse is 120, a fast trackpad flick can produce
                // multiples.
                let (units, horizontal) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        if x.abs() > y.abs() {
                            ((x * 120.0) as i32, true)
                        } else {
                            ((y * 120.0) as i32, false)
                        }
                    }
                    MouseScrollDelta::PixelDelta(p) => {
                        if p.x.abs() > p.y.abs() {
                            (p.x as i32, true)
                        } else {
                            (p.y as i32, false)
                        }
                    }
                };
                if units == 0 {
                    return;
                }
                let units = units.clamp(-127, 127) as i16;
                let flags = if horizontal {
                    PointerFlags::HORIZONTAL_WHEEL
                } else {
                    PointerFlags::VERTICAL_WHEEL
                };
                self.send_input(FastPathInputEvent::MouseEvent(MousePdu {
                    flags,
                    number_of_wheel_rotation_units: units,
                    x_position: 0,
                    y_position: 0,
                }));
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::Focused(has_focus) => {
                // Gate the LL keyboard hook so we only steal Win-key / Ctrl+Esc
                // while the viewer is the foreground app. Losing focus releases
                // the OS shortcuts back to whichever window the user switched to.
                #[cfg(windows)]
                winhook::set_active(has_focus);
                if !has_focus {
                    self.release_held_buttons(u8::MAX);
                }
                #[cfg(not(windows))]
                if !has_focus {
                    self.release_remote_modifiers();
                } else {
                    // Caps Lock may have been toggled in another app.
                    self.sync_locks(true);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // While the About overlay is open, swallow all keyboard input; Esc
                // closes it. Nothing reaches the remote session.
                if self.about_open {
                    if event.state == ElementState::Pressed
                        && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Escape))
                    {
                        self.about_open = false;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                // Type-text overlay owns all keyboard input while up (checked
                // before host_prompt so the two never fight if both were
                // somehow open — type_prompt can only appear over a live
                // session, host_prompt only when disconnected).
                if let Some(prompt) = self.type_prompt.as_mut() {
                    if event.state == ElementState::Pressed {
                        // Ctrl+V / Ctrl+Insert → paste OS clipboard. Critical
                        // for the intended flow: copy password from password
                        // manager → paste here → Enter → scancodes to UAC
                        // (which Windows blocks direct cliprdr paste into).
                        // Without this, the Type Text field is unusable for
                        // the very case it was designed for. (Cmd+V on macOS.)
                        if edit_mod(self.modifiers)
                            && matches!(
                                event.physical_key,
                                PhysicalKey::Code(KeyCode::KeyV)
                                    | PhysicalKey::Code(KeyCode::Insert)
                            )
                        {
                            if let Some(clip) = read_clipboard_text() {
                                prompt.on_text(&clip);
                            }
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        // Ctrl+A / Ctrl+Delete → clear the field (see host_prompt).
                        if edit_mod(self.modifiers)
                            && matches!(
                                event.physical_key,
                                PhysicalKey::Code(KeyCode::KeyA)
                                    | PhysicalKey::Code(KeyCode::Delete)
                            )
                        {
                            prompt.on_clear();
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        match event.physical_key {
                            PhysicalKey::Code(KeyCode::Escape) => prompt.on_esc(),
                            PhysicalKey::Code(KeyCode::Enter)
                            | PhysicalKey::Code(KeyCode::NumpadEnter) => prompt.on_enter(),
                            PhysicalKey::Code(KeyCode::Backspace)
                            | PhysicalKey::Code(KeyCode::Delete) => prompt.on_backspace(),
                            _ => {
                                if let Some(text) = event.text.as_deref() {
                                    if !text.is_empty() {
                                        prompt.on_text(text);
                                    }
                                }
                            }
                        }
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                // Logon overlay owns all keyboard input while it's up.
                if let Some(prompt) = self.login_prompt.as_mut() {
                    // A held Space must not flicker the checkbox on every repeat.
                    let repeat_blocked = event.repeat && !prompt.key_repeat_ok();
                    if event.state == ElementState::Pressed && !repeat_blocked {
                        let m = self.modifiers;
                        match event.physical_key {
                            PhysicalKey::Code(KeyCode::KeyV)
                            | PhysicalKey::Code(KeyCode::Insert)
                                if edit_mod(m) =>
                            {
                                if let Some(clip) = read_clipboard_text() {
                                    prompt.on_text(&clip);
                                }
                            }
                            PhysicalKey::Code(KeyCode::KeyA)
                            | PhysicalKey::Code(KeyCode::Delete)
                                if edit_mod(m) =>
                            {
                                prompt.on_clear()
                            }
                            PhysicalKey::Code(KeyCode::Escape) => prompt.on_esc(),
                            PhysicalKey::Code(KeyCode::Tab)
                            | PhysicalKey::Code(KeyCode::ArrowUp)
                            | PhysicalKey::Code(KeyCode::ArrowDown) => prompt.on_tab(),
                            PhysicalKey::Code(KeyCode::Enter)
                            | PhysicalKey::Code(KeyCode::NumpadEnter) => prompt.on_enter(),
                            PhysicalKey::Code(KeyCode::Backspace)
                            | PhysicalKey::Code(KeyCode::Delete) => prompt.on_backspace(),
                            _ if edit_mod(m) => {} // swallow other shortcuts (Cmd+Q is the OS's)
                            _ => {
                                if let Some(text) = event.text.as_deref() {
                                    if !text.is_empty() {
                                        prompt.on_text(text);
                                    }
                                }
                            }
                        }
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                // Host-prompt overlay owns all keyboard input while it's up.
                if let Some(prompt) = self.host_prompt.as_mut() {
                    if event.state == ElementState::Pressed {
                        // Ctrl+V / Ctrl+Insert → paste the OS clipboard into the
                        // connect field. The spartan v1 keyboard model only
                        // appended printable chars, so a paste otherwise leaked
                        // through as a literal "v". on_text() strips control
                        // chars, so any CR/LF from a copied line is dropped.
                        // (Cmd+V on macOS.)
                        if edit_mod(self.modifiers)
                            && matches!(
                                event.physical_key,
                                PhysicalKey::Code(KeyCode::KeyV)
                                    | PhysicalKey::Code(KeyCode::Insert)
                            )
                        {
                            if let Some(clip) = read_clipboard_text() {
                                prompt.on_text(&clip);
                            }
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        // Ctrl+A → clear the field. We don't render a visual
                        // selection, so "select all + type" and "clear" look
                        // the same to the operator.
                        // Cmd/Ctrl+S → save / rename the highlighted target.
                        // Cmd/Ctrl+Backspace / Delete → delete the highlighted
                        // saved target (twice), else clear the field. Key
                        // repeat is ignored so holding it can't confirm.
                        if edit_mod(self.modifiers) {
                            let done = match event.physical_key {
                                PhysicalKey::Code(KeyCode::KeyA) => {
                                    prompt.on_clear();
                                    true
                                }
                                PhysicalKey::Code(KeyCode::KeyS) => {
                                    if !event.repeat {
                                        prompt.on_save();
                                    }
                                    true
                                }
                                PhysicalKey::Code(KeyCode::Backspace)
                                | PhysicalKey::Code(KeyCode::Delete) => {
                                    if !event.repeat {
                                        prompt.on_delete();
                                    }
                                    true
                                }
                                _ => false,
                            };
                            if done {
                                if let Some(w) = &self.window {
                                    w.request_redraw();
                                }
                                return;
                            }
                        }
                        let handled = match event.physical_key {
                            PhysicalKey::Code(KeyCode::Escape) => {
                                prompt.on_esc();
                                true
                            }
                            PhysicalKey::Code(KeyCode::Enter)
                            | PhysicalKey::Code(KeyCode::NumpadEnter) => {
                                prompt.on_enter();
                                true
                            }
                            // Delete without Ctrl behaves like Backspace here
                            // (single-line, no cursor position) — matches the
                            // "delete forward = delete at end" intuition on a
                            // caret-at-end text field.
                            PhysicalKey::Code(KeyCode::Backspace)
                            | PhysicalKey::Code(KeyCode::Delete) => {
                                prompt.on_backspace();
                                true
                            }
                            PhysicalKey::Code(KeyCode::ArrowUp) => {
                                prompt.on_arrow_up();
                                true
                            }
                            PhysicalKey::Code(KeyCode::ArrowDown) => {
                                prompt.on_arrow_down();
                                true
                            }
                            _ => {
                                // Printable text: winit puts the composed
                                // char(s) in `event.text` for us.
                                if let Some(text) = event.text.as_deref() {
                                    if !text.is_empty() {
                                        prompt.on_text(text);
                                    }
                                }
                                false
                            }
                        };
                        let _ = handled;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                // Ctrl+Alt+End → send Ctrl+Alt+Del (SAS) to the remote, like
                // CmRcViewer (Ctrl+Alt+Del itself is swallowed by the local OS).
                if event.state == ElementState::Pressed
                    && self.modifiers.control_key()
                    && self.modifiers.alt_key()
                    && matches!(event.physical_key, PhysicalKey::Code(KeyCode::End))
                {
                    self.send_ctrl_alt_del();
                    return;
                }
                // Ctrl+Shift+V → "paste as scancodes": read the local
                // clipboard and type it into the remote via FastPath keyboard
                // events. Bypasses the RDP clipboard channel entirely, so it
                // works in a UAC prompt / Secure Desktop where MS-RDPECLIP is
                // deliberately blocked by Windows. Convention matches
                // browser/terminal "paste as plain text". US-layout ASCII
                // only; non-ASCII chars are silently dropped by
                // `type_text::encode_string`.
                if event.state == ElementState::Pressed
                    && self.modifiers.control_key()
                    && self.modifiers.shift_key()
                    && matches!(event.physical_key, PhysicalKey::Code(KeyCode::KeyV))
                {
                    if !self.view_only {
                        if let Some(clip) = read_clipboard_text() {
                            let events = type_text::encode_string(&clip);
                            if !events.is_empty() {
                                if let Some(tx) = &self.input_tx {
                                    self.mark_user_input();
                                    let _ = tx.try_send(events);
                                }
                                info!(
                                    len = clip.chars().count(),
                                    "Ctrl+Shift+V — typed clipboard into remote via scancodes"
                                );
                            }
                        }
                    }
                    return;
                }
                // Ctrl+Esc → send the Windows key to the remote. CmRcViewer only
                // passes Win-key through in fullscreen; the LL OS hook in
                // windowed mode is heavyweight, so we expose the canonical PS/2
                // alternative chord instead. Swallow both press AND release so
                // a stray Esc-up doesn't leak to the remote.
                if self.modifiers.control_key()
                    && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Escape))
                {
                    if event.state == ElementState::Pressed {
                        self.send_win_key();
                    }
                    return;
                }
                // Ctrl+Tab → cycle the monitor view (All / Screen N) of a
                // multi-monitor target. Handled locally, never forwarded.
                if self.modifiers.control_key()
                    && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Tab))
                {
                    if event.state == ElementState::Pressed {
                        self.cycle_view();
                    }
                    return;
                }
                // Plain Ctrl+V (no Shift, no Alt) → start a UAC-fallback
                // watch: snapshot the local clipboard and the current
                // cliprdr request counter. The scancode below still forwards
                // Ctrl+V to the remote so a normal cliprdr paste can happen;
                // if that paste never triggers a FormatDataRequest, the
                // about_to_wait handler types the clipboard as scancodes
                // when V is released (+ a 100 ms floor). Non-fatal if the
                // clipboard is empty — we just don't stage a fallback.
                // `remote_ctrl`: on macOS Cmd+V reaches the remote as Ctrl+V.
                if event.state == ElementState::Pressed
                    && remote_ctrl(self.modifiers)
                    && !self.modifiers.shift_key()
                    && !self.modifiers.alt_key()
                    && matches!(event.physical_key, PhysicalKey::Code(KeyCode::KeyV))
                    && !self.view_only
                {
                    if let Some(clip) = read_clipboard_text() {
                        if !clip.is_empty() {
                            self.paste_pending = Some(PastePending {
                                started: std::time::Instant::now(),
                                baseline: self
                                    .cliprdr_req_counter
                                    .load(std::sync::atomic::Ordering::Relaxed),
                                clip,
                                v_released: false,
                            });
                            // Wake about_to_wait quickly so it can arm the
                            // 100 ms floor without waiting for the next
                            // natural event.
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                        }
                    }
                    // Fall through — normal scancode forward still happens.
                }
                // V released → mark the pending as "released". Actual
                // fallback firing lives in about_to_wait.
                if event.state == ElementState::Released
                    && matches!(event.physical_key, PhysicalKey::Code(KeyCode::KeyV))
                {
                    if let Some(p) = self.paste_pending.as_mut() {
                        p.v_released = true;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    // Fall through — normal scancode forward still happens.
                }
                // Caps Lock toggles produce no event on macOS, so check the
                // local state before each key press reaches the remote.
                if event.state == ElementState::Pressed {
                    self.sync_locks(false);
                }
                if let Some((code, extended)) = remote_scancode(&event.physical_key) {
                    let mut flags = KeyboardFlags::empty();
                    if event.state == ElementState::Released {
                        flags |= KeyboardFlags::RELEASE;
                    }
                    if extended {
                        flags |= KeyboardFlags::EXTENDED;
                    }
                    self.send_input(FastPathInputEvent::KeyboardEvent(flags, code));
                }
            }
            WindowEvent::RedrawRequested => {
                // Cap repaints to ~60 fps. Excess requests just set redraw_pending;
                // about_to_wait schedules a trailing paint so the latest frame lands.
                const FRAME: std::time::Duration = std::time::Duration::from_millis(16);
                if self.last_paint.elapsed() >= FRAME {
                    self.redraw_pending = false;
                    self.last_paint = std::time::Instant::now();
                    self.render();
                } else {
                    self.redraw_pending = true;
                }
            }
            _ => {}
        }
    }
}

impl App {
    /// GPU render path (wgpu). Draws the desktop framebuffer as a quad below the
    /// toolbar, and the toolbar (or the connect/closed splash) as an overlay quad
    /// rasterised by the existing CPU code into a small buffer. Used by default;
    /// SCCM_RC_GPU=0 forces the softbuffer CPU fallback.
    fn render_gpu(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        let (win_w, win_h) = (size.width.max(1), size.height.max(1));
        let k = ui_scale_of(&window);
        let bar_h = (self.bar_h_logical() * k).min(win_h);
        // A native sheet is open: plain background, no spinner or text.
        let quiet = self.native_sheet_open();
        let mut frame = self.shared.lock().unwrap();
        let connected = frame.width != 0 && frame.height != 0 && !frame.rgba.is_empty();
        let (fb_w, fb_h) = (frame.width, frame.height);
        // Active monitor crop (RRCV-20): source UV sub-rect + toolbar label. The
        // `_for` variants take the dims we already hold so they don't re-lock.
        let monitor_label = Self::monitor_label_for(self.view, fb_w, fb_h);
        let crop = if connected && fb_w > 0 && fb_h > 0 {
            Self::crop_rect(self.view, fb_w, fb_h)
        } else {
            (0, 0, 1, 1)
        };
        let desk_rect = desktop_rect(crop, win_w, win_h, bar_h);
        let desktop_uv = if connected && fb_w > 0 && fb_h > 0 {
            let (cx, cy, cw, ch) = crop;
            [
                cx as f32 / fb_w as f32,
                cy as f32 / fb_h as f32,
                (cx + cw) as f32 / fb_w as f32,
                (cy + ch) as f32 / fb_h as f32,
            ]
        } else {
            [0.0, 0.0, 1.0, 1.0]
        };
        // Dirty-region upload: full on resync/size-change, else just the changed
        // row band, else nothing (the GPU texture persists across frames).
        let desktop_upload = if frame.full_resync {
            frame.full_resync = false;
            frame.dirty = None;
            gpu::DesktopUpload::Full
        } else if let Some(d) = frame.dirty.take() {
            gpu::DesktopUpload::Rows(d.top as u32, d.bottom as u32)
        } else {
            gpu::DesktopUpload::Skip
        };
        let bytes_per_sec = frame.bytes_per_sec;
        let status_msg = frame.status.clone();
        let security = frame.security.clone();
        let secure = frame.secure;
        let encrypted = frame.encrypted;
        if let Some(rec) = self.recorder.as_mut() {
            rec.maybe_capture(frame.width, frame.height, &frame.rgba);
        }

        // Rasterise the overlay into a CPU u32 buffer with the existing drawing
        // code: the toolbar strip when connected, else the full-window splash.
        let show_full_overlay = self.about_open
            || self.host_prompt.is_some()
            || self.login_prompt.is_some()
            || self.type_prompt.is_some()
            || !connected;
        // Overlay is drawn and uploaded at logical size (`ov_w × ov_h`); the GPU
        // stretches it over `dest` with nearest sampling (crisp pixel-doubling).
        let (lw, lh) = ((win_w / k).max(1), (win_h / k).max(1));
        // With the native macOS toolbar (`bar_h == 0`) there is no strip:
        // no overlay is drawn or uploaded while the desktop shows.
        let (ov_w, ov_h, dest) = if show_full_overlay {
            (lw, lh, gpu::OverlayDest::Full)
        } else if bar_h == 0 {
            (lw, 0, gpu::OverlayDest::TopStrip(0))
        } else {
            (
                lw,
                toolbar::TOOLBAR_H.min(lh),
                gpu::OverlayDest::TopStrip(bar_h),
            )
        };
        let mut ov = vec![0u32; (ov_w * ov_h) as usize];
        if let Some(prompt) = self.login_prompt.as_ref() {
            login_prompt::draw(&mut ov, ov_w, ov_h, self.font.as_ref(), prompt);
        } else if let Some(prompt) = self.host_prompt.as_ref() {
            host_prompt::draw(&mut ov, ov_w, ov_h, self.font.as_ref(), prompt);
        } else if let Some(prompt) = self.type_prompt.as_ref() {
            type_text::draw(&mut ov, ov_w, ov_h, self.font.as_ref(), prompt);
        } else if self.about_open {
            draw_about(&mut ov, ov_w, ov_h, self.font.as_ref());
        } else if connected && ov_h == 0 {
            // Native macOS toolbar: nothing to draw over the desktop.
        } else if connected {
            let mode = if self.view_only {
                t!("mode.view_only")
            } else {
                t!("mode.control")
            }
            .to_string();
            let state = if self.switch_flash > 0 {
                self.switch_flash -= 1;
                // Keep animating the flash down even if no server frames arrive.
                window.request_redraw();
                t!("monitor.switching").to_string()
            } else {
                t!("status.connected").to_string()
            };
            let status = toolbar::Status {
                host: &self.host,
                mode: &mode,
                state: &state,
                connected,
                fps: self.fps,
                bytes_per_sec,
                recording: self.recorder.is_some(),
                curtain: self.curtain.load(Ordering::Relaxed),
                security: &security,
                secure,
                view_only: self.view_only,
                encrypted,
                monitor: monitor_label.as_deref(),
            };
            toolbar::draw(&mut ov, ov_w, ov_h, &status, self.font.as_ref());
        } else {
            let fill = if self.closed.is_some() {
                0x0040_0000
            } else {
                0x0020_2020
            };
            for px in ov.iter_mut() {
                *px = fill;
            }
            let msg = if let Some(reason) = &self.closed {
                t!("status.disconnected_reason", reason => reason).to_string()
            } else if status_msg.is_empty() {
                t!("status.connecting").to_string()
            } else {
                status_msg
            };
            let (cx, cy) = (ov_w / 2, ov_h / 2);
            if self.closed.is_none() && !quiet {
                draw_spinner(
                    &mut ov,
                    ov_w,
                    ov_h,
                    cx,
                    cy.saturating_sub(72),
                    self.connect_start.elapsed(),
                );
            }
            if quiet {
            } else if let Some(f) = self.font.as_ref() {
                f.draw_centered(
                    &mut ov,
                    ov_w,
                    ov_h,
                    cy as f32 + 4.0,
                    &self.host,
                    0x00FF_FFFF,
                    34.0,
                );
                f.draw_centered(
                    &mut ov,
                    ov_w,
                    ov_h,
                    cy as f32 + 38.0,
                    &msg,
                    0x00B0_C0D0,
                    19.0,
                );
            } else {
                toolbar::draw_text_centered(
                    &mut ov,
                    ov_w,
                    ov_h,
                    cy.saturating_sub(28),
                    &self.host,
                    0x00FF_FFFF,
                    3,
                );
                toolbar::draw_text_centered(&mut ov, ov_w, ov_h, cy + 8, &msg, 0x00B0_C0D0, 2);
            }
        }
        // Pack 0x00RRGGBB -> RGBA bytes (opaque).
        let mut ov_rgba = vec![0u8; (ov_w * ov_h * 4) as usize];
        for (i, &px) in ov.iter().enumerate() {
            let o = i * 4;
            ov_rgba[o] = ((px >> 16) & 0xff) as u8;
            ov_rgba[o + 1] = ((px >> 8) & 0xff) as u8;
            ov_rgba[o + 2] = (px & 0xff) as u8;
            ov_rgba[o + 3] = 0xff;
        }

        // Client cursor as a GPU quad at the live mouse position — the clean #87
        // fix (no CPU cursor-box-fill). Same conditions as the softbuffer path.
        let cursor = {
            let cur = self.cursor.lock().unwrap();
            let show = connected
                && cur.draw
                && !cur.rgba.is_empty()
                && self.cursor_inside
                && mouse_in_rect(self.mouse_win, desk_rect);
            window.set_cursor_visible(!show);
            if show {
                let dx = self.mouse_win.0 as i32 - cur.hotspot_x as i32;
                let dy = self.mouse_win.1 as i32 - cur.hotspot_y as i32;
                Some((
                    cur.width as u32,
                    cur.height as u32,
                    cur.rgba.clone(),
                    dx,
                    dy,
                ))
            } else {
                None
            }
        };

        let desktop = if connected {
            Some((fb_w, fb_h, frame.rgba.as_slice(), desktop_upload))
        } else {
            None
        };
        let cursor_ref = cursor
            .as_ref()
            .map(|(w, h, r, x, y)| (*w, *h, r.as_slice(), *x, *y));
        let dump = if connected {
            self.gpu_dump.take()
        } else {
            None
        };
        let gpu = self.gpu.as_mut().unwrap();
        gpu.render(
            win_w,
            win_h,
            desk_rect,
            desktop,
            (ov_h > 0).then_some((ov_w, ov_h, ov_rgba.as_slice(), dest)),
            cursor_ref,
            desktop_uv,
        );
        if let Some(p) = dump {
            match gpu.dump_png(&p) {
                Ok(()) => info!(path = %p, "GPU frame dumped"),
                Err(e) => warn!(error = %e, "GPU dump failed"),
            }
        }
    }

    fn render(&mut self) {
        if self.gpu.is_some() {
            self.render_gpu();
            return;
        }
        // Capture the monitor view before borrowing the surface `&mut self.surface`
        // (which then precludes any `&self` method call for the rest of render).
        let view = self.view;
        let bar_logical = self.bar_h_logical();
        // A native sheet is open: plain background, no spinner or text.
        let quiet = self.native_sheet_open();
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (win_w, win_h) = (size.width.max(1), size.height.max(1));
        let (Some(nw), Some(nh)) = (NonZeroU32::new(win_w), NonZeroU32::new(win_h)) else {
            return;
        };
        if surface.resize(nw, nh).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        let rstart = std::time::Instant::now();

        // Host-prompt overlay owns the window while the user picks a target
        // — no desktop, no toolbar. Draw and present directly.
        // Full-window overlays are drawn at logical size and pixel-scaled up.
        let k = ui_scale_of(window);
        let (lw, lh) = ((win_w / k).max(1), (win_h / k).max(1));
        let full_overlay = self.login_prompt.is_some()
            || self.host_prompt.is_some()
            || self.type_prompt.is_some()
            || self.about_open;
        if full_overlay {
            let paint = |buf: &mut [u32], w: u32, h: u32| {
                if let Some(prompt) = self.login_prompt.as_ref() {
                    login_prompt::draw(buf, w, h, self.font.as_ref(), prompt);
                } else if let Some(prompt) = self.host_prompt.as_ref() {
                    host_prompt::draw(buf, w, h, self.font.as_ref(), prompt);
                } else if let Some(prompt) = self.type_prompt.as_ref() {
                    type_text::draw(buf, w, h, self.font.as_ref(), prompt);
                } else {
                    draw_about(buf, w, h, self.font.as_ref());
                }
            };
            if k == 1 {
                paint(&mut buffer[..], win_w, win_h);
            } else {
                let mut lbuf = vec![0u32; (lw * lh) as usize];
                paint(&mut lbuf, lw, lh);
                upscale_into(&lbuf, lw, lh, k, &mut buffer[..], win_w, win_h);
            }
            let _ = buffer.present();
            return;
        }

        // The remote desktop renders BELOW the toolbar strip (reserved at top).
        let bar_h = (bar_logical * k).min(win_h);
        let frame = self.shared.lock().unwrap();
        let frame_count = frame.frames;
        let bytes_per_sec = frame.bytes_per_sec;
        let connected = frame.width != 0 && frame.height != 0 && !frame.rgba.is_empty();
        let monitor_label = Self::monitor_label_for(view, frame.width, frame.height);
        let desk_rect = {
            let (_, _, cw, ch) = Self::crop_rect(view, frame.width, frame.height);
            desktop_rect((0, 0, cw.max(1), ch.max(1)), win_w, win_h, bar_h)
        };
        let status = frame.status.clone();
        let security = frame.security.clone();
        let secure = frame.secure;
        let encrypted = frame.encrypted;
        // Session recording: queue the current desktop (throttled internally).
        if let Some(rec) = self.recorder.as_mut() {
            rec.maybe_capture(frame.width, frame.height, &frame.rgba);
        }
        if !connected {
            let fill = if self.closed.is_some() {
                0x0040_0000
            } else {
                0x0020_2020
            };
            for px in buffer.iter_mut() {
                *px = fill;
            }
            // Connection-progress overlay: host title + current phase + a spinner.
            let msg = if let Some(reason) = &self.closed {
                t!("status.disconnected_reason", reason => reason).to_string()
            } else if status.is_empty() {
                t!("status.connecting").to_string()
            } else {
                status
            };
            let cx = win_w / 2;
            let cy = win_h / 2;
            // Animated spinner above the text (only while still connecting).
            if self.closed.is_none() && !quiet {
                draw_spinner(
                    &mut buffer[..],
                    win_w,
                    win_h,
                    cx,
                    cy.saturating_sub(72),
                    self.connect_start.elapsed(),
                );
            }
            if quiet {
            } else if let Some(f) = self.font.as_ref() {
                f.draw_centered(
                    &mut buffer[..],
                    win_w,
                    win_h,
                    cy as f32 + 4.0,
                    &self.host,
                    0x00FF_FFFF,
                    34.0,
                );
                f.draw_centered(
                    &mut buffer[..],
                    win_w,
                    win_h,
                    cy as f32 + 38.0,
                    &msg,
                    0x00B0_C0D0,
                    19.0,
                );
            } else {
                toolbar::draw_text_centered(
                    &mut buffer[..],
                    win_w,
                    win_h,
                    cy.saturating_sub(28),
                    &self.host,
                    0x00FF_FFFF,
                    3,
                );
                toolbar::draw_text_centered(
                    &mut buffer[..],
                    win_w,
                    win_h,
                    cy + 8,
                    &msg,
                    0x00B0_C0D0,
                    2,
                );
            }
        } else {
            let (fb_w, fb_h) = (frame.width, frame.height);
            let src = &frame.rgba;
            let fbw = fb_w as usize;
            // Active monitor crop (RRCV-20): blit only this source sub-rect, scaled
            // to fill the desktop area. `All` = the whole framebuffer.
            let (cx, cy, cw, ch) = Self::crop_rect(view, fb_w, fb_h);
            let (cx, cy) = (cx as usize, cy as usize);
            // The shown region keeps its aspect ratio: letterbox/pillarbox bars in
            // the background colour, the desktop in `desk_rect`.
            let (rx, ry, rw, rh) = desk_rect;
            for wy in bar_h..win_h {
                let out_row = (wy * win_w) as usize;
                buffer[out_row..out_row + win_w as usize].fill(CPU_BAR_FILL);
            }
            if rw == cw && rh == ch {
                // 1:1 — direct copy, sharpest (no interpolation).
                for wy in ry..ry + rh {
                    let row = (cy + (wy - ry) as usize) * fbw + cx;
                    let out_row = (wy * win_w + rx) as usize;
                    for wx in 0..rw as usize {
                        let si = (row + wx) * 4;
                        buffer[out_row + wx] = if si + 2 < src.len() {
                            ((src[si] as u32) << 16)
                                | ((src[si + 1] as u32) << 8)
                                | (src[si + 2] as u32)
                        } else {
                            0
                        };
                    }
                }
            } else {
                // Bilinear scale of the cropped sub-rect. (A dirty-region variant was
                // tried but needs a full per-frame copy to erase the client cursor,
                // costing ~the same; the 60 fps paint cap is the real client win.)
                let map = |dst: u32, dst_n: u32, src_n: u32| -> (usize, usize, u32) {
                    // saturating_sub guards a 0-dim source (the `connected` check
                    // upstream already ensures src_n > 0, but the invariant is
                    // non-local — don't let it underflow-panic here).
                    let max_i = (src_n as usize).saturating_sub(1);
                    let s = (((dst as f64 + 0.5) * src_n as f64 / dst_n as f64) - 0.5).max(0.0);
                    let i0 = (s as usize).min(max_i);
                    let i1 = (i0 + 1).min(max_i);
                    (i0, i1, ((s - i0 as f64) * 256.0) as u32)
                };
                // Column/row sample indices are taken within the crop, then shifted
                // by the crop origin (cx, cy) into the full framebuffer.
                let cols: Vec<(usize, usize, u32)> = (0..rw)
                    .map(|wx| {
                        let (i0, i1, f) = map(wx, rw, cw);
                        (cx + i0, cx + i1, f)
                    })
                    .collect();
                #[inline(always)]
                fn lerp(a: u32, b: u32, f: u32) -> u32 {
                    (a * (256 - f) + b * f) >> 8
                }
                for wy in ry..ry + rh {
                    let (y0, y1, fy) = map(wy - ry, rh, ch);
                    let r0 = (cy + y0) * fbw;
                    let r1 = (cy + y1) * fbw;
                    let out_row = (wy * win_w + rx) as usize;
                    for (wx, &(x0, x1, fx)) in cols.iter().enumerate() {
                        let p00 = (r0 + x0) * 4;
                        let p01 = (r0 + x1) * 4;
                        let p10 = (r1 + x0) * 4;
                        let p11 = (r1 + x1) * 4;
                        if p11 + 2 >= src.len() {
                            continue;
                        }
                        let mut out = 0u32;
                        for ch in 0..3 {
                            let top = lerp(src[p00 + ch] as u32, src[p01 + ch] as u32, fx);
                            let bot = lerp(src[p10 + ch] as u32, src[p11 + ch] as u32, fx);
                            out |= lerp(top, bot, fy) << (16 - ch * 8);
                        }
                        buffer[out_row + wx] = out;
                    }
                }
            }
        }
        drop(frame);

        // FPS: recompute once per second from the cumulative frame counter.
        let now = std::time::Instant::now();
        let el = now.duration_since(self.fps_t);
        if el >= std::time::Duration::from_secs(1) {
            self.fps = ((frame_count.wrapping_sub(self.fps_base)) as f64 / el.as_secs_f64()) as u32;
            self.fps_base = frame_count;
            self.fps_t = now;
        }

        // Client-side cursor: draw the remote cursor shape at the LOCAL mouse
        // position so it tracks instantly (no server round-trip), and hide the OS
        // cursor while we do. Falls back to the OS cursor when there's no shape.
        let cur = self.cursor.lock().unwrap();
        let draw_remote = cur.draw
            && !cur.rgba.is_empty()
            && self.cursor_inside
            && connected
            && mouse_in_rect(self.mouse_win, desk_rect);
        window.set_cursor_visible(!draw_remote);
        if draw_remote {
            // The remote cursor arrives as a separate Pointer PDU (the host sends it
            // out-of-band, NOT baked into the desktop bitmap — see
            // pointer_software_rendering=false + the FullControl grant), so the
            // desktop underneath the cursor is already clean. We just alpha-blend our
            // cursor bitmap on top. RRCV-18: the old background-fill patch that hid a
            // (no-longer-present) server-baked cursor is removed — that flat patch was
            // itself visible as a block over non-uniform backgrounds.
            let (cw, ch) = (cur.width as i32, cur.height as i32);
            let ox = self.mouse_win.0 as i32 - cur.hotspot_x as i32;
            let oy = self.mouse_win.1 as i32 - cur.hotspot_y as i32;
            for j in 0..ch {
                let py = oy + j;
                if py < 0 || py >= win_h as i32 {
                    continue;
                }
                for i in 0..cw {
                    let px = ox + i;
                    if px < 0 || px >= win_w as i32 {
                        continue;
                    }
                    let si = ((j * cw + i) as usize) * 4;
                    if si + 3 >= cur.rgba.len() {
                        continue;
                    }
                    let a = cur.rgba[si + 3] as u32;
                    if a == 0 {
                        continue;
                    }
                    let (r, g, b) = (
                        cur.rgba[si] as u32,
                        cur.rgba[si + 1] as u32,
                        cur.rgba[si + 2] as u32,
                    );
                    let di = (py as u32 * win_w + px as u32) as usize;
                    buffer[di] = if a == 255 {
                        (r << 16) | (g << 8) | b
                    } else {
                        let d = buffer[di];
                        let (dr, dg, db) = ((d >> 16) & 0xff, (d >> 8) & 0xff, d & 0xff);
                        (((r * a + dr * (255 - a)) / 255) << 16)
                            | (((g * a + dg * (255 - a)) / 255) << 8)
                            | ((b * a + db * (255 - a)) / 255)
                    };
                }
            }
        }
        drop(cur);

        // Overlay toolbar/status bar on top.
        let state = if connected && self.switch_flash > 0 {
            self.switch_flash -= 1;
            if let Some(w) = &self.window {
                w.request_redraw(); // keep the flash animating without server frames
            }
            t!("monitor.switching").to_string()
        } else if connected {
            t!("status.connected").to_string()
        } else if self.closed.is_some() {
            t!("status.disconnected").to_string()
        } else {
            t!("status.connecting").to_string()
        };
        let mode = if self.view_only {
            t!("mode.view_only")
        } else {
            t!("mode.control")
        }
        .to_string();
        let status = toolbar::Status {
            host: &self.host,
            mode: &mode,
            state: &state,
            connected,
            fps: self.fps,
            bytes_per_sec,
            recording: self.recorder.is_some(),
            curtain: self.curtain.load(Ordering::Relaxed),
            security: if connected { &security } else { "" },
            secure,
            view_only: self.view_only,
            encrypted,
            monitor: monitor_label.as_deref(),
        };
        if bar_h == 0 {
            // Native macOS toolbar: nothing to draw over the desktop.
        } else if k == 1 {
            toolbar::draw(&mut buffer[..], win_w, win_h, &status, self.font.as_ref());
        } else {
            let strip_h = toolbar::TOOLBAR_H.min(lh);
            let mut lbuf = vec![0u32; (lw * strip_h) as usize];
            toolbar::draw(&mut lbuf, lw, strip_h, &status, self.font.as_ref());
            let rows = (bar_h as usize) * (win_w as usize);
            upscale_into(&lbuf, lw, strip_h, k, &mut buffer[..rows], win_w, bar_h);
        }

        // Viewer-side paint cost (scale + cursor + toolbar), excluding present.
        self.rprof_accum += rstart.elapsed();
        self.rprof_n += 1;
        if std::env::var("SCCM_RC_PROFILE").as_deref() == Ok("1")
            && self.rprof_t.elapsed() >= std::time::Duration::from_secs(1)
        {
            let n = self.rprof_n.max(1);
            let avg_us = self.rprof_accum.as_micros() as u64 / n as u64;
            info!(
                paints = self.rprof_n,
                avg_paint_us = avg_us,
                win = format!("{win_w}x{win_h}"),
                "RENDER PROFILE (viewer-side paint)"
            );
            self.rprof_accum = std::time::Duration::ZERO;
            self.rprof_n = 0;
            self.rprof_t = std::time::Instant::now();
        }

        let _ = buffer.present();
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::{next_backoff, RECONNECT_MAX, RECONNECT_MIN};
    use std::time::Duration;

    #[test]
    fn doubles_on_quick_failures_and_caps() {
        let mut d = RECONNECT_MIN;
        let mut seen = vec![];
        for _ in 0..8 {
            d = next_backoff(d, Duration::from_millis(5));
            seen.push(d.as_secs());
        }
        assert_eq!(seen, vec![2, 4, 8, 16, 30, 30, 30, 30]);
    }

    #[test]
    fn resets_after_a_long_session() {
        assert_eq!(
            next_backoff(RECONNECT_MAX, Duration::from_secs(600)),
            RECONNECT_MIN
        );
    }
}

#[cfg(test)]
mod ui_scale_tests {
    use super::upscale_into;

    #[test]
    fn doubles_each_pixel() {
        let src = [1u32, 2, 3, 4]; // 2x2
        let mut dst = vec![0u32; 16]; // 4x4
        upscale_into(&src, 2, 2, 2, &mut dst, 4, 4);
        assert_eq!(dst, vec![1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
    }

    #[test]
    fn odd_physical_size_repeats_the_edge() {
        let src = [1u32, 2]; // 2x1
        let mut dst = vec![0u32; 5 * 3];
        upscale_into(&src, 2, 1, 2, &mut dst, 5, 3);
        assert_eq!(&dst[0..5], &[1, 1, 2, 2, 2]);
        assert_eq!(&dst[10..15], &[1, 1, 2, 2, 2]);
    }

    #[test]
    fn scale_one_is_identity() {
        let src = [7u32, 8, 9];
        let mut dst = vec![0u32; 3];
        upscale_into(&src, 3, 1, 1, &mut dst, 3, 1);
        assert_eq!(dst, src);
    }
}

#[cfg(test)]
mod aspect_tests {
    use super::{fit_rect, map_to_desktop, release_pdus};
    use super::PointerFlags;

    #[test]
    fn fit_rect_letterboxes_wide_window() {
        // 1024x768 into a 1467x650 area: height-limited, centred horizontally.
        assert_eq!(fit_rect(1024, 768, 1467, 650), (300, 0, 867, 650));
    }

    #[test]
    fn fit_rect_pillarboxes_tall_window() {
        assert_eq!(fit_rect(1920, 1080, 800, 800), (0, 175, 800, 450));
    }

    #[test]
    fn fit_rect_exact_fit() {
        assert_eq!(fit_rect(1024, 768, 1024, 768), (0, 0, 1024, 768));
        assert_eq!(fit_rect(1024, 768, 512, 384), (0, 0, 512, 384));
    }

    #[test]
    fn cursor_in_bar_is_outside() {
        let rect = fit_rect(1024, 768, 1467, 650); // window area, bar_h = 0
        let crop = (0, 0, 1024, 768);
        let fb = (1024, 768);
        assert_eq!(map_to_desktop(100.0, 300.0, rect, crop, fb), None);
        assert_eq!(map_to_desktop(1300.0, 300.0, rect, crop, fb), None);
        // Centre of the window maps to the centre of the desktop.
        assert_eq!(
            map_to_desktop(734.0, 325.5, rect, crop, fb),
            Some((512, 384))
        );
        // Corners of the rect map to the desktop corners.
        assert_eq!(map_to_desktop(300.0, 0.0, rect, crop, fb), Some((0, 0)));
        assert_eq!(
            map_to_desktop(1166.9, 649.9, rect, crop, fb),
            Some((1023, 767))
        );
    }

    #[test]
    fn held_buttons_released_on_focus_loss() {
        let mut held = 1 | 2 | 4;
        let ev = release_pdus(&mut held, 10, 20);
        assert_eq!(held, 0);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].flags, PointerFlags::LEFT_BUTTON);
        assert_eq!(ev[1].flags, PointerFlags::RIGHT_BUTTON);
        assert!(ev.iter().all(|e| e.x_position == 10 && e.y_position == 20));
        // Nothing held: nothing to send.
        assert!(release_pdus(&mut held, 0, 0).is_empty());
    }

    #[test]
    fn cursor_honours_toolbar_offset_and_crop() {
        // Second monitor of a 3840x1080 desktop, window 960x540 below a 40px bar.
        let rect = (0, 40, 960, 540);
        let crop = (1920, 0, 1920, 1080);
        assert_eq!(
            map_to_desktop(0.0, 40.0, rect, crop, (3840, 1080)),
            Some((1920, 0))
        );
        assert_eq!(map_to_desktop(10.0, 20.0, rect, crop, (3840, 1080)), None);
    }
}
