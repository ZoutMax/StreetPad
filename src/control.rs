//! Drives Street View inside the Google Maps page by sending trusted input events through the
//! WebView2 DevTools protocol (Input.dispatchMouseEvent / dispatchKeyEvent). These behave like
//! real mouse and keyboard input, so it works with the normal, signed-in Google Maps site.
//!
//! - Looking around = a continuous left-button drag on the panorama.
//! - Walking        = Arrow Up / Arrow Down key presses (Street View's own keyboard controls).
//! - Zoom           = mouse wheel at the centre of the view.

use serde_json::{Value, json};
use std::time::{Duration, Instant};
use webview2_com::{CallDevToolsProtocolMethodCompletedHandler, ExecuteScriptCompletedHandler};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
use windows::core::HSTRING;

/// Movement needed before a drag starts, so a drag can never be mistaken for a click
/// (a click in Street View walks to the clicked spot).
const DRAG_START_PX: f64 = 8.0;
/// Delay between moving the pointer under the crosshair and clicking.
const CLICK_DELAY: Duration = Duration::from_millis(110);
/// Release the drag after the stick has been idle this long.
const DRAG_IDLE_RELEASE: Duration = Duration::from_millis(140);
/// Drag stays within this fraction of the smaller window dimension around the centre.
const DRAG_RADIUS_FRACTION: f64 = 0.28;

/// Make sure keyboard input (arrow-key steps) reaches the panorama: leave text fields and give
/// focus to the 3D picture (after a page load nothing has focus and Google ignores the keys).
const FOCUS_SCENE_JS: &str = r#"(() => {
  const a = document.activeElement;
  if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA' || a.isContentEditable)) a.blur();
  if (document.activeElement && document.activeElement.tagName === 'CANVAS') return;
  let best = null, area = 0;
  for (const c of document.querySelectorAll('canvas')) {
    const r = c.getBoundingClientRect();
    if (r.width * r.height > area) { area = r.width * r.height; best = c; }
  }
  if (best) { if (!best.hasAttribute('tabindex')) best.setAttribute('tabindex', '-1'); best.focus({ preventScroll: true }); }
})()"#;

pub struct Driver {
    wv: ICoreWebView2,
    /// Current pointer position of an active drag (CSS px).
    drag: Option<(f64, f64)>,
    pending: (f64, f64),
    last_look: Instant,
    /// Viewport size in CSS px.
    pub size: (f64, f64),
    /// In Street View (small, even drag range) vs the map (wide drag range, fewer re-grabs).
    street_view: bool,
    /// Map-mode crosshair position (CSS px). In Street View the crosshair is the centre.
    pub cursor: (f64, f64),
    /// Immersive mode: park the hidden pointer outside the window when idle, so Google
    /// doesn't draw its gray ground marker with the walking arrow.
    pub immersive: bool,
    /// A click waiting for Google to notice what is under the pointer.
    pending_click: Option<Instant>,
    /// When to bring the hidden pointer back under the crosshair after a click.
    pointer_back_at: Option<Instant>,
}

/// Shows (`want = true`) or hides Google's blue Street View coverage lines with the
/// "Street View" switch of the Layers menu. Google sometimes ignores the first click (the menu
/// loads lazily), so the state is re-checked and the click retried a few times.
/// Returns "changed", "already" or "missing".
pub fn coverage_js(want: bool) -> String {
    format!(
        r#"(() => {{
  const want = {want};
  const sw = () => document.querySelector('button[jsaction*="layerswitcher.intent.streetview"]');
  const on = () => {{ const b = sw(); return !!b && (b.getAttribute('aria-checked') || b.getAttribute('aria-pressed')) === 'true'; }};
  clearInterval(window.__spCovTimer);
  if (!sw()) return 'missing';
  if (on() === want) return 'already';
  sw().click();
  let tries = 0;
  window.__spCovTimer = setInterval(() => {{
    if (on() === want || ++tries > 5) {{ clearInterval(window.__spCovTimer); return; }}
    sw().click();
  }}, 700);
  return 'changed';
}})()"#
    )
}

impl Driver {
    pub fn new(wv: ICoreWebView2) -> Self {
        Self { wv, drag: None, pending: (0.0, 0.0), last_look: Instant::now(), size: (1280.0, 800.0), street_view: true, cursor: (640.0, 400.0), immersive: false, pending_click: None, pointer_back_at: None }
    }

