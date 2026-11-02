//! StreetPad: walk Google Street View with a game controller.
//!
//! One portable .exe. Uses the WebView2 runtime built into Windows 10/11, stores settings in
//! HKCU\Software\StreetPad, and keeps the Google login either in a persistent profile
//! (%LOCALAPPDATA%\StreetPad) or a temporary one that is wiped on exit.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod control;
mod geo;
mod input;
mod settings;
mod steer;

use control::Driver;
use input::{DeviceInfo, InputEvent, Shared};
use settings::{ACTIONS, AXIS_TARGETS, BUTTONS, Bookmark, CONTINUOUS, MAX_BOOKMARKS, MAX_PROFILES, Profile, Settings, action_label, button_label_for, family_name, trigger_labels};
use steer::{StartError, Steer};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{DeviceEvent, Event, WindowEvent};
use tao::event_loop::{ControlFlow, DeviceEventFilter, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::{IconExtWindows, WindowExtWindows};
use tao::window::{Fullscreen, Icon, Window, WindowBuilder};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW};
use windows::core::HSTRING;
use wry::{Rect, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows};

const APP: &str = "StreetPad";
const MAPS_HOME: &str = "https://www.google.com/maps";
const INJECT_JS: &str = include_str!("inject.js");
const UI_HTML: &str = include_str!("ui.html");
const PRIVATE_PREFIX: &str = "StreetPad-private-";
/// Browser engine flags (identical for both webviews: they share one browser environment).
/// The first part is wry's default; the rest makes sure GPU acceleration (WebGL 3D, GPU
/// rasterization) is used even on GPUs Chromium would otherwise block-list.
/// The background flags keep the page running at full speed when StreetPad is not the active
/// window or is covered, so the controller works the same in the background.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion \
    --disable-background-timer-throttling --disable-backgrounding-occluded-windows --disable-renderer-backgrounding \
    --ignore-gpu-blocklist --enable-gpu-rasterization --enable-zero-copy";
const WEBVIEW2_DOWNLOAD: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

#[derive(Debug)]
enum UserEvent {
    Input(InputEvent),
    /// IPC message from the Google page (`from_ui = false`) or the settings panel (`true`).
    Ipc { from_ui: bool, body: String },
    /// The Google page's document title changed (used to name bookmarks).
    Title(String),
    /// The coverage layer is off (cleanup done).
    CoverageClean,
    /// The search keyboard is no longer on the page (e.g. the page changed): leave keyboard mode.
    KeyboardGone,
    /// The coverage layer was switched on by us (true) or was already on / unavailable.
    CoverageOn(bool),
}

fn message_box(text: &str, error: bool) {
    let style = MB_OK | if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(APP), style);
    }
}

fn persistent_profile_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join(APP)
}

fn private_profile_dir() -> PathBuf {
    std::env::temp_dir().join(format!("{PRIVATE_PREFIX}{}", std::process::id()))
}

/// Remove private profiles left behind by earlier runs (e.g. after a crash or power loss).
/// Profiles still in use by a running instance are locked and simply fail to delete.
fn cleanup_stale_private_profiles(keep: &Path) {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let is_ours = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(PRIVATE_PREFIX));
        if is_ours && p != keep {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

fn remove_dir_with_retry(dir: &Path) {
    // The WebView2 browser processes take a moment to exit and release their files.
    for _ in 0..40 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => std::thread::sleep(Duration::from_millis(150)),
        }
    }
}

/// The WebView2 (Microsoft Edge) engine version, e.g. "156.0.4314.58".
fn engine_version(main: &WebView) -> String {
    let env = main.environment();
    let mut out = windows::core::PWSTR::null();
    unsafe {
        if env.BrowserVersionString(&mut out).is_ok() && !out.is_null() {
            let v = out.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(out.0 as *const _));
            return v;
        }
    }
    String::new()
}

/// Percent-encode a search query for a URL path segment.
fn url_encode(q: &str) -> String {
    q.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn is_google_url(url: &str) -> bool {
    settings::is_google_maps_url(url)
}

/// Only one StreetPad at a time (it reads the controller in the background, so two copies
/// would both react). A second launch brings the running window to the front and exits.
/// Returns the mutex handle to keep (released before a self-restart), or None to exit.
fn single_instance() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM};
    use windows::core::BOOL;
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, SW_RESTORE, SetForegroundWindow, ShowWindow};
    use windows::core::w;

    let handle = unsafe { CreateMutexW(None, true, w!("Local\\StreetPad.SingleInstance")) }.ok()?;
    if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return Some(handle);
    }
    unsafe extern "system" fn find(hwnd: HWND, _: LPARAM) -> BOOL {
        let mut buf = [0u16; 128];
        let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
        if String::from_utf16_lossy(&buf[..n]).starts_with("StreetPad  ·") {
            unsafe {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
            return BOOL(0); // found: stop
        }
        BOOL(1)
    }
    unsafe {
        let _ = EnumWindows(Some(find), LPARAM(0));
    }
    None
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        message_box(&format!("StreetPad hit an unexpected error and must close.\n\n{info}"), true);
    }));
    let Some(instance) = single_instance() else { return };

    let settings = Settings::load();
    let private = settings.login_mode == "private";
    let profile = if private { private_profile_dir() } else { persistent_profile_dir() };
    cleanup_stale_private_profiles(&profile);
    if let Err(e) = std::fs::create_dir_all(&profile) {
        message_box(&format!("Could not create the browser profile folder:\n{}\n\n{e}", profile.display()), true);
        return;
    }

    let restart = run(settings, &profile, private);

    if private {
        remove_dir_with_retry(&profile);
    }
    if restart {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(instance); // let the new copy start
        }
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).spawn();
        }
    }
}

/// After an A click in Street View: the view before the click.
struct Freeze {
    since: Instant,
    /// Page URL at the click: the view to return to when the panel is closed.
    url: String,
    pos: (f64, f64),
    heading: f64,
    /// A place panel opened: stay frozen until the next controller input.
    held: bool,
}

struct App {
    window: Window,
    main: WebView,
    ui: WebView,
    driver: Driver,
    shared: Arc<Shared>,
    devices: Vec<DeviceInfo>,
    /// Controller in use.
    active: Option<DeviceInfo>,
    private: bool,
    running_login_mode: String,
    profile: PathBuf,
    last_saved_url: String,
    next_url_save: Instant,
    /// Pointer hidden because the controller is in use; None = visible.
    cursor_hidden_at: Option<Instant>,
    steer: Steer,
    title: String,
    /// Toast shown once the newly loaded page is ready.
    pending_toast: Option<(Instant, String)>,
    /// Immersive mode (Google's buttons and panels hidden).
    immersive: bool,
    next_immersive_apply: Instant,
    next_mode_check: Instant,
    /// View pressed on the map: quit after this event.
    quit_requested: bool,
    /// Picture frozen after an A click (see `freeze_tick`).
    freeze: Option<Freeze>,
    /// When the last panel reset reopened Street View.
    last_reset: Option<Instant>,
    /// After closing a place without a reload: (when, click-time URL) for the safety net.
    place_watch: Option<(Instant, String)>,
    proxy: EventLoopProxy<UserEvent>,
    /// Hold-Y on the map is in progress (blue lines showing until Y is released).
    sv_hold: bool,
    /// Where the user aimed when entering Street View (for the fallback and distance hint).
    sv_target: Option<((f64, f64), Instant)>,
    /// If Street View hasn't opened by then after releasing Y, open the nearest photo by URL.
    sv_fallback_at: Option<Instant>,
    /// Last position seen in Street View, and the exit spot to put the crosshair on.
    last_sv_pos: Option<(f64, f64)>,
    exit_target: Option<((f64, f64), Instant)>,
    exit_reloaded: bool,
    /// Attempts left to switch the coverage layer off.
    coverage_tries: u8,
    /// We switched Google's coverage layer on and must switch it off once back on the map.
    coverage_cleanup: bool,
    coverage_off_at: Option<Instant>,
    /// Diagnostics (--debug-actions <file>): [[delay_ms, kind, arg], ...] controller events
    /// (kind: action, step, hold_end, zoom, kbnav), played as if from the controller.
    debug_actions: Vec<(Instant, String, String)>,
    /// Diagnostics (--debug-sample <js>): every 500 ms append {t,url,v} to
    /// %TEMP%\streetpad-samples.txt (v = the expression's value in the Google page).
    debug_sample: Option<(String, Instant, Instant)>,
    /// Diagnostics (--debug-js-ui <file>): script run in the settings page at 10 s.
    debug_js_ui: Option<(Instant, String)>,
    /// Diagnostics (--debug-cdp <file>): [[delay_ms, method, params], ...] DevTools commands.
    debug_cdp: Vec<(Instant, String, serde_json::Value)>,
    /// Diagnostics (--debug-js <file>): script to run once the page has loaded.
    debug_js: Option<(Instant, String)>,
    /// When the on-screen button guide was opened (None = hidden).
    guide_shown_at: Option<Instant>,
    /// WebView2 engine version, for the About section.
    engine_version: String,
    last_periodic: Instant,
}

