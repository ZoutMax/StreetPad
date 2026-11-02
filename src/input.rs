//! Controller input thread.
//!
//! Reads the selected controller (XInput for Xbox pads, gilrs / Windows.Gaming.Input for
//! generic pads), applies dead zone, curve and button bindings, and emits high-level
//! [`InputEvent`]s to the UI thread, which drives Street View.

use crate::settings::{ACTIONS, AXIS_TARGETS, BUTTONS, CONTINUOUS, RawInput, Remap, Settings};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::UI::Input::XboxController::{
    XINPUT_STATE, XINPUT_VIBRATION, XInputGetState, XInputSetState,
};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

const TICK: Duration = Duration::from_millis(8);
const XINPUT_PROBE_EVERY: Duration = Duration::from_millis(1500);
const DEVICE_SCAN_EVERY: Duration = Duration::from_millis(500);
const PREVIEW_EVERY: Duration = Duration::from_millis(33);
/// Press length that counts as a hold.
const HOLD_TIME: Duration = Duration::from_millis(500);
/// Stick deflection needed before forward/back movement starts.
const MOVE_THRESHOLD: f32 = 0.45;

/// State shared between the UI thread and the input thread.
pub struct Shared {
    pub settings: RwLock<Settings>,
    /// Settings panel visible: controller drives the panel instead of Street View.
    pub ui_visible: AtomicBool,
    /// Top-level window handle, used to check whether StreetPad is in the foreground.
    pub hwnd: AtomicIsize,
    /// Current look speed in CSS px/s (two f32s packed), applied once per display frame by
    /// the pacer thread so motion is smooth at any refresh rate.
    pub look_vel: AtomicU64,
    /// Set by the UI thread: Street View (look/walk) vs map (free crosshair/pan).
    pub street_view: AtomicBool,
    /// Remap wizard: target waiting for a physical input ("a", "lx", ...), or None.
    pub capture: std::sync::Mutex<Option<String>>,
    /// On-screen search keyboard open: the controller types instead of moving.
    pub keyboard: AtomicBool,
    /// Vibration patterns requested by the UI thread (played by the controller thread).
    pub haptics: std::sync::Mutex<Vec<&'static str>>,
}

fn pack(x: f32, y: f32) -> u64 {
    ((x.to_bits() as u64) << 32) | y.to_bits() as u64
}

fn unpack(v: u64) -> (f32, f32) {
    (f32::from_bits((v >> 32) as u32), f32::from_bits(v as u32))
}

impl Shared {
    pub fn set_look_vel(&self, x: f32, y: f32) {
        self.look_vel.store(pack(x, y), Ordering::Relaxed);
    }

    pub fn look_vel(&self) -> (f32, f32) {
        unpack(self.look_vel.load(Ordering::Relaxed))
    }

    /// Request a vibration pattern by name (see `pattern`).
    pub fn haptic(&self, name: &'static str) {
        if let Ok(mut q) = self.haptics.lock() {
            q.push(name);
        }
    }