    fn cdp(&self, method: &str, params: Value) {
        let method = HSTRING::from(method);
        let params = HSTRING::from(params.to_string());
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(())));
        unsafe {
            let _ = self.wv.CallDevToolsProtocolMethod(&method, &params, &handler);
        }
    }

    pub fn eval(&self, js: &str) {
        let js = HSTRING::from(js);
        let handler = ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(())));
        unsafe {
            let _ = self.wv.ExecuteScript(&js, &handler);
        }
    }

    /// Evaluate a script through DevTools (awaits promises); `done` gets the raw CDP JSON reply.
    pub fn eval_await(&self, js: &str, done: impl FnOnce(String) + 'static) {
        let method = HSTRING::from("Runtime.evaluate");
        let params = HSTRING::from(json!({ "expression": js, "awaitPromise": true, "returnByValue": true }).to_string());
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |_, result| {
            done(result);
            Ok(())
        }));
        unsafe {
            let _ = self.wv.CallDevToolsProtocolMethod(&method, &params, &handler);
        }
    }

    /// Diagnostics: send a raw DevTools command.
    pub fn cdp_raw(&self, method: &str, params: Value) {
        self.cdp(method, params);
    }

    /// Keep the page acting focused even when StreetPad is in the background, so Google Maps
    /// keeps accepting zoom (wheel) and keyboard input from the controller.
    pub fn keep_page_focused(&self) {
        self.cdp("Emulation.setFocusEmulationEnabled", json!({ "enabled": true }));
    }

    /// Run a script and get its JSON-encoded result back on the UI thread.
    pub fn eval_with(&self, js: &str, done: impl FnOnce(String) + 'static) {
        let js = HSTRING::from(js);
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |_, result| {
            done(result);
            Ok(())
        }));
        unsafe {
            let _ = self.wv.ExecuteScript(&js, &handler);
        }
    }

    fn mouse(&self, kind: &str, (x, y): (f64, f64), buttons: u8) {
        self.cdp(
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": "left", "buttons": buttons, "clickCount": 1 }),
        );
    }

    fn center(&self) -> (f64, f64) {
        (self.size.0 / 2.0, self.size.1 / 2.0)
    }

    /// Where clicks, hovering and zoom happen: the free crosshair on the map, the centre in
    /// Street View.
    pub fn aim(&self) -> (f64, f64) {
        self.center() // the crosshair is always centred (map and Street View)
    }

    /// Where the hidden pointer rests between actions.
    fn park(&self) -> (f64, f64) {
        if self.immersive { (-10.0, -10.0) } else { self.aim() }
    }

    /// Move the hidden pointer to its resting place now.
    pub fn park_now(&self) {
        if self.drag.is_none() {
            self.mouse("mouseMoved", self.park(), 0);
        }
    }

    /// Switch between Street View (centred crosshair) and map (free crosshair).
    pub fn set_street_view(&mut self, sv: bool) {
        if sv == self.street_view {
            return;
        }
        self.release();
        self.street_view = sv;
        self.cursor = self.center();
        self.eval("window.__spFollow && __spFollow(false)");
        self.mouse("mouseMoved", self.park(), 0);
    }

    /// Continue (or start) the look drag by (dx, dy) CSS pixels.
    pub fn look(&mut self, dx: f64, dy: f64) {
        self.pending_click = None; // looking around cancels a click that hasn't happened yet
        self.last_look = Instant::now();
        let c = self.center();
        match self.drag {
            None => {
                self.pending.0 += dx;
                self.pending.1 += dy;
                let threshold = if self.street_view { DRAG_START_PX } else { 4.0 };
                if self.pending.0.hypot(self.pending.1) < threshold {
                    return;
                }
                let p = (c.0 + self.pending.0, c.1 + self.pending.1);
                self.pending = (0.0, 0.0);
                self.mouse("mousePressed", c, 1);
                self.mouse("mouseMoved", p, 1);
                self.drag = Some(p);
            }
            Some(p) if !self.street_view => {
                // Map: use most of the screen, away from Google's buttons at the edges. When
                // the drag reaches the edge of that area, re-grab on the opposite side so the
                // next stretch is as long as possible (few re-grabs = smooth panning).
                let np = (p.0 + dx, p.1 + dy);
                self.mouse("mouseMoved", np, 1);
                let (w, h) = self.size;
                let (x0, x1, y0, y1) = (w * 0.15, w * 0.85, h * 0.2, h * 0.8);
                if np.0 < x0 || np.0 > x1 || np.1 < y0 || np.1 > y1 {
                    self.mouse("mouseReleased", np, 0);
                    let len = dx.hypot(dy).max(1e-6);
                    let (ux, uy) = (dx / len, dy / len);
                    let start = (
                        (c.0 - ux * w * 0.33).clamp(x0, x1),
                        (c.1 - uy * h * 0.27).clamp(y0, y1),
                    );
                    let nudge = (start.0 + ux * DRAG_START_PX, start.1 + uy * DRAG_START_PX);
                    self.mouse("mousePressed", start, 1);
                    self.mouse("mouseMoved", nudge, 1);
                    self.drag = Some(nudge);
                } else {
                    self.drag = Some(np);
                }
            }
            Some(p) => {
                let np = (p.0 + dx, p.1 + dy);
                self.mouse("mouseMoved", np, 1);
                let radius = self.size.0.min(self.size.1) * DRAG_RADIUS_FRACTION;
                if (np.0 - c.0).hypot(np.1 - c.1) > radius {
                    // Ran out of room: release and re-grab at the centre, already moved a few px
                    // in the same direction so the new grab is a drag, never a click.
                    self.mouse("mouseReleased", np, 0);
                    let len = dx.hypot(dy).max(1e-6);
                    let nudge = (c.0 + dx / len * DRAG_START_PX, c.1 + dy / len * DRAG_START_PX);
                    self.mouse("mousePressed", c, 1);
                    self.mouse("mouseMoved", nudge, 1);
                    self.drag = Some(nudge);
                } else {
                    self.drag = Some(np);
                }
            }
        }
    }

    /// Automatic turns: low-level drag control (no re-grabs, no fling).
    pub fn sweep_press(&mut self, p: (f64, f64)) {
        self.release();
        self.mouse("mouseMoved", p, 0);
        self.mouse("mousePressed", p, 1);
    }

    pub fn sweep_move(&mut self, p: (f64, f64)) {
        self.mouse("mouseMoved", p, 1);
    }

    pub fn sweep_release(&mut self, p: (f64, f64)) {
        self.mouse("mouseReleased", p, 0);
        self.mouse("mouseMoved", self.park(), 0);
    }

    pub fn size(&self) -> (f64, f64) {
        self.size
    }

    /// Release the look drag if it is active, then park the (hidden) pointer under the
    /// crosshair so Google's hover effects follow the aim point.
    pub fn release(&mut self) {
        if let Some(p) = self.drag.take() {
            self.mouse("mouseReleased", p, 0);
            self.mouse("mouseMoved", self.park(), 0);
        }
        self.pending = (0.0, 0.0);
    }

    /// Left-click whatever is under the crosshair.
    /// The pointer goes under the crosshair first; the click follows ~110 ms later (in `tick`)
    /// so Google has noticed what is under the pointer (e.g. a place badge) by then. Clicking in
    /// the same instant made the click land on the picture behind the badge.
    pub fn click(&mut self) {
        self.release();
        self.mouse("mouseMoved", self.aim(), 0);
        self.pending_click = Some(Instant::now() + CLICK_DELAY);
    }

    fn do_click(&mut self) {
        let c = self.aim();
        self.mouse("mouseMoved", c, 0);
        self.mouse("mousePressed", c, 1);
        self.mouse("mouseReleased", c, 0);
        // Move the pointer off the picture right away: after a click Google turns the camera
        // towards whatever the pointer is on (the "slide" when clicking badges and walls).
        // In normal mode it comes back under the crosshair after a moment (floor marker).
        self.mouse("mouseMoved", (-10.0, -10.0), 0);
        if !self.immersive {
            self.pointer_back_at = Some(Instant::now() + Duration::from_millis(3000));
        }
    }

    /// Call regularly: ends the drag when the stick has gone idle.
    pub fn tick(&mut self) {
        if self.pending_click.is_some_and(|t| Instant::now() >= t) {
            self.pending_click = None;
            self.do_click();
        }
        if self.pointer_back_at.is_some_and(|t| Instant::now() >= t) {
            self.pointer_back_at = None;
            self.park_now();
        }
        if self.drag.is_some() && self.last_look.elapsed() > DRAG_IDLE_RELEASE {
            self.release();
        }
        if self.drag.is_none() && self.last_look.elapsed() > DRAG_IDLE_RELEASE {
            self.pending = (0.0, 0.0);
        }
    }

    fn key(&self, key: &str, vk: u32) {
        let base = json!({ "key": key, "code": key, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk });
        let mut down = base.clone();
        down["type"] = json!("rawKeyDown");
        let mut up = base;
        up["type"] = json!("keyUp");
        self.cdp("Input.dispatchKeyEvent", down);
        self.cdp("Input.dispatchKeyEvent", up);
    }

    pub fn step(&mut self, forward: bool) {
        self.release();
        self.eval(FOCUS_SCENE_JS);
        if forward { self.key("ArrowUp", 0x26) } else { self.key("ArrowDown", 0x28) }
    }

    pub fn escape(&mut self) {
        self.release();
        self.key("Escape", 0x1B);
    }

    /// Analog zoom: a stream of small pixel wheel deltas, like a touchpad, for smooth zooming.
    pub fn zoom_smooth(&mut self, px: f64) {
        let c = self.aim();
        self.cdp(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": c.0, "y": c.1, "deltaX": 0, "deltaY": -px, "pointerType": "mouse" }),
        );
    }

    pub fn zoom(&mut self, notches: i32) {
        let c = self.aim();
        self.cdp(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": c.0, "y": c.1, "deltaX": 0, "deltaY": -100 * notches }),
        );
    }
}