/// Runs the app. Returns true when it should restart itself (login mode changed).
fn run(settings: Settings, profile: &Path, private: bool) -> bool {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    // Raw mouse input tells real mouse use apart from our own simulated drags. The window's
    // focus sits in the child webview, so ask for device events regardless of focus.
    event_loop.set_device_event_filter(DeviceEventFilter::Never);

    // Reopen on the monitor used last time (if it's still connected).
    let monitor = event_loop.available_monitors().find(|m| m.name().is_some_and(|n| n == settings.monitor));
    let mut builder = WindowBuilder::new();
    if let Some(m) = &monitor {
        builder = builder.with_position(m.position());
    }
    let window = match builder
        .with_title(APP)
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .with_min_inner_size(LogicalSize::new(640.0, 420.0))
        .with_maximized(true)
        .with_window_icon(Icon::from_resource(1, None).ok())
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => {
            message_box(&format!("Could not create the window.\n\n{e}"), true);
            return false;
        }
    };
    if settings.fullscreen {
        window.set_fullscreen(Some(Fullscreen::Borderless(monitor.clone())));
    }

    let arg_url = {
        let args: Vec<String> = std::env::args().collect();
        args.iter().position(|a| a == "--url").and_then(|i| args.get(i + 1).cloned())
    };
    let start_url = if let Some(u) = arg_url {
        u
    } else if settings.start_at_last_location && is_google_url(&settings.last_url) {
        settings.last_url.clone()
    } else {
        MAPS_HOME.to_string()
    };
    let running_login_mode = settings.login_mode.clone();
    let last_saved_url = settings.last_url.clone();

    let shared = Arc::new(Shared {
        settings: RwLock::new(settings),
        ui_visible: AtomicBool::new(false),
        hwnd: AtomicIsize::new(window.hwnd()),
        look_vel: std::sync::atomic::AtomicU64::new(0),
        capture: std::sync::Mutex::new(None),
        keyboard: AtomicBool::new(false),
        haptics: std::sync::Mutex::new(Vec::new()),
        street_view: AtomicBool::new(true),
    });

    let mut context = WebContext::new(Some(profile.to_path_buf()));
    let (main, ui) = match build_webviews(&window, &mut context, &proxy, &start_url, private) {
        Ok(v) => v,
        Err(e) => {
            message_box(
                &format!(
                    "StreetPad needs the Microsoft Edge WebView2 Runtime, which is part of \
                     Windows 10/11. It seems to be missing or broken on this PC.\n\n\
                     Install it from:\n{WEBVIEW2_DOWNLOAD}\n\nDetails: {e}"
                ),
                true,
            );
            return false;
        }
    };

    let mut app = App {
        driver: Driver::new(main.webview()),
        window,
        main,
        ui,
        shared: shared.clone(),
        devices: Vec::new(),
        active: None,
        private,
        running_login_mode,
        profile: profile.to_path_buf(),
        last_saved_url,
        next_url_save: Instant::now() + Duration::from_secs(10),
        cursor_hidden_at: None,
        steer: Steer::new(),
        title: String::new(),
        pending_toast: None,
        immersive: false,
        next_immersive_apply: Instant::now(),
        next_mode_check: Instant::now(),
        quit_requested: false,
        freeze: None,
        last_reset: None,
        place_watch: None,
        proxy: proxy.clone(),
        sv_hold: false,
        sv_target: None,
        sv_fallback_at: None,
        last_sv_pos: None,
        exit_target: None,
        exit_reloaded: false,
        coverage_tries: 20,
        // At startup, make sure Google's coverage layer is off (Google remembers it between
        // sessions); the blue lines should only show while Y is held.
        coverage_cleanup: true,
        coverage_off_at: Some(Instant::now() + Duration::from_secs(4)),
        debug_actions: {
            let args: Vec<String> = std::env::args().collect();
            let start = Instant::now() + Duration::from_secs(10);
            args.iter()
                .position(|a| a == "--debug-actions")
                .and_then(|i| args.get(i + 1))
                .and_then(|f| std::fs::read_to_string(f).ok())
                .and_then(|j| serde_json::from_str::<Vec<(u64, String, String)>>(&j).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|(ms, k, a)| (start + Duration::from_millis(ms), k, a))
                .collect()
        },
        debug_sample: {
            let args: Vec<String> = std::env::args().collect();
            args.iter().position(|a| a == "--debug-sample").and_then(|i| args.get(i + 1)).map(|js| {
                let _ = std::fs::remove_file(std::env::temp_dir().join("streetpad-samples.txt"));
                // "@file" reads the expression from a file (avoids command-line quoting issues).
                let js = js.strip_prefix('@').and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_else(|| js.clone());
                (js, Instant::now(), Instant::now())
            })
        },
        debug_js_ui: {
            let args: Vec<String> = std::env::args().collect();
            args.iter()
                .position(|a| a == "--debug-js-ui")
                .and_then(|i| args.get(i + 1))
                .and_then(|f| std::fs::read_to_string(f).ok())
                .map(|js| (Instant::now() + Duration::from_secs(10), js))
        },
        debug_cdp: {
            let args: Vec<String> = std::env::args().collect();
            let start = Instant::now() + Duration::from_secs(10);
            args.iter()
                .position(|a| a == "--debug-cdp")
                .and_then(|i| args.get(i + 1))
                .and_then(|f| std::fs::read_to_string(f).ok())
                .and_then(|j| serde_json::from_str::<Vec<(u64, String, serde_json::Value)>>(&j).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|(ms, m, p)| (start + Duration::from_millis(ms), m, p))
                .collect()
        },
        debug_js: {
            let args: Vec<String> = std::env::args().collect();
            args.iter()
                .position(|a| a == "--debug-js")
                .and_then(|i| args.get(i + 1))
                .and_then(|f| std::fs::read_to_string(f).ok())
                .map(|js| (Instant::now() + Duration::from_secs(10), js))
        },
        guide_shown_at: None,
        engine_version: String::new(),
        last_periodic: Instant::now(),
    };
    app.engine_version = engine_version(&app.main);
    app.driver.keep_page_focused();
    app.layout();
    app.update_title();
    if std::env::args().any(|a| a == "--settings") {
        app.show_ui(true); // diagnostics: open the settings panel at startup
    }

    let input_proxy = proxy.clone();
    let frame_proxy = proxy.clone();
    input::spawn(
        shared,
        move |ev| {
            let _ = input_proxy.send_event(UserEvent::Input(ev));
        },
        move |ev| {
            let _ = frame_proxy.send_event(UserEvent::Input(ev));
        },
    );

    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(20));
        match event {
            Event::MainEventsCleared => {
                if app.last_periodic.elapsed() >= Duration::from_millis(15) {
                    app.last_periodic = Instant::now();
                    app.periodic();
                }
            }
            Event::WindowEvent { event: WindowEvent::Resized(_), .. }
            | Event::WindowEvent { event: WindowEvent::ScaleFactorChanged { .. }, .. } => app.layout(),
            Event::WindowEvent { event: WindowEvent::Focused(false), .. } => app.driver.release(),
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                app.save_monitor();
                app.save_location(true);
                *control_flow = ControlFlow::Exit;
            }
            Event::DeviceEvent {
                event: DeviceEvent::MouseMotion { .. } | DeviceEvent::MouseWheel { .. } | DeviceEvent::Button { .. },
                ..
            } => app.on_real_mouse(),
            Event::UserEvent(UserEvent::Input(ev)) => {
                app.on_input(ev);
                if app.quit_requested {
                    app.save_monitor();
                    app.save_location(true);
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Title(t)) => app.title = t,
            Event::UserEvent(UserEvent::KeyboardGone) => app.shared.keyboard.store(false, Ordering::Relaxed),
            Event::UserEvent(UserEvent::CoverageClean) => app.coverage_cleanup = false,
            Event::UserEvent(UserEvent::CoverageOn(ours)) => {
                if ours {
                    app.coverage_cleanup = true;
                    app.coverage_tries = 20;
                }
            }
            Event::UserEvent(UserEvent::Ipc { from_ui, body }) => {
                if app.on_ipc(from_ui, &body) {
                    app.save_location(true);
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    });
    // `app` (and with it both webviews) has been dropped here by run_return's closure cleanup.
    RESTART.load(Ordering::SeqCst)
}