    fn stop_motion(&self) {
        self.set_look_vel(0.0, 0.0);
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// Backend: "xinput" or "generic".
    pub kind: &'static str,
    /// Controller family: "xbox", "playstation", "nintendo" or "generic".
    pub family: &'static str,
    /// Stable key to remember this controller's profile ("xinput" or "gilrs:<uuid>").
    pub key: String,
}

#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Drag the panorama by this many CSS pixels.
    Look(f64, f64),
    /// One step forward (true) or back (false).
    Step(bool),
    /// Zoom notches (+ in, - out).
    Zoom(i32),
    /// Smooth analog zoom, in wheel pixels (+ in, - out).
    ZoomSmooth(f64),
    /// One-shot action name from settings::ACTIONS (or "close_ui").
    Action(&'static str),
    /// Live controller state for the settings panel preview (JSON).
    Pad(String),
    Devices(Vec<DeviceInfo>),
    /// A hold action's button was released (used by "street_view": drop Pegman on release).
    HoldEnd(&'static str),
    /// The controller is being used while another window is in front: bring StreetPad forward.
    FocusMe,
    /// Search keyboard: up, down, left, right, activate, backspace, space, submit, close.
    KbNav(&'static str),
    /// Settings-page navigation: up, down, left, right, activate, prev_section, next_section, close.
    UiNav(&'static str),
    /// Name of the controller currently in use, or None.
    Active(Option<DeviceInfo>),
    /// Remap wizard: the physical input captured for a target.
    Captured(String, RawInput),
}

#[derive(Clone, Copy, Default, Debug)]
struct PadState {
    lx: f32,
    ly: f32,
    rx: f32,
    ry: f32,
    lt: f32,
    rt: f32,
    /// Bit i = BUTTONS[i] pressed.
    buttons: u16,
}

impl PadState {
    fn pressed(&self, idx: usize) -> bool {
        self.buttons & (1 << idx) != 0
    }
}

// XInput button bits, in the same order as settings::BUTTONS.
const XINPUT_BITS: [u16; 14] = [
    0x1000, 0x2000, 0x4000, 0x8000, // a b x y
    0x0100, 0x0200, // lb rb
    0x0020, 0x0010, // back start
    0x0040, 0x0080, // ls rs
    0x0001, 0x0002, 0x0004, 0x0008, // dpad up down left right
];

const GILRS_BUTTONS: [gilrs::Button; 14] = {
    use gilrs::Button::*;
    [
        South, East, West, North, LeftTrigger, RightTrigger, Select, Start, LeftThumb,
        RightThumb, DPadUp, DPadDown, DPadLeft, DPadRight,
    ]
};

fn from_xinput(s: &XINPUT_STATE) -> PadState {
    let g = &s.Gamepad;
    let axis = |v: i16| (v as f32 / 32767.0).clamp(-1.0, 1.0);
    let mut buttons = 0u16;
    for (i, bit) in XINPUT_BITS.iter().enumerate() {
        if g.wButtons.0 & bit != 0 {
            buttons |= 1 << i;
        }
    }
    PadState {
        lx: axis(g.sThumbLX),
        ly: axis(g.sThumbLY),
        rx: axis(g.sThumbRX),
        ry: axis(g.sThumbRY),
        lt: g.bLeftTrigger as f32 / 255.0,
        rt: g.bRightTrigger as f32 / 255.0,
        buttons,
    }
}

/// Guess the controller family from the USB vendor id and the name.
fn family_of(gp: &gilrs::Gamepad) -> &'static str {
    let name = gp.name().to_lowercase();
    match gp.vendor_id() {
        Some(0x054C) => return "playstation",
        Some(0x057E) => return "nintendo",
        Some(0x045E) => return "xbox",
        _ => {}
    }
    if name.contains("xbox") || name.contains("xinput") {
        "xbox"
    } else if ["dualsense", "dualshock", "ps4", "ps5", "playstation"].iter().any(|k| name.contains(k)) {
        "playstation"
    } else if ["pro controller", "joy-con", "nintendo", "switch"].iter().any(|k| name.contains(k)) {
        "nintendo"
    } else {
        "generic"
    }
}

/// Raw snapshot of every physical button and axis: (code, is_axis, value).
fn raw_snapshot(gp: &gilrs::Gamepad) -> Vec<(u32, bool, f32)> {
    let st = gp.state();
    let mut v: Vec<(u32, bool, f32)> = st.buttons().map(|(c, d)| (c.into_u32(), false, d.value())).collect();
    v.extend(st.axes().map(|(c, d)| (c.into_u32(), true, d.value())));
    v
}

fn raw_value(raw: &[(u32, bool, f32)], input: &RawInput) -> f32 {
    let v = raw.iter().find(|(c, a, _)| *c == input.code && *a == input.axis).map(|x| x.2).unwrap_or(input.rest);
    if !input.axis {
        return v;
    }
    let span = input.full - input.rest;
    if span.abs() < 1e-3 { 0.0 } else { (v - input.rest) / span }
}

fn from_gilrs(gp: &gilrs::Gamepad, remap: &Remap) -> PadState {
    use gilrs::{Axis, Button};
    let mut buttons = 0u16;
    for (i, b) in GILRS_BUTTONS.iter().enumerate() {
        if gp.is_pressed(*b) {
            buttons |= 1 << i;
        }
    }
    let trig = |b: Button| gp.button_data(b).map(|d| d.value()).unwrap_or(0.0).clamp(0.0, 1.0);
    let mut s = PadState {
        lx: gp.value(Axis::LeftStickX),
        ly: gp.value(Axis::LeftStickY),
        rx: gp.value(Axis::RightStickX),
        ry: gp.value(Axis::RightStickY),
        lt: trig(Button::LeftTrigger2),
        rt: trig(Button::RightTrigger2),
        buttons,
    };
    if remap.inputs.is_empty() {
        return s;
    }
    // Custom remap from the setup wizard overrides the built-in mapping per control.
    let raw = raw_snapshot(gp);
    for (i, b) in BUTTONS.iter().enumerate() {
        if let Some(inp) = remap.inputs.get(*b) {
            let pressed = if inp.axis { raw_value(&raw, inp) > 0.5 } else { raw_value(&raw, inp) > 0.5 };
            if pressed { s.buttons |= 1 << i } else { s.buttons &= !(1 << i) }
        }
    }
    for t in AXIS_TARGETS {
        if let Some(inp) = remap.inputs.get(*t) {
            let v = raw_value(&raw, inp);
            match *t {
                "lx" => s.lx = v.clamp(-1.0, 1.0),
                "ly" => s.ly = v.clamp(-1.0, 1.0),
                "rx" => s.rx = v.clamp(-1.0, 1.0),
                "ry" => s.ry = v.clamp(-1.0, 1.0),
                "lt" => s.lt = v.clamp(0.0, 1.0),
                "rt" => s.rt = v.clamp(0.0, 1.0),
                _ => {}
            }
        }
    }
    s
}

/// Vibration patterns: segments of (heavy left motor, light right motor, milliseconds), 0..1.
fn pattern(name: &str) -> &'static [(f32, f32, u64)] {
    match name {
        "step" => &[(1.0, 1.0, 120)],
        "step_run" => &[(1.0, 1.0, 70)],
        "click" => &[(0.0, 0.55, 35)],
        "turn_right" => &[(0.45, 0.10, 90), (0.30, 0.30, 90), (0.10, 0.45, 90)],
        "turn_left" => &[(0.10, 0.45, 90), (0.30, 0.30, 90), (0.45, 0.10, 90)],
        "turn_around" => &[(0.50, 0.10, 90), (0.40, 0.20, 90), (0.30, 0.30, 90), (0.20, 0.40, 90), (0.10, 0.50, 90)],
        "level" => &[(0.0, 0.40, 40), (0.0, 0.0, 60), (0.0, 0.40, 40)],
        "sv_enter" => &[(0.15, 0.10, 60), (0.35, 0.25, 80), (0.55, 0.40, 100), (0.30, 0.20, 80), (0.10, 0.05, 80)],
        "sv_leave" => &[(0.10, 0.05, 80), (0.30, 0.20, 80), (0.55, 0.40, 100), (0.35, 0.25, 80), (0.15, 0.10, 60)],
        "bm_saved" => &[(0.0, 0.50, 50), (0.0, 0.0, 60), (0.0, 0.50, 50)],
        "bm_exists" => &[(0.45, 0.0, 140)],
        "bm_jump" => &[(0.30, 0.30, 90)],
        "walk_on" => &[(0.15, 0.15, 50), (0.0, 0.0, 50), (0.35, 0.35, 60)],
        "walk_off" => &[(0.35, 0.0, 90)],
        "immersive_on" => &[(0.10, 0.10, 80), (0.25, 0.20, 120), (0.10, 0.10, 120)],
        "immersive_off" => &[(0.25, 0.20, 80), (0.10, 0.10, 120)],
        "hold" => &[(0.30, 0.60, 120)],
        "key" => &[(0.0, 0.20, 20)],
        "search_sent" => &[(0.0, 0.45, 45), (0.0, 0.0, 50), (0.0, 0.45, 45)],
        "ui_tick" => &[(0.0, 0.12, 15)],
        "nope" => &[(0.50, 0.0, 70), (0.0, 0.0, 60), (0.50, 0.0, 70)],
        "connected" => &[(0.30, 0.30, 150)],
        _ => &[],
    }
}

/// Radial dead zone + response curve. Returns a vector with magnitude 0..1.
fn shape(x: f32, y: f32, deadzone: f32, curve: f32) -> (f32, f32) {
    let m = (x * x + y * y).sqrt();
    if m <= deadzone || m == 0.0 {
        return (0.0, 0.0);
    }
    let n = ((m - deadzone) / (1.0 - deadzone)).min(1.0).powf(curve);
    (x / m * n, y / m * n)
}

fn gilrs_id(gp: &gilrs::Gamepad, ordinal: usize) -> String {
    let hex: String = gp.uuid().iter().map(|b| format!("{b:02x}")).collect();
    format!("gilrs:{hex}:{ordinal}")
}

fn foreground_is(hwnd: isize) -> bool {
    hwnd != 0 && unsafe { GetForegroundWindow() }.0 as isize == hwnd
}

/// Starts the controller thread and the frame pacer thread.
pub fn spawn(
    shared: Arc<Shared>,
    send: impl Fn(InputEvent) + Send + 'static,
    send_frame: impl Fn(InputEvent) + Send + 'static,
) {
    let sh = shared.clone();
    thread::Builder::new()
        .name("controller".into())
        .spawn(move || Worker::new(sh, Box::new(send)).run())
        .expect("spawn controller thread");

    // Pacer: wakes once per display refresh (DwmFlush waits for the next composition) and
    // sends exactly one look movement sized by the real frame time. Avoids the judder of
    // mixing the controller polling rate with the monitor refresh rate.
    thread::Builder::new()
        .name("look-pacer".into())
        .spawn(move || {
            let mut last = Instant::now();
            loop {
                if unsafe { DwmFlush() }.is_err() {
                    thread::sleep(Duration::from_millis(7));
                }
                let now = Instant::now();
                let dt = (now - last).as_secs_f32().min(0.05);
                last = now;
                let (vx, vy) = shared.look_vel();
                if vx != 0.0 || vy != 0.0 {
                    send_frame(InputEvent::Look((vx * dt) as f64, (vy * dt) as f64));
                }
            }
        })
        .expect("spawn pacer thread");
}

struct Worker {
    shared: Arc<Shared>,
    send: Box<dyn Fn(InputEvent) + Send>,
    gilrs: Option<gilrs::Gilrs>,
    xi_connected: [bool; 4],
    xi_state: [XINPUT_STATE; 4],
    xi_next_probe: [Instant; 4],
    devices: Vec<DeviceInfo>,
    next_scan: Instant,
    /// Id of the controller in use.
    active: Option<String>,
    prev_buttons: u16,
    /// Remap wizard: target being captured and the raw snapshot taken when it started.
    capture_base: Option<(String, Vec<(u32, bool, f32)>)>,
    next_focus_request: Instant,
    /// Auto-walk (cruise) active.
    cruise: bool,
    /// Settings-page navigation: direction held and when it repeats next.
    nav_dir: Option<&'static str>,
    nav_next: Instant,
    /// Press start for buttons that have a hold action (None once the hold has fired).
    hold_start: [Option<Instant>; 14],
    /// Buttons whose hold action fired and that are still down.
    hold_fired: u16,
    move_dir: i8,
    move_acc: f32,
    zoom_acc: f32,
    next_zoom_send: Instant,
    next_preview: Instant,
    /// Vibration pattern playing: XInput slot, segments, current index, end of segment.
    haptic: Option<(usize, &'static [(f32, f32, u64)], usize, Instant)>,
}

impl Worker {
    fn new(shared: Arc<Shared>, send: Box<dyn Fn(InputEvent) + Send>) -> Self {
        let now = Instant::now();
        Self {
            shared,
            send,
            gilrs: gilrs::Gilrs::new().ok(),
            xi_connected: [false; 4],
            xi_state: [XINPUT_STATE::default(); 4],
            xi_next_probe: [now; 4],
            devices: Vec::new(),
            next_scan: now,
            active: None,
            prev_buttons: 0,
            hold_start: [None; 14],
            hold_fired: 0,
            capture_base: None,
            next_focus_request: now,
            cruise: false,
            nav_dir: None,
            nav_next: now,
            move_dir: 0,
            move_acc: 0.0,
            zoom_acc: 0.0,
            next_zoom_send: now,
            next_preview: now,
            haptic: None,
        }
    }

    fn run(mut self) {
        let mut last = Instant::now();
        loop {
            thread::sleep(TICK);
            let now = Instant::now();
            let dt = (now - last).as_secs_f32().min(0.1);
            last = now;
            self.tick(now, dt);
        }
    }

    fn poll_hardware(&mut self, now: Instant) {
        if let Some(g) = self.gilrs.as_mut() {
            while g.next_event().is_some() {}
        }
        for i in 0..4 {
            // Polling an empty XInput slot is slow, so only probe empty slots occasionally.
            if self.xi_connected[i] || now >= self.xi_next_probe[i] {
                let mut st = XINPUT_STATE::default();
                let ok = unsafe { XInputGetState(i as u32, &mut st) } == 0;
                self.xi_connected[i] = ok;
                if ok {
                    self.xi_state[i] = st;
                } else {
                    self.xi_next_probe[i] = now + XINPUT_PROBE_EVERY;
                }
            }
        }
    }

    fn scan_devices(&mut self) {
        let mut list = Vec::new();
        for i in 0..4 {
            if self.xi_connected[i] {
                list.push(DeviceInfo {
                    id: format!("xinput:{i}"),
                    name: format!("Xbox controller #{} (XInput)", i + 1),
                    kind: "xinput",
                    family: "xbox",
                    key: "xinput".into(),
                });
            }
        }
        if let Some(g) = self.gilrs.as_ref() {
            let mut seen: Vec<[u8; 16]> = Vec::new();
            for (_, gp) in g.gamepads() {
                if !gp.is_connected() {
                    continue;
                }
                let ord = seen.iter().filter(|u| **u == gp.uuid()).count();
                seen.push(gp.uuid());
                let suffix = if ord > 0 { format!(" #{}", ord + 1) } else { String::new() };
                let hex: String = gp.uuid().iter().map(|b| format!("{b:02x}")).collect();
                list.push(DeviceInfo {
                    id: gilrs_id(&gp, ord),
                    name: format!("{}{suffix} (generic)", gp.name()),
                    kind: "generic",
                    family: family_of(&gp),
                    key: format!("gilrs:{hex}"),
                });
            }
        }
        if list != self.devices {
            self.devices = list.clone();
            (self.send)(InputEvent::Devices(list));
        }
    }

    /// Resolve the configured device to a live device and read its state.
    /// The gilrs gamepad behind a device id.
    fn gilrs_pad(&self, id: &str) -> Option<gilrs::Gamepad<'_>> {
        let g = self.gilrs.as_ref()?;
        let mut seen: Vec<[u8; 16]> = Vec::new();
        for (_, gp) in g.gamepads() {
            if !gp.is_connected() {
                continue;
            }
            let ord = seen.iter().filter(|u| **u == gp.uuid()).count();
            seen.push(gp.uuid());
            if gilrs_id(&gp, ord) == id {
                return Some(gp);
            }
        }
        None
    }

    fn read_selected(&self, wanted: &str, remap: &Remap) -> Option<(DeviceInfo, PadState)> {
        let dev = if wanted == "auto" {
            self.devices.first()?.clone()
        } else {
            self.devices.iter().find(|d| d.id == wanted)?.clone()
        };
        let state = if let Some(slot) = dev.id.strip_prefix("xinput:") {
            let i: usize = slot.parse().ok()?;
            if !self.xi_connected[i] {
                return None; // unplugged since the last device scan: don't reuse its last state
            }
            from_xinput(&self.xi_state[i])
        } else {
            from_gilrs(&self.gilrs_pad(&dev.id)?, remap)
        };
        Some((dev, state))
    }

    /// End every hold in progress, telling the UI thread (e.g. Y held for the blue lines).
    fn flush_holds(&mut self, s: &Settings) {
        for (i, b) in BUTTONS.iter().enumerate() {
            if self.hold_fired & (1 << i) != 0 {
                if let Some(name) = ACTIONS.iter().find(|x| **x == s.hold_action_for(b)) {
                    (self.send)(InputEvent::HoldEnd(name));
                }
            }
        }
        self.hold_fired = 0;
        self.hold_start = [None; 14];
    }

    /// Direction from D-pad or left stick with keyboard-like auto-repeat, sent to the settings
    /// page (`keyboard = false`) or the search keyboard.
    fn nav_repeat(&mut self, pad: &PadState, now: Instant, keyboard: bool) {
        let pressed = |name: &str| BUTTONS.iter().position(|b| *b == name).is_some_and(|i| pad.pressed(i));
        let dir = if pressed("dup") || pad.ly > 0.6 {
            Some("up")
        } else if pressed("ddown") || pad.ly < -0.6 {
            Some("down")
        } else if pressed("dleft") || pad.lx < -0.6 {
            Some("left")
        } else if pressed("dright") || pad.lx > 0.6 {
            Some("right")
        } else {
            None
        };
        let send = |w: &Self, d: &'static str| {
            w.shared.haptic("ui_tick");
            (w.send)(if keyboard { InputEvent::KbNav(d) } else { InputEvent::UiNav(d) })
        };
        if dir != self.nav_dir {
            self.nav_dir = dir;
            if let Some(d) = dir {
                send(self, d);
                self.nav_next = now + Duration::from_millis(380);
            }
        } else if let Some(d) = dir {
            if now >= self.nav_next {
                send(self, d);
                self.nav_next = now + Duration::from_millis(95);
            }
        }
    }

    /// Remap wizard step. Returns true while a capture is in progress (normal control paused).
    fn capture_tick(&mut self, dev: Option<&DeviceInfo>) -> bool {
        let wanted = self.shared.capture.lock().ok().and_then(|c| c.clone());
        let Some(target) = wanted else {
            self.capture_base = None;
            return false;
        };
        let Some(dev) = dev.filter(|d| d.kind == "generic") else { return true };
        let Some(gp) = self.gilrs_pad(&dev.id) else { return true };
        let now_raw = raw_snapshot(&gp);
        let base = match &self.capture_base {
            Some((t, b)) if *t == target => b.clone(),
            _ => {
                // New step: remember the resting state (anything already held is ignored).
                self.capture_base = Some((target.clone(), now_raw));
                return true;
            }
        };
        let rest_of = |code: u32, axis: bool| base.iter().find(|(c, a, _)| *c == code && *a == axis).map(|x| x.2);
        // A newly pressed button wins; otherwise an axis that moved far from rest.
        let mut found: Option<RawInput> = None;
        for (code, axis, v) in &now_raw {
            let rest = rest_of(*code, *axis).unwrap_or(if *axis { 0.0 } else { 0.0 });
            if !axis && *v > 0.5 && rest <= 0.5 {
                found = Some(RawInput { code: *code, axis: false, rest: 0.0, full: 1.0 });
                break;
            }
            if *axis && (v - rest).abs() > 0.6 && found.is_none() {
                // gilrs axes span -1..1: use the end of the range the user pushed towards.
                found = Some(RawInput { code: *code, axis: true, rest, full: if *v > rest { 1.0 } else { -1.0 } });
            }
        }
        if let Some(inp) = found {
            if let Ok(mut c) = self.shared.capture.lock() {
                *c = None;
            }
            self.capture_base = None;
            (self.send)(InputEvent::Captured(target, inp));
        }
        true
    }

    fn set_motors(slot: usize, left: f32, right: f32, scale: f32) {
        let m = |v: f32| ((v * scale).clamp(0.0, 1.0) * 65535.0) as u16;
        let v = XINPUT_VIBRATION { wLeftMotorSpeed: m(left), wRightMotorSpeed: m(right) };
        unsafe { XInputSetState(slot as u32, &v) };
    }

    /// Start a vibration pattern on the controller in use (XInput only). A new pattern
    /// replaces the one playing.
    fn play(&mut self, dev: &DeviceInfo, name: &str, now: Instant, s: &Settings) {
        if !s.vibration {
            return;
        }
        let Some(slot) = dev.id.strip_prefix("xinput:").and_then(|x| x.parse::<usize>().ok()) else { return };
        let segs = pattern(name);
        let Some(&(l, r, ms)) = segs.first() else { return };
        if let Some((old, ..)) = self.haptic {
            if old != slot {
                Self::set_motors(old, 0.0, 0.0, 1.0);
            }
        }
        Self::set_motors(slot, l, r, s.vibration_strength);
        self.haptic = Some((slot, segs, 0, now + Duration::from_millis(ms)));
    }

    /// Advance the vibration pattern; motors off when it ends.
    fn haptic_tick(&mut self, now: Instant, scale: f32) {
        let Some((slot, segs, idx, end)) = self.haptic else { return };
        if now < end {
            return;
        }
        let next = idx + 1;
        match segs.get(next) {
            Some(&(l, r, ms)) => {
                Self::set_motors(slot, l, r, scale);
                self.haptic = Some((slot, segs, next, now + Duration::from_millis(ms)));
            }
            None => {
                Self::set_motors(slot, 0.0, 0.0, 1.0);
                self.haptic = None;
            }
        }
    }

    /// Emit a one-shot action.
    fn fire(&mut self, action: &str) {
        match action {
            "none" => {}
            "auto_walk" => {
                self.cruise = !self.cruise;
                self.move_dir = 0; // first step comes right away
                (self.send)(InputEvent::Action(if self.cruise { "auto_walk_on" } else { "auto_walk_off" }));
            }
            "zoom_in" => (self.send)(InputEvent::Zoom(1)),
            "zoom_out" => (self.send)(InputEvent::Zoom(-1)),
            a => {
                if let Some(name) = ACTIONS.iter().find(|x| **x == a) {
                    (self.send)(InputEvent::Action(name));
                }
            }
        }
    }

    fn tick(&mut self, now: Instant, dt: f32) {
        self.poll_hardware(now);
        if now >= self.next_scan {
            self.next_scan = now + DEVICE_SCAN_EVERY;
            self.scan_devices();
        }

        let s = match self.shared.settings.read() {
            Ok(s) => s.clone(),
            Err(_) => return,
        };
        let selected = self.read_selected(&s.device, &s.remap);
        let active = selected.as_ref().map(|(d, _)| d.clone());
        if active.as_ref().map(|d| &d.id) != self.active.as_ref() {
            self.active = active.as_ref().map(|d| d.id.clone());
            if active.is_some() {
                self.shared.haptic("connected");
            }
            (self.send)(InputEvent::Active(active.clone()));
        }
        self.haptic_tick(now, s.vibration_strength);
        let queued: Vec<&'static str> = self.shared.haptics.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        if let Some(dev) = active.as_ref() {
            for name in queued {
                self.play(dev, name, now, &s);
            }
        }
        // Remap wizard: wait for a new physical input on the selected generic controller.
        if self.capture_tick(selected.as_ref().map(|(d, _)| d)) {
            self.shared.stop_motion();
            self.prev_buttons = selected.as_ref().map(|(_, p)| p.buttons).unwrap_or(0);
            return;
        }
        let Some((dev, pad)) = selected else {
            // Controller gone: end holds and auto-walk so nothing resumes by itself later.
            self.shared.stop_motion();
            self.flush_holds(&s);
            if self.cruise {
                self.cruise = false;
                (self.send)(InputEvent::Action("auto_walk_off"));
            }
            self.prev_buttons = 0;
            self.move_dir = 0;
            return;
        };

        let ui_visible = self.shared.ui_visible.load(Ordering::Relaxed);
        let focused = foreground_is(self.shared.hwnd.load(Ordering::Relaxed));
        // Controller in use while another window is in front: ask the UI thread to bring
        // StreetPad to the front (at most once per second).
        let in_use = pad.buttons != 0
            || pad.lt > 0.3 || pad.rt > 0.3
            || shape(pad.lx, pad.ly, s.deadzone, 1.0) != (0.0, 0.0)
            || shape(pad.rx, pad.ry, s.deadzone, 1.0) != (0.0, 0.0);
        if !focused && in_use && !s.only_when_focused && now >= self.next_focus_request {
            self.next_focus_request = now + Duration::from_secs(1);
            (self.send)(InputEvent::FocusMe);
        }

        if ui_visible && now >= self.next_preview {
            self.next_preview = now + PREVIEW_EVERY;
            let pressed: Vec<&str> =
                (0..BUTTONS.len()).filter(|i| pad.pressed(*i)).map(|i| BUTTONS[i]).collect();
            let json = serde_json::json!({
                "lx": pad.lx, "ly": pad.ly, "rx": pad.rx, "ry": pad.ry,
                "lt": pad.lt, "rt": pad.rt, "buttons": pressed,
            });
            (self.send)(InputEvent::Pad(json.to_string()));
        }

        if s.only_when_focused && !focused {
            self.shared.stop_motion();
            self.flush_holds(&s);
            if self.cruise {
                self.cruise = false;
                (self.send)(InputEvent::Action("auto_walk_off"));
            }
            self.prev_buttons = pad.buttons;
            self.move_dir = 0;
            self.zoom_acc = 0.0;
            return;
        }

        let just_pressed = pad.buttons & !self.prev_buttons;
        let just_released = !pad.buttons & self.prev_buttons;
        self.prev_buttons = pad.buttons;

        // Search keyboard open: D-pad/stick move, A types, B deletes, X space, Start searches,
        // Back closes.
        if self.shared.keyboard.load(Ordering::Relaxed) && !ui_visible {
            self.shared.stop_motion();
            self.cruise = false;
            self.flush_holds(&s);
            self.move_dir = 0;
            let jp = |name: &str| BUTTONS.iter().position(|b| *b == name).is_some_and(|i| just_pressed & (1 << i) != 0);
            for (btn, cmd) in [("a", "activate"), ("b", "backspace"), ("x", "space"), ("start", "submit"), ("back", "close"), ("y", "close")] {
                if jp(btn) {
                    if matches!(cmd, "activate" | "backspace" | "space") {
                        self.shared.haptic("key");
                    }
                    (self.send)(InputEvent::KbNav(cmd));
                }
            }
            self.nav_repeat(&pad, now, true);
            return;
        }
        // Settings page open: the controller navigates it.
        if ui_visible {
            self.shared.stop_motion();
            self.cruise = false;
            self.flush_holds(&s);
            self.move_dir = 0;
            let jp = |name: &str| BUTTONS.iter().position(|b| *b == name).is_some_and(|i| just_pressed & (1 << i) != 0);
            if jp("b") || jp("start") {
                (self.send)(InputEvent::UiNav("close"));
            } else if jp("a") {
                (self.send)(InputEvent::UiNav("activate"));
            } else if jp("lb") {
                (self.send)(InputEvent::UiNav("prev_section"));
            } else if jp("rb") {
                (self.send)(InputEvent::UiNav("next_section"));
            }
            self.nav_repeat(&pad, now, false);
            return;
        }
        self.nav_dir = None;

        // ---- buttons
        // Continuous actions act while held. One-shot actions fire on press, unless the button
        // also has a hold action: then a short press fires the tap action on release and a
        // long press fires the hold action once.
        let mut held_fwd = false;
        let mut held_back = false;
        let mut running = false;
        let mut turn = 0.0f32; // +1 = turn left
        let mut pitch = 0.0f32; // +1 = look up
        for i in 0..BUTTONS.len() {
            let bit = 1u16 << i;
            let tap = s.action_for(BUTTONS[i]);
            let hold = s.hold_action_for(BUTTONS[i]);
            if pad.pressed(i) {
                match tap {
                    "forward" => held_fwd = true,
                    "back" => held_back = true,
                    "run" => running = true,
                    "turn_left" => turn += 1.0,
                    "turn_right" => turn -= 1.0,
                    "look_up" => pitch += 1.0,
                    "look_down" => pitch -= 1.0,
                    _ => {}
                }
            }
            if CONTINUOUS.contains(&tap) {
                continue;
            }
            if hold == "none" {
                if just_pressed & bit != 0 {
                    self.fire(tap);
                }
                continue;
            }
            if just_pressed & bit != 0 {
                self.hold_start[i] = Some(now);
            }
            if just_released & bit != 0 && self.hold_fired & bit != 0 {
                self.hold_fired &= !bit;
                if let Some(name) = ACTIONS.iter().find(|x| **x == hold) {
                    (self.send)(InputEvent::HoldEnd(name));
                }
            }
            if let Some(t0) = self.hold_start[i] {
                if pad.pressed(i) && now - t0 >= HOLD_TIME {
                    self.hold_start[i] = None; // fired; the release only ends the hold
                    self.hold_fired |= bit;
                    if hold == "street_view" {
                        (self.send)(InputEvent::Action("street_view_hold"));
                    } else {
                        self.fire(hold);
                    }
                    self.play(&dev, "hold", now, &s);
                } else if just_released & bit != 0 {
                    self.hold_start[i] = None;
                    self.fire(tap);
                }
            }
        }

        let (move_stick, look_stick) = if s.swap_sticks {
            ((pad.rx, pad.ry), (pad.lx, pad.ly))
        } else {
            ((pad.lx, pad.ly), (pad.rx, pad.ry))
        };

        let street_view = self.shared.street_view.load(Ordering::Relaxed);
        let (lx, ly) = shape(look_stick.0, look_stick.1, s.deadzone, s.look_curve);
        let (mx, my) = shape(move_stick.0, move_stick.1, s.deadzone, s.look_curve);

        if !street_view {
            // ---- map: the crosshair stays centred; both sticks pan the map under it.
            // Stick right = see further east = drag the map left. Stick up = north = drag down.
            let (px, py) = ((lx + mx).clamp(-1.0, 1.0), (ly + my).clamp(-1.0, 1.0));
            self.shared.set_look_vel(-px * s.pan_speed, py * s.pan_speed);
            if self.cruise {
                self.cruise = false;
                (self.send)(InputEvent::Action("auto_walk_off"));
            }
            self.move_dir = 0;
            self.move_acc = 0.0;
        } else {
            // ---- Street View: look stick drags the panorama.
            let mut dx = -lx * s.look_speed_x; // stick right -> drag left -> view turns right
            let mut dy = ly * s.look_speed_y * if s.invert_y { -1.0 } else { 1.0 };
            if s.left_x_mode == "turn" {
                dx += -mx * s.look_speed_x;
            }
            dx += turn * s.look_speed_x * 0.6;
            dy += pitch * s.look_speed_y * 0.6;
            self.shared.set_look_vel(dx, dy);
        }

        // ---- movement: discrete Street View steps, repeated while held
        let raw_y = if street_view { move_stick.1 } else { 0.0 };
        // Pulling back (stick or button) cancels auto-walk.
        if self.cruise && (held_back || raw_y < -MOVE_THRESHOLD) {
            self.cruise = false;
            (self.send)(InputEvent::Action("auto_walk_off"));
        }
        let (held_fwd, held_back) = if street_view { (held_fwd, held_back) } else { (false, false) };
        let (dir, mag): (i8, f32) = if held_fwd || (self.cruise && raw_y <= MOVE_THRESHOLD) {
            (1, 1.0)
        } else if held_back {
            (-1, 1.0)
        } else if raw_y > MOVE_THRESHOLD {
            (1, raw_y)
        } else if raw_y < -MOVE_THRESHOLD {
            (-1, -raw_y)
        } else {
            (0, 0.0)
        };
        if dir != 0 {
            if dir != self.move_dir {
                self.move_acc = 1.0; // step immediately when movement starts
            } else {
                let speed = 0.4 + 0.6 * ((mag - MOVE_THRESHOLD) / (1.0 - MOVE_THRESHOLD)).clamp(0.0, 1.0);
                let rate = s.move_rate * speed * if running { s.run_multiplier } else { 1.0 };
                self.move_acc += rate * dt;
            }
            if self.move_acc >= 1.0 {
                self.move_acc -= 1.0;
                (self.send)(InputEvent::Step(dir > 0));
                if s.vibrate_on_step {
                    self.play(&dev, if running { "step_run" } else { "step" }, now, &s);
                }
            }
        } else {
            self.move_acc = 0.0;
        }
        self.move_dir = dir;

        // ---- zoom on triggers
        let z = (if pad.rt > 0.1 { pad.rt } else { 0.0 }) - (if pad.lt > 0.1 { pad.lt } else { 0.0 });
        // Smooth: small wheel deltas every tick, sent at ~60 Hz (zoom_rate notches/s, 100 px per notch).
        if z != 0.0 {
            let curved = z.signum() * z.abs().powf(1.5);
            self.zoom_acc += curved * s.zoom_rate * 100.0 * dt;
            if self.zoom_acc.abs() >= 2.0 && now >= self.next_zoom_send {
                (self.send)(InputEvent::ZoomSmooth(self.zoom_acc as f64));
                self.zoom_acc = 0.0;
                self.next_zoom_send = now + Duration::from_millis(16);
            }
        } else {
            self.zoom_acc = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_zone_and_curve() {
        assert_eq!(shape(0.1, 0.0, 0.2, 1.0), (0.0, 0.0));
        let (x, y) = shape(1.0, 0.0, 0.2, 1.7);
        assert!((x - 1.0).abs() < 1e-6 && y == 0.0);
        let (half, _) = shape(0.6, 0.0, 0.2, 1.0);
        assert!((half - 0.5).abs() < 1e-6, "linear between dead zone and edge");
        let (curved, _) = shape(0.6, 0.0, 0.2, 2.0);
        assert!(curved < half, "curve gives finer control near the centre");
    }

    #[test]
    fn vibration_patterns_exist_and_are_in_range() {
        let names = [
            "step", "step_run", "click", "turn_right", "turn_left", "turn_around", "level", "sv_enter", "sv_leave",
            "bm_saved", "bm_exists", "bm_jump", "walk_on", "walk_off", "immersive_on", "immersive_off",
            "hold", "key", "search_sent", "ui_tick", "nope", "connected",
        ];
        for n in names {
            let p = pattern(n);
            assert!(!p.is_empty(), "pattern {n} missing");
            for &(l, r, ms) in p {
                assert!((0.0..=1.0).contains(&l) && (0.0..=1.0).contains(&r) && ms > 0 && ms <= 500, "{n}");
            }
        }
        // Every name requested in the code must exist in the table.
        for src in [include_str!("main.rs"), include_str!("input.rs")] {
            for part in src.split("haptic(\"").skip(1) {
                let name = &part[..part.find('"').unwrap()];
                assert!(!pattern(name).is_empty(), "unknown pattern {name}");
            }
            for part in src.split("play(&dev, \"").skip(1) {
                let name = &part[..part.find('"').unwrap()];
                assert!(!pattern(name).is_empty(), "unknown pattern {name}");
            }
        }
    }

    #[test]
    fn look_velocity_round_trips() {
        assert_eq!(unpack(pack(-123.5, 42.25)), (-123.5, 42.25));
        assert_eq!(unpack(pack(0.0, 0.0)), (0.0, 0.0));
    }

    #[test]
    fn remapped_axes_are_normalised() {
        let raw = vec![(7u32, true, 0.5f32), (3u32, false, 1.0f32)];
        let stick = RawInput { code: 7, axis: true, rest: 0.0, full: 1.0 };
        assert!((raw_value(&raw, &stick) - 0.5).abs() < 1e-6);
        let inverted = RawInput { code: 7, axis: true, rest: 0.0, full: -1.0 };
        assert!((raw_value(&raw, &inverted) + 0.5).abs() < 1e-6, "pushed the other way");
        let trigger = RawInput { code: 7, axis: true, rest: -1.0, full: 1.0 };
        assert!((raw_value(&raw, &trigger) - 0.75).abs() < 1e-6, "trigger resting at -1");
        let button = RawInput { code: 3, axis: false, rest: 0.0, full: 1.0 };
        assert_eq!(raw_value(&raw, &button), 1.0);
        let missing = RawInput { code: 99, axis: true, rest: 0.0, full: 1.0 };
        assert_eq!(raw_value(&raw, &missing), 0.0, "absent input reads as resting");
    }
}