/// Set when the app should relaunch itself after exiting.
static RESTART: AtomicBool = AtomicBool::new(false);

fn build_webviews(
    window: &Window,
    context: &mut WebContext,
    proxy: &EventLoopProxy<UserEvent>,
    start_url: &str,
    private: bool,
) -> wry::Result<(WebView, WebView)> {
    let p = proxy.clone();
    let main = WebViewBuilder::new_with_web_context(context)
        .with_url(start_url)
        .with_initialization_script(INJECT_JS)
        .with_ipc_handler(move |req| {
            let _ = p.send_event(UserEvent::Ipc { from_ui: false, body: req.body().clone() });
        })
        .with_document_title_changed_handler({
            let p = proxy.clone();
            move |title| {
                let _ = p.send_event(UserEvent::Title(title));
            }
        })
        .with_incognito(private)
        .with_hotkeys_zoom(false) // page zoom would break the pointer coordinates
        .with_devtools(false)
        .with_focused(true)
        .with_additional_browser_args(BROWSER_ARGS)
        .with_background_color((20, 22, 27, 255))
        .with_browser_accelerator_keys(true)
        .build_as_child(window)?;

    let p = proxy.clone();
    let ui = WebViewBuilder::new_with_web_context(context)
        .with_html(UI_HTML)
        .with_ipc_handler(move |req| {
            let _ = p.send_event(UserEvent::Ipc { from_ui: true, body: req.body().clone() });
        })
        .with_incognito(private)
        .with_hotkeys_zoom(false)
        .with_devtools(false)
        .with_visible(false)
        .with_additional_browser_args(BROWSER_ARGS)
        .with_background_color((21, 24, 30, 255))
        .with_default_context_menus(false)
        .with_browser_accelerator_keys(false)
        .build_as_child(window)?;
    Ok((main, ui))
}

impl App {
    fn layout(&mut self) {
        let size = self.window.inner_size();
        let scale = self.window.scale_factor();
        let _ = self.main.set_bounds(Rect {
            position: PhysicalPosition::new(0, 0).into(),
            size: PhysicalSize::new(size.width, size.height).into(),
        });
        let logical: LogicalSize<f64> = size.to_logical(scale);
        self.driver.size = (logical.width.max(1.0), logical.height.max(1.0));

        // Settings is a full-window page (content centred by its own CSS), so it never looks
        // like a box overlapping the map.
        let _ = self.ui.set_bounds(Rect {
            position: PhysicalPosition::new(0, 0).into(),
            size: PhysicalSize::new(size.width, size.height).into(),
        });
    }

    fn update_title(&self) {
        let device = self.active.as_ref().map(|d| d.name.as_str()).unwrap_or_else(|| {
            let wanted = self.shared.settings.read().map(|s| s.device != "auto").unwrap_or(false);
            if wanted { "selected controller not connected" } else { "no controller connected" }
        });
        let mode = if self.private { "  ·  Private mode" } else { "" };
        let menu = button_label_for("start", self.family());
        self.window.set_title(&format!("{APP}  ·  {device}{mode}  ·  F1 / {menu}: settings"));
    }

    fn ui_visible(&self) -> bool {
        self.shared.ui_visible.load(Ordering::Relaxed)
    }

    fn show_ui(&mut self, show: bool) {
        self.unfreeze();
        if std::mem::take(&mut self.sv_hold) {
            self.coverage_off_at = Some(Instant::now());
        }
        if show && self.shared.keyboard.load(Ordering::Relaxed) {
            self.close_keyboard();
        }
        if !show {
            if let Ok(mut c) = self.shared.capture.lock() {
                *c = None;
            }
        }
        self.shared.ui_visible.store(show, Ordering::Relaxed);
        self.driver.release();
        let _ = self.ui.set_visible(show);
        if show {
            self.push_state();
            let _ = self.ui.focus();
        } else {
            let _ = self.main.focus();
        }
    }

    /// Bring StreetPad to the front and give it the keyboard focus. Windows blocks background
    /// programs from doing this, so StreetPad briefly attaches its input queue to the window in
    /// front (no fake key presses), then brings itself forward.
    fn bring_to_front(&mut self) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        use windows::Win32::UI::WindowsAndMessaging::{
            BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
        };
        let hwnd = HWND(self.window.hwnd() as *mut _);
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let fg = GetForegroundWindow();
            let fg_thread = GetWindowThreadProcessId(fg, None);
            let me = GetCurrentThreadId();
            let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, true).as_bool();
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
            if attached {
                let _ = AttachThreadInput(me, fg_thread, false);
            }
        }
        self.window.set_focus();
        let _ = if self.ui_visible() { self.ui.focus() } else { self.main.focus() };
    }

    fn toggle_fullscreen(&mut self) {
        let fs = self.window.fullscreen().is_none();
        // Session-only: the "start in fullscreen" setting is not changed.
        self.window.set_fullscreen(if fs { Some(Fullscreen::Borderless(None)) } else { None });
    }

    fn push_state(&self) {
        let Ok(s) = self.shared.settings.read() else { return };
        let state = serde_json::json!({
            "settings": &*s,
            "devices": self.devices,
            "active": self.active.as_ref().map(|d| d.name.clone()),
            "family": self.family(),
            "familyName": family_name(self.family()),
            "deviceKind": self.active.as_ref().map(|d| d.kind),
            "profiles": s.all_profiles().iter().map(|p| serde_json::json!({
                "id": p.id, "name": p.name, "family": p.family, "builtin": !p.id.starts_with("user:"),
                "remapped": !p.remap.inputs.is_empty(),
            })).collect::<Vec<_>>(),
            "activeProfile": s.active_profile,
            "axisTargets": AXIS_TARGETS,
            "triggerLabels": trigger_labels(self.family()),
            "buttons": BUTTONS,
            "actions": ACTIONS,
            "continuous": CONTINUOUS,
            "labels": ACTIONS.iter().map(|a| (*a, action_label(a))).collect::<std::collections::BTreeMap<_, _>>(),
            "buttonLabels": BUTTONS.iter().map(|b| (*b, button_label_for(b, self.family()))).collect::<std::collections::BTreeMap<_, _>>(),
            "version": env!("CARGO_PKG_VERSION"),
            "engine": self.engine_version,
            "profileDir": if s.login_mode == "private" && self.private { self.profile.display().to_string() } else { persistent_profile_dir().display().to_string() },
            "runningLoginMode": self.running_login_mode,
        });
        let _ = self.ui.evaluate_script(&format!("window.sp && sp.state({state})"));
    }

    /// Remember which monitor StreetPad is on (used at the next start).
    fn save_monitor(&self) {
        let Some(name) = self.window.current_monitor().and_then(|m| m.name()) else { return };
        if let Ok(mut s) = self.shared.settings.write() {
            if s.monitor != name {
                s.monitor = name;
                s.save();
            }
        }
    }

    fn save_location(&mut self, force: bool) {
        let Ok(url) = self.main.url() else { return };
        if !is_google_url(&url) || url == self.last_saved_url {
            return;
        }
        if !force && !url.contains("/maps") {
            return;
        }
        if let Ok(mut s) = self.shared.settings.write() {
            s.last_url = url.clone();
            s.save();
        }
        self.last_saved_url = url;
    }

    /// Frozen picture after an A click (the view at the click stays on screen):
    /// - the click walked somewhere -> unfreeze at once (show the walk);
    /// - a place panel opened (or Google turned towards a place) -> stay frozen, nothing moves,
    ///   until the next controller input resets to the view at the click;
    /// - nothing happened -> unfreeze after 1.5 s.
    fn freeze_tick(&mut self) {
        let Some(f) = self.freeze.as_mut() else { return };
        if f.held {
            return;
        }
        let age = f.since.elapsed();
        let url = self.main.url().unwrap_or_default();
        let done = match (steer::parse_view(&url), geo::position(&url)) {
            (Some(v), Some(p)) => {
                let turned = ((v.heading - f.heading + 540.0) % 360.0 - 180.0).abs() > 2.0;
                if geo::distance_m(p, f.pos) > 1.0 {
                    true // the click walked somewhere: show it
                } else if turned || url != f.url && url.contains("/place/") {
                    f.held = true; // a place got selected: wait for the next controller input
                    false
                } else {
                    age > Duration::from_millis(1500)
                }
            }
            _ => true, // left Street View
        };
        if done {
            self.unfreeze();
        }
    }

    /// Close the place panel without a reload: tell Google to switch to the click-time Street View
    /// address minus the place (like Back/Forward). Safety net: if the next steps land in the
    /// place's photo gallery (Google keeps a bit of the place), fall back to the clean reload.
    fn close_place(&mut self, click_url: String) {
        let mut clean = click_url.clone();
        if let Some(i) = clean.find("/place/") {
            let rest = &clean[i + 7..];
            let end = rest.find('/').map(|j| i + 7 + j).unwrap_or(clean.len());
            clean.replace_range(i..end, "");
        }
        if let Some(i) = clean.find("!4m") {
            let end = clean[i..].find('?').map(|j| i + j).unwrap_or(clean.len());
            clean.replace_range(i..end, "");
        }
        let js = format!("window.__spClosePlace && __spClosePlace({})", serde_json::to_string(&clean).unwrap_or_default());
        self.driver.eval(&js);
        self.place_watch = Some((Instant::now(), click_url));
    }

    /// Safety net after `close_place`: landing in a place photo gallery ("!1e2" in the address)
    /// within 10 s means Google still had the place: reopen cleanly at the click-time view.
    fn place_watch_tick(&mut self, url: &str) {
        let Some((since, _)) = &self.place_watch else { return };
        if since.elapsed() > Duration::from_secs(10) {
            self.place_watch = None;
            return;
        }
        if url.contains("!1e2") {
            let (_, click_url) = self.place_watch.take().unwrap();
            self.reopen_without_place(Some(click_url));
        }
    }

    /// Reopen Street View at the same photo, direction, tilt and zoom without the selected
    /// place (closes the side panel reliably; Google's Back could go back to the map).
    fn reopen_without_place(&mut self, view_url: Option<String>) {
        let url = view_url.unwrap_or_else(|| self.main.url().unwrap_or_default());
        let (Some(v), Some((lat, lng))) = (steer::parse_view(&url), geo::position(&url)) else { return };
        let pano = url.split("!1s").nth(1).and_then(|r| r.split('!').next()).filter(|p| !p.is_empty() && p.len() < 100);
        let mut target = format!(
            "https://www.google.com/maps/@?api=1&map_action=pano&viewpoint={lat:.7},{lng:.7}&heading={:.2}&pitch={:.2}&fov={:.0}",
            v.heading,
            (v.tilt - 90.0).clamp(-90.0, 90.0),
            v.fov.clamp(10.0, 100.0)
        );
        if let Some(p) = pano {
            target.push_str(&format!("&pano={}", url_encode(p)));
        }
        let _ = self.main.load_url(&target);
    }

    fn unfreeze(&mut self) {
        if self.freeze.take().is_some() {
            self.driver.eval("window.__spFreeze = false");
        }
    }

    fn periodic(&mut self) {
        self.freeze_tick();
        if Instant::now() >= self.next_mode_check {
            self.next_mode_check = Instant::now() + Duration::from_millis(250);
            let url = self.main.url().unwrap_or_default();
            // Google's photo address (used when opening/reopening Street View) is Street View even
            // before Google has rewritten it into the normal @lat,lng,...h,...t form.
            let sv = steer::parse_view(&url).is_some() || url.contains("map_action=pano");
            let was_sv = self.shared.street_view.load(Ordering::Relaxed);
            self.shared.street_view.store(sv, Ordering::Relaxed);
            self.driver.set_street_view(sv);
            if sv && !was_sv {
                self.shared.haptic("sv_enter");
            }
            if !sv && was_sv {
                self.shared.haptic("sv_leave");
            }
            if sv {
                self.last_sv_pos = geo::position(&url);
                self.sv_fallback_at = None;
                // Tell the user when the nearest photo is noticeably away from where they aimed.
                if let (Some((aim, at)), Some(pos)) = (self.sv_target, self.last_sv_pos) {
                    if at.elapsed() < Duration::from_secs(15) {
                        let d = geo::distance_m(aim, pos);
                        if d > 30.0 {
                            self.toast(&format!("Nearest Street View photo is {} m from the crosshair", d.round()));
                        }
                    }
                    self.sv_target = None;
                }
            } else if was_sv {
                // Left Street View (any way): put the crosshair on the exit spot.
                self.exit_target = self.last_sv_pos.map(|p| (p, Instant::now()));
                self.exit_reloaded = false;
                if self.immersive {
                    self.immersive = false;
                    self.driver.immersive = false;
                    self.driver.eval("window.__spImmersive && __spImmersive(false); location.reload()");
                }
            }
            if !sv {
                self.place_exit_crosshair(&url);
            }
            self.place_watch_tick(&url);
            self.driver.keep_page_focused();
            if self.shared.keyboard.load(Ordering::Relaxed) {
                // If the page changed underneath (real mouse/keyboard), leave keyboard mode.
                let proxy = self.proxy.clone();
                self.driver.eval_with("!!(window.__spKb && __spKb.isOpen())", move |r| {
                    if r.trim() != "true" {
                        let _ = proxy.send_event(UserEvent::KeyboardGone);
                    }
                });
            }
            // Page loads reset the page script's state: re-assert the crosshair mode.
            self.driver.eval("window.__spFollow && __spFollow(false)");
            self.driver.eval(&format!("window.__spDarkCrosshair && __spDarkCrosshair({})", !sv));
        }
        if self.steer.busy() {
            let main = &self.main;
            let read_url = || main.url().ok();
            self.steer.tick(&mut self.driver, &read_url);
        } else {
            self.driver.tick();
        }
        if self.sv_fallback_at.is_some_and(|t| Instant::now() >= t) {
            self.sv_fallback_at = None;
            if !self.shared.street_view.load(Ordering::Relaxed) {
                if let Some((target, _)) = self.sv_target {
                    self.open_street_view_at(target);
                    self.toast("Opening the nearest Street View photo");
                }
            }
        }
        if self.immersive && Instant::now() >= self.next_immersive_apply {
            // Page loads (bookmarks, Street View entry) reset the page; re-apply quietly.
            self.next_immersive_apply = Instant::now() + Duration::from_secs(2);
            self.driver.eval("window.__spImmersive && __spImmersive(true)");
        }
        if self.coverage_cleanup && !self.sv_hold && self.coverage_off_at.is_some_and(|t| Instant::now() >= t) {
            // Keep retrying (every 1.5 s) until we're on the map page where the switch exists.
            self.coverage_off_at = Some(Instant::now() + Duration::from_millis(1500));
            if !self.driver_street_view() {
                self.coverage_tries = self.coverage_tries.saturating_sub(1);
                if self.coverage_tries == 0 {
                    self.coverage_cleanup = false; // give up (switch not available on this page)
                }
                let proxy = self.proxy.clone();
                self.driver.eval_with(&control::coverage_js(false), move |result| {
                    if result.contains("changed") || result.contains("already") {
                        let _ = proxy.send_event(UserEvent::CoverageClean);
                    }
                });
            }
        }
        while self.debug_actions.first().is_some_and(|(at, _, _)| Instant::now() >= *at) {
            let (_, kind, arg) = self.debug_actions.remove(0);
            let name: &'static str = Box::leak(arg.clone().into_boxed_str());
            let ev = match kind.as_str() {
                "action" => Some(InputEvent::Action(name)),
                "step" => Some(InputEvent::Step(arg != "back")),
                "hold_end" => Some(InputEvent::HoldEnd(name)),
                "zoom" => arg.parse().ok().map(InputEvent::Zoom),
                "kbnav" => Some(InputEvent::KbNav(name)),
                "close_place" => {
                    // Test hook: close the current place exactly like the panel reset does.
                    let url = self.main.url().unwrap_or_default();
                    self.close_place(url);
                    None
                }
                _ => None,
            };
            if let Some(ev) = ev {
                self.on_input(ev);
                if self.quit_requested {
                    self.save_monitor();
                    self.save_location(true);
                    std::process::exit(0);
                }
            }
        }
        if let Some((js, start, next)) = self.debug_sample.as_mut() {
            if Instant::now() >= *next {
                *next = Instant::now() + Duration::from_millis(500);
                let t = start.elapsed().as_millis();
                let url = self.main.url().unwrap_or_default();
                self.driver.eval_with(&js.clone(), move |v| {
                    use std::io::Write;
                    let line = serde_json::json!({ "t": t, "url": url, "v": v }).to_string();
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("streetpad-samples.txt")) {
                        let _ = writeln!(f, "{line}");
                    }
                });
            }
        }
        if self.debug_js_ui.as_ref().is_some_and(|(at, _)| Instant::now() >= *at) {
            let (_, js) = self.debug_js_ui.take().unwrap();
            Driver::new(self.ui.webview()).eval_await(&js, |result| {
                let _ = std::fs::write(std::env::temp_dir().join("streetpad-debug-ui.txt"), result);
            });
        }
        while self.debug_cdp.first().is_some_and(|(at, _, _)| Instant::now() >= *at) {
            let (_, method, params) = self.debug_cdp.remove(0);
            self.driver.cdp_raw(&method, params);
        }
        if self.debug_js.as_ref().is_some_and(|(at, _)| Instant::now() >= *at) {
            // Diagnostics: run the script and write its result to %TEMP%\streetpad-debug.txt.
            let (_, js) = self.debug_js.take().unwrap();
            self.driver.eval_await(&js, |result| {
                let _ = std::fs::write(std::env::temp_dir().join("streetpad-debug.txt"), result);
            });
        }
        if self.guide_shown_at.is_some_and(|t| t.elapsed() > Duration::from_secs(12)) {
            self.show_guide(false);
        }
        if let Some((at, _)) = &self.pending_toast {
            if Instant::now() >= *at {
                let (_, text) = self.pending_toast.take().unwrap();
                self.toast(&text);
            }
        }
        if Instant::now() >= self.next_url_save {
            self.next_url_save = Instant::now() + Duration::from_secs(10);
            self.save_location(false);
        }
    }

    /// Controller in use: hide the pointer. Re-applied every few seconds while the controller
    /// keeps being used, because page navigations reset it.
    fn hide_cursor(&mut self) {
        let due = self.cursor_hidden_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2));
        if due && !self.ui_visible() {
            let crosshair = self.shared.settings.read().map(|s| s.crosshair).unwrap_or(true);
            let (x, y) = self.driver.aim();
            self.driver.eval(&format!("window.__spCursor && __spCursor(true, {crosshair}, false, {x}, {y})"));
            self.cursor_hidden_at = Some(Instant::now());
        }
    }

    /// Family of the controller in use (button names, default preset).
    fn family(&self) -> &'static str {
        self.active.as_ref().map(|d| d.family).unwrap_or("xbox")
    }

    fn device_identity(&self) -> (Option<String>, String) {
        (self.active.as_ref().map(|d| d.key.clone()), self.family().to_string())
    }

    /// A controller was connected/selected: load the profile last used with it, else the
    /// preset for its family (like games do).
    fn load_profile_for_device(&mut self) {
        let Some(dev) = self.active.clone() else { return };
        let msg = {
            let Ok(mut s) = self.shared.settings.write() else { return };
            // No profile remembered for this controller but a personal profile is active (e.g.
            // edited before the pad connected): keep it and remember it for this controller.
            if !s.device_profiles.contains_key(&dev.key) && s.active_profile.starts_with("user:") {
                let id = s.active_profile.clone();
                s.device_profiles.insert(dev.key.clone(), id);
                s.save();
                return;
            }
            let id = s.device_profiles.get(&dev.key).cloned().unwrap_or_else(|| format!("preset:{}", dev.family));
            let Some(p) = s.find_profile(&id).or_else(|| s.find_profile(&format!("preset:{}", dev.family))) else { return };
            if s.active_profile == p.id {
                return;
            }
            s.apply_profile(&p);
            s.save();
            format!("{} connected · profile: {}", family_name(dev.family), p.name)
        };
        self.toast(&msg);
    }

    fn toast(&self, text: &str) {
        let js = format!("window.__spToast && __spToast({})", serde_json::to_string(text).unwrap_or_default());
        self.driver.eval(&js);
    }

    /// In Street View: leave it (Esc). On the map: open Street View at the crosshair, which is
    /// the map centre, using Google's official Maps URL for panoramas.
    fn toggle_street_view(&mut self) {
        let url = self.main.url().unwrap_or_default();
        if steer::parse_view(&url).is_some() {
            // Leave Street View by opening the map at the current spot (street-level zoom).
            // More reliable than Esc, which Street View may use for other things.
            self.steer.cancel(&mut self.driver);
            if self.immersive {
                // Immersive is for Street View only; the map loads normally.
                self.immersive = false;
                self.driver.immersive = false;
                self.driver.eval("window.__spImmersive && __spImmersive(false)");
            }
            let spot = url.split("/@").nth(1).and_then(|r| {
                let mut it = r.split(',');
                Some((it.next()?.parse::<f64>().ok()?, it.next()?.parse::<f64>().ok()?))
            });
            match spot {
                Some((lat, lng)) => {
                    let _ = self.main.load_url(&format!("https://www.google.com/maps/@{lat:.7},{lng:.7},19z"));
                }
                None => self.driver.eval("history.back()"),
            }
            return;
        }
        match self.crosshair_latlng(&url) {
            Some(target) => self.open_street_view_at(target),
            None => {
                self.toast("Move the map a little first, then try again");
                self.shared.haptic("nope");
            }
        }
    }

    /// Coordinates under the crosshair (exact Web Mercator); the map centre for satellite/3D.
    fn crosshair_latlng(&self, url: &str) -> Option<(f64, f64)> {
        match geo::map_view(url) {
            Some(view) => Some(geo::screen_to_latlng(view, self.driver.cursor, self.driver.size)),
            None => geo::position(url),
        }
    }

    /// Open the Street View photo nearest to these coordinates (Google's official pano URL).
    fn open_street_view_at(&mut self, (lat, lng): (f64, f64)) {
        let _ = self.main.load_url(&format!(
            "https://www.google.com/maps/@?api=1&map_action=pano&viewpoint={lat:.7},{lng:.7}"
        ));
        self.sv_target = Some(((lat, lng), Instant::now()));
    }

    /// Hold Y. In Street View: leave it. On the map: show Google's blue Street View lines;
    /// both sticks keep working (pan / aim) until Y is released.
    fn street_view_hold_start(&mut self) {
        let url = self.main.url().unwrap_or_default();
        if steer::parse_view(&url).is_some() {
            self.toggle_street_view();
            return;
        }
        self.sv_hold = true;
        self.coverage_off_at = None;
        let proxy = self.proxy.clone();
        self.driver.eval_with(&control::coverage_js(true), move |result| {
            let _ = proxy.send_event(UserEvent::CoverageOn(result.contains("changed")));
        });
        self.toast("Aim at a blue line and release Y");
    }

    /// Y released: click the blue line under the crosshair, which opens Street View there.
    /// The coverage layer is switched off again shortly after (or once back on the map).
    fn street_view_hold_end(&mut self) {
        if !std::mem::take(&mut self.sv_hold) {
            return;
        }
        let url = self.main.url().unwrap_or_default();
        self.sv_target = self.crosshair_latlng(&url).map(|t| (t, Instant::now()));
        self.driver.click();
        // Released just beside a blue line? Then open the nearest photo by coordinates.
        self.sv_fallback_at = Some(Instant::now() + Duration::from_millis(1800));
        self.coverage_off_at = Some(Instant::now() + Duration::from_millis(1500));
    }

    /// After leaving Street View: put the map crosshair exactly on the spot where you were.
    /// If that spot is off-screen, centre the map on it first.
    fn place_exit_crosshair(&mut self, url: &str) {
        let Some((spot, since)) = self.exit_target else { return };
        if since.elapsed() > Duration::from_secs(10) {
            self.exit_target = None;
            return;
        }
        let Some(view) = geo::map_view(url) else { return }; // map not ready yet
        let (w, h) = self.driver.size;
        let (x, y) = geo::latlng_to_screen(view, spot, (w, h));
        // The crosshair is the centre: centre the map on the exit spot (unless it already is).
        if (x - w / 2.0).abs() < 8.0 && (y - h / 2.0).abs() < 8.0 {
            self.exit_target = None;
        } else if !self.exit_reloaded {
            // Re-centre once; the timeout above ends the attempt if the page never gets there.
            let (lat, lng) = spot;
            self.exit_reloaded = true;
            let _ = self.main.load_url(&format!("https://www.google.com/maps/@{lat:.7},{lng:.7},{}z", view.zoom.max(17.0)));
        }
    }

    /// Name (as printed on the controller) of the button whose tap does `action`.
    fn button_for_action(&self, action: &str) -> String {
        let fam = self.family();
        self.shared
            .settings
            .read()
            .ok()
            .and_then(|s| BUTTONS.iter().find(|b| s.action_for(b) == action).map(|b| button_label_for(b, fam).to_string()))
            .unwrap_or_else(|| "the same button".into())
    }

    fn driver_street_view(&self) -> bool {
        self.shared.street_view.load(Ordering::Relaxed)
    }

    /// Open the on-screen search keyboard (controller types; normal control paused).
    fn open_keyboard(&mut self) {
        self.steer.cancel(&mut self.driver);
        self.driver.release();
        self.show_guide(false);
        let recent = self.shared.settings.read().map(|s| s.recent_searches.clone()).unwrap_or_default();
        let json = serde_json::to_string(&recent).unwrap_or_else(|_| "[]".into());
        self.shared.keyboard.store(true, Ordering::Relaxed);
        let proxy = self.proxy.clone();
        let fam = self.family();
        let labels = serde_json::json!({ "start": button_label_for("start", fam), "back": button_label_for("back", fam), "y": button_label_for("y", fam) });
        self.driver.eval_with(&format!("window.__spKb ? __spKb.open({json}, {labels}) : false"), move |r| {
            if r.trim() != "true" {
                let _ = proxy.send_event(UserEvent::KeyboardGone);
            }
        });
    }

    fn close_keyboard(&mut self) {
        self.shared.keyboard.store(false, Ordering::Relaxed);
        self.driver.eval("window.__spKb && __spKb.close()");
    }

    /// Search Google Maps (official search URL) and remember the query.
    fn search(&mut self, q: &str) {
        if q.is_empty() {
            return;
        }
        if let Ok(mut s) = self.shared.settings.write() {
            s.recent_searches.retain(|x| !x.eq_ignore_ascii_case(q));
            s.recent_searches.insert(0, q.to_string());
            s.recent_searches.truncate(settings::MAX_RECENT_SEARCHES);
            s.save();
        }
        self.shared.haptic("search_sent");
        let _ = self.main.load_url(&format!("https://www.google.com/maps/search/{}", url_encode(q)));
    }

    /// Immersive mode: only the picture. Hides Google's interface and stops Google from
    /// painting text (street names, place icons) into the picture. A quick reload of the same
    /// view applies it to labels that were already drawn.
    fn set_immersive(&mut self, on: bool) {
        self.immersive = on;
        self.driver.immersive = on;
        self.driver.park_now();
        self.shared.haptic(if on { "immersive_on" } else { "immersive_off" });
        self.steer.cancel(&mut self.driver);
        self.driver.release();
        self.driver.eval(&format!("window.__spImmersive && __spImmersive({on}); location.reload()"));
        self.next_immersive_apply = Instant::now() + Duration::from_millis(2500);
        let key = self.button_for_action("immersive");
        let msg = if on { format!("Immersive mode: picture only ({key} to exit)") } else { "Immersive mode off".to_string() };
        self.pending_toast = Some((Instant::now() + Duration::from_millis(2200), msg.into()));
    }

    fn show_guide(&mut self, show: bool) {
        if !show {
            if self.guide_shown_at.take().is_some() {
                self.driver.eval("window.__spGuide && __spGuide(null)");
            }
            return;
        }
        let Ok(s) = self.shared.settings.read() else { return };
        let mut rows: Vec<(String, String)> = vec![
            ("Left stick".into(), if s.swap_sticks { "Look around".into() } else if s.left_x_mode == "turn" { "Walk ↑↓ · turn ←→".into() } else { "Walk ↑↓".into() }),
            ("Right stick".into(), if s.swap_sticks { "Walk".into() } else { "Look around".into() }),
            ({ let (l, r) = trigger_labels(self.family()); format!("{r} / {l}") }, "Zoom in / out".into()),
        ];
        for b in BUTTONS {
            let (tap, hold) = (s.action_for(b), s.hold_action_for(b));
            if tap == "none" && hold == "none" {
                continue;
            }
            let mut txt = action_label(tap).to_string();
            if hold != "none" {
                txt.push_str(&format!("  ·  hold: {}", action_label(hold)));
            }
            rows.push((button_label_for(b, self.family()).into(), txt));
        }
        drop(s);
        let json = serde_json::to_string(&rows).unwrap_or_default();
        self.driver.eval(&format!("window.__spGuide && __spGuide({json})"));
        self.guide_shown_at = Some(Instant::now());
    }

    fn snap_angle(&self) -> f64 {
        self.shared.settings.read().map(|s| s.snap_angle as f64).unwrap_or(90.0)
    }

    fn turn(&mut self, deg: f64) {
        let url = self.main.url().unwrap_or_default();
        if let Err(StartError::NotStreetView) = self.steer.turn(deg, &url) {
            self.toast("Turning works in Street View");
            self.shared.haptic("nope");
        } else {
            self.shared.haptic(if deg.abs() >= 179.0 { "turn_around" } else if deg > 0.0 { "turn_right" } else { "turn_left" });
        }
    }

    /// A readable name for the current place: page title without the " - Google Maps" suffix,
    /// falling back to the coordinates from the URL.
    fn place_name(&self, url: &str) -> String {
        let mut name = self.title.trim().to_string();
        for suffix in [" - Google Maps", " – Google Maps", " — Google Maps"] {
            if let Some(stripped) = name.strip_suffix(suffix) {
                name = stripped.trim().to_string();
            }
        }
        if name.is_empty() || name == "Google Maps" {
            name = url
                .split("/@")
                .nth(1)
                .map(|r| r.split(',').take(2).collect::<Vec<_>>().join(", "))
                .unwrap_or_else(|| "Bookmark".into());
        }
        name.chars().take(120).collect()
    }

    fn bookmark_save(&mut self) {
        let Ok(url) = self.main.url() else { return };
        if !is_google_url(&url) || !url.contains("/@") {
            self.toast("Go to a place first, then save it");
            self.shared.haptic("nope");
            return;
        }
        let name = self.place_name(&url);
        let msg = {
            let Ok(mut s) = self.shared.settings.write() else { return };
            // Same spot = same coordinates (camera direction may differ).
            let spot = |u: &str| u.split("/@").nth(1).map(|r| r.split(',').take(2).collect::<Vec<_>>().join(","));
            if let Some(i) = s.bookmarks.iter().position(|b| spot(&b.url) == spot(&url)) {
                s.bookmarks[i].url = url.clone(); // keep the latest view direction
                s.bookmark_pos = i;
                s.save();
                self.shared.haptic("bm_exists");
                format!("★ Already saved ({}/{}): {}", i + 1, s.bookmarks.len(), s.bookmarks[i].name)
            } else if s.bookmarks.len() >= MAX_BOOKMARKS {
                self.shared.haptic("nope");
                format!("Bookmark list is full ({MAX_BOOKMARKS}). Delete some in settings.")
            } else {
                s.bookmarks.push(Bookmark { name: name.clone(), url });
                s.bookmark_pos = s.bookmarks.len() - 1;
                s.save();
                self.shared.haptic("bm_saved");
                format!("★ Saved bookmark {}: {name}", s.bookmarks.len())
            }
        };
        self.toast(&msg);
    }

    fn bookmark_step(&mut self, dir: i64) {
        let target = {
            let Ok(mut s) = self.shared.settings.write() else { return };
            let n = s.bookmarks.len();
            if n == 0 {
                None
            } else {
                // From a non-bookmarked place, "next" starts at the first and "previous" at the last.
                let here = geo::position(&self.main.url().unwrap_or_default());
                let on_bookmark = s.bookmarks.get(s.bookmark_pos).and_then(|b| geo::position(&b.url)).zip(here)
                    .is_some_and(|(a, b)| geo::distance_m(a, b) < 25.0);
                let pos = if on_bookmark {
                    (s.bookmark_pos as i64 + dir).rem_euclid(n as i64) as usize
                } else if dir > 0 { 0 } else { n - 1 };
                s.bookmark_pos = pos;
                s.save();
                Some((pos, n, s.bookmarks[pos].clone()))
            }
        };
        match target {
            None => {
                self.toast("No bookmarks yet: press X to save this place");
                self.shared.haptic("nope");
            }
            Some((pos, n, bm)) => {
                self.shared.haptic("bm_jump");
                self.steer.cancel(&mut self.driver);
                let _ = self.main.load_url(&bm.url);
                self.pending_toast = Some((Instant::now() + Duration::from_millis(1200), format!("★ {}/{}: {}", pos + 1, n, bm.name)));
            }
        }
    }

    /// The physical mouse moved/clicked/scrolled: show the pointer again (the crosshair stays
    /// centred and is not moved by the mouse).
    fn on_real_mouse(&mut self) {
        if self.cursor_hidden_at.take().is_some() {
            self.driver.eval("window.__spCursor && __spCursor(false, false)");
        }
    }

    fn on_input(&mut self, ev: InputEvent) {
        let ui = self.ui_visible();
        if !ui && matches!(ev, InputEvent::Look(..) | InputEvent::Step(_) | InputEvent::Zoom(_) | InputEvent::ZoomSmooth(_) | InputEvent::Action(_)) {
            self.hide_cursor();
        }
        // Any controller use closes the on-screen button guide (but not the press that opened it).
        if self.guide_shown_at.is_some_and(|t| t.elapsed() > Duration::from_millis(400))
            && matches!(ev, InputEvent::Look(..) | InputEvent::Step(_) | InputEvent::Zoom(_) | InputEvent::ZoomSmooth(_) | InputEvent::Action(_))
        {
            self.show_guide(false);
        }
        // Shop panel open: any controller input closes it (reopens Street View at the same spot
        // without the place); that input does nothing else.
        let controller_input = matches!(
            ev,
            InputEvent::Look(..) | InputEvent::Step(_) | InputEvent::Zoom(_) | InputEvent::ZoomSmooth(_) | InputEvent::Action(_) | InputEvent::HoldEnd(_)
        );
        let held = self.freeze.as_ref().is_some_and(|f| f.held);
        let recently_reset = self.last_reset.is_some_and(|t| t.elapsed() < Duration::from_secs(4));
        if held && !ui && controller_input && !recently_reset {
            self.last_reset = Some(Instant::now());
            self.steer.cancel(&mut self.driver);
            self.driver.release();
            let click_view = self.freeze.take().map(|f| f.url).unwrap_or_else(|| self.main.url().unwrap_or_default());
            self.driver.eval("window.__spFreeze = false");
            self.close_place(click_view);
            return;
        }
        // Looking around or walking ends a freeze at once.
        if matches!(ev, InputEvent::Look(..) | InputEvent::Step(_)) && self.freeze.is_some() {
            self.steer.cancel(&mut self.driver);
            self.unfreeze();
        }
        // Manual look/walk takes over from an automatic turn.
        if !ui && matches!(ev, InputEvent::Look(..) | InputEvent::Step(_)) {
            self.steer.cancel(&mut self.driver);
        }
        match ev {
            InputEvent::Look(dx, dy) if !ui => self.driver.look(dx, dy),
            InputEvent::Step(fwd) if !ui => {
                // Walking closes Google's place side panel (turning doesn't). The movement that
                // closes it doesn't step; walking continues with the next step.
                self.driver.step(fwd);
                if self.immersive {
                    self.driver.park_now();
                }
            }
            InputEvent::Zoom(n) if !ui => self.driver.zoom(n),
            InputEvent::ZoomSmooth(px) if !ui => self.driver.zoom_smooth(px),
            InputEvent::Action(a) => match a {
                "settings" => self.show_ui(!ui),
                "close_ui" => self.show_ui(false),
                "fullscreen" => self.toggle_fullscreen(),
                "click" if !ui => {
                    // In Street View, freeze the picture: if Google turns the camera towards a
                    // selected place, it is turned back unseen before the picture runs again.
                    let url = self.main.url().unwrap_or_default();
                    if let (Some(v), Some(p)) = (steer::parse_view(&url), geo::position(&url)) {
                        self.driver.eval("window.__spFreeze = true");
                        self.freeze = Some(Freeze { since: Instant::now(), url: url.clone(), pos: p, heading: v.heading, held: false });
                    }
                    self.driver.click();
                    self.shared.haptic("click");
                }
                "turn_around" if !ui => self.turn(180.0),
                "snap_left" if !ui => self.turn(-self.snap_angle()),
                "snap_right" if !ui => self.turn(self.snap_angle()),
                "level_view" if !ui => {
                    let url = self.main.url().unwrap_or_default();
                    if let Err(StartError::NotStreetView) = self.steer.level(&url) {
                        self.toast("Level view works in Street View");
                        self.shared.haptic("nope");
                    } else {
                        self.shared.haptic("level");
                    }
                }
                "street_view" if !ui => self.toggle_street_view(),
                "street_view_hold" if !ui => self.street_view_hold_start(),
                "immersive" if !ui => {
                    let now_url = self.main.url().unwrap_or_default();
                    let on_map = steer::parse_view(&now_url).is_none() && !now_url.contains("map_action=pano");
                    if !self.immersive && on_map {
                        self.quit_requested = true; // View on the map quits StreetPad
                    } else {
                        self.set_immersive(!self.immersive);
                    }
                }
                "guide" if !ui => self.show_guide(true),
                "search" if !ui => self.open_keyboard(),
                "auto_walk_on" if !ui => {
                    self.toast("Auto-walk on: pull back or press again to stop");
                    self.shared.haptic("walk_on");
                }
                "auto_walk_off" if !ui => {
                    self.toast("Auto-walk off");
                    self.shared.haptic("walk_off");
                }
                "bookmark_save" if !ui => self.bookmark_save(),
                "bookmark_next" if !ui => self.bookmark_step(1),
                "bookmark_prev" if !ui => self.bookmark_step(-1),
                "browser_back" if !ui => self.driver.eval("history.back()"),
                "escape" if !ui => self.driver.escape(),
                "reload" if !ui => {
                    let _ = self.main.reload();
                }
                _ => {}
            },
            InputEvent::HoldEnd("street_view") => self.street_view_hold_end(),
            InputEvent::KbNav(cmd) => {
                if cmd == "close" {
                    self.close_keyboard();
                } else {
                    self.driver.eval(&format!("window.__spKb && __spKb.nav('{cmd}')"));
                }
            }
            InputEvent::FocusMe => self.bring_to_front(),
            InputEvent::UiNav(cmd) => {
                if ui {
                    if cmd == "close" {
                        self.show_ui(false);
                    } else {
                        let _ = self.ui.evaluate_script(&format!("window.sp && sp.nav('{cmd}')"));
                    }
                }
            }
            InputEvent::Pad(json) => {
                if ui {
                    let _ = self.ui.evaluate_script(&format!("window.sp && sp.pad({json})"));
                }
            }
            InputEvent::Devices(list) => {
                self.devices = list;
                if ui {
                    self.push_state();
                }
            }
            InputEvent::Active(dev) => {
                let changed_key = dev.as_ref().map(|d| &d.key) != self.active.as_ref().map(|d| &d.key);
                self.active = dev;
                if changed_key {
                    self.load_profile_for_device();
                }
                self.update_title();
                if ui {
                    self.push_state();
                }
            }
            InputEvent::Captured(target, raw) => {
                let (key, family) = self.device_identity();
                let created = self.shared.settings.write().ok().and_then(|mut s| {
                    s.remap.inputs.insert(target.clone(), raw);
                    let c = s.store_controls(key.as_deref(), &family);
                    s.save();
                    c
                });
                self.push_state();
                let note = created.map(|n| format!("Saved in new profile \"{n}\"")).unwrap_or_default();
                let _ = self.ui.evaluate_script(&format!(
                    "window.sp && sp.captured({}, {})",
                    serde_json::to_string(&target).unwrap_or_default(),
                    serde_json::to_string(&note).unwrap_or_default()
                ));
            }
            _ => {}
        }
    }

    /// Handles an IPC message. Returns true when the app should exit.
    fn on_ipc(&mut self, from_ui: bool, body: &str) -> bool {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(body) else { return false };
        let cmd = msg.get("cmd").and_then(|c| c.as_str()).unwrap_or("");

        if !from_ui {
            // The Google page may only toggle these two things.
            match cmd {
                "toggleSettings" => self.show_ui(!self.ui_visible()),
                "fullscreen" => self.toggle_fullscreen(),
                "kbClose" => self.shared.keyboard.store(false, Ordering::Relaxed),
                "kbSearch" if self.shared.keyboard.load(Ordering::Relaxed) => {
                    let q = msg.get("q").and_then(|v| v.as_str()).unwrap_or_default().trim().chars().take(200).collect::<String>();
                    self.shared.keyboard.store(false, Ordering::Relaxed);
                    self.search(&q);
                }
                _ => {}
            }
            return false;
        }

        match cmd {
            "ready" => self.push_state(),
            "close" => self.show_ui(false),
            "fullscreen" => self.toggle_fullscreen(),
            "save" => {
                let Some(new) = msg.get("settings").and_then(|v| serde_json::from_value::<Settings>(v.clone()).ok()) else {
                    return false;
                };
                let (key, family) = self.device_identity();
                let created = {
                    let Ok(mut s) = self.shared.settings.write() else { return false };
                    let mut new = new;
                    // Fields owned by the app, not by the settings page.
                    new.last_url = s.last_url.clone();
                    new.profiles = s.profiles.clone();
                    new.device_profiles = s.device_profiles.clone();
                    new.active_profile = s.active_profile.clone();
                    new.remap = s.remap.clone();
                    new.sanitize();
                    let controls_changed = new.controls() != s.controls();
                    *s = new;
                    let created = if controls_changed { s.store_controls(key.as_deref(), &family) } else { None };
                    s.save();
                    created
                };
                if let Some(name) = created {
                    // Editing a preset made a personal copy: show it in the profile list.
                    self.push_state();
                    let _ = self.ui.evaluate_script(&format!(
                        "window.sp && sp.note({})",
                        serde_json::to_string(&format!("Presets stay unchanged: your changes are saved in the new profile \"{name}\".")).unwrap_or_default()
                    ));
                }
                self.cursor_hidden_at = None; // re-apply pointer/crosshair state on next controller use
                self.update_title();
            }
            "reset" => {
                // Reset the active profile's controls to the preset defaults (remap kept).
                let (key, family) = self.device_identity();
                if let Ok(mut s) = self.shared.settings.write() {
                    let remap = s.remap.clone();
                    let id = s.active_profile.clone();
                    s.apply_profile(&Profile { id, remap, ..Profile::default() });
                    if s.active_profile.starts_with("user:") {
                        s.store_controls(key.as_deref(), &family);
                    }
                    s.save();
                }
                self.push_state();
            }
            "profileSelect" => {
                let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let key = self.device_identity().0;
                if let Ok(mut s) = self.shared.settings.write() {
                    if let Some(p) = s.find_profile(&id) {
                        s.apply_profile(&p);
                        if let Some(k) = key {
                            s.device_profiles.insert(k, id);
                        }
                        s.save();
                    }
                }
                self.push_state();
            }
            "profileNew" => {
                let (key, family) = self.device_identity();
                if let Ok(mut s) = self.shared.settings.write() {
                    if s.profiles.len() < MAX_PROFILES {
                        let (id, name) = s.new_profile_identity(&format!("My {} profile", family_name(&family)));
                        let p = Profile { id: id.clone(), name, family, controls: s.controls(), remap: s.remap.clone() };
                        s.profiles.push(p);
                        s.active_profile = id.clone();
                        if let Some(k) = key {
                            s.device_profiles.insert(k, id);
                        }
                        s.save();
                    }
                }
                self.push_state();
            }
            "profileRename" => {
                let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                let name: String = msg.get("name").and_then(|v| v.as_str()).unwrap_or_default().trim().chars().take(60).collect();
                if let Ok(mut s) = self.shared.settings.write() {
                    if let Some(p) = s.profiles.iter_mut().find(|p| p.id == id) {
                        if !name.is_empty() {
                            p.name = name;
                        }
                    }
                    s.save();
                }
                self.push_state();
            }
            "profileDelete" => {
                let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let family = self.family();
                if let Ok(mut s) = self.shared.settings.write() {
                    s.profiles.retain(|p| p.id != id);
                    s.device_profiles.retain(|_, v| *v != id);
                    if s.active_profile == id {
                        if let Some(p) = s.find_profile(&format!("preset:{family}")) {
                            s.apply_profile(&p);
                        }
                    }
                    s.save();
                }
                self.push_state();
            }
            "remapStart" => {
                let target = msg.get("target").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if let Ok(mut c) = self.shared.capture.lock() {
                    *c = Some(target);
                }
            }
            "remapCancel" => {
                if let Ok(mut c) = self.shared.capture.lock() {
                    *c = None;
                }
            }
            "remapClear" => {
                let (key, family) = self.device_identity();
                if let Ok(mut s) = self.shared.settings.write() {
                    s.remap = Default::default();
                    s.store_controls(key.as_deref(), &family);
                    s.save();
                }
                self.push_state();
            }
            "clearData" => {
                let _ = self.main.clear_all_browsing_data();
                let _ = self.main.load_url(MAPS_HOME);
            }
            "bookmarkGo" => {
                let idx = msg.get("index").and_then(|v| v.as_u64()).unwrap_or(u64::MAX) as usize;
                let url = self.shared.settings.write().ok().and_then(|mut s| {
                    let bm = s.bookmarks.get(idx)?.clone();
                    s.bookmark_pos = idx;
                    s.save();
                    Some(bm.url)
                });
                if let Some(url) = url {
                    let _ = self.main.load_url(&url);
                        self.show_ui(false);
                }
            }
            "home" => {
                let _ = self.main.load_url(MAPS_HOME);
                self.show_ui(false);
            }
            "restart" => {
                RESTART.store(true, Ordering::SeqCst);
                return true;
            }
            _ => {}
        }
        false
    }
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<UserEvent>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_queries_are_url_encoded() {
        assert_eq!(url_encode("Dam Square"), "Dam%20Square");
        assert_eq!(url_encode("café & co/1"), "caf%C3%A9%20%26%20co%2F1");
    }

    #[test]
    fn only_google_urls_are_remembered() {
        assert!(is_google_url("https://www.google.com/maps/@52,4,15z"));
        assert!(is_google_url("https://www.google.nl/maps"));
        assert!(!is_google_url("https://evil.example/maps"));
        assert!(!is_google_url("http://www.google.com/maps"));
    }

    #[test]
    fn coverage_script_targets_the_layers_switch() {
        let on = control::coverage_js(true);
        assert!(on.contains("const want = true"));
        assert!(on.contains("layerswitcher.intent.streetview"));
        assert!(control::coverage_js(false).contains("const want = false"));
    }
}
