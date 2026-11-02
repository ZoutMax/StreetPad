//! User settings, persisted as one JSON value under HKCU\Software\StreetPad.
//! Nothing is written to disk for settings.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use winreg::{RegKey, enums::HKEY_CURRENT_USER};

pub const REG_PATH: &str = r"Software\StreetPad";
const REG_VALUE: &str = "Settings";
pub const SETTINGS_VERSION: u32 = 8;

/// Every controller button the app knows about (Xbox naming; generic pads are mapped onto these).
pub const BUTTONS: &[&str] = &[
    "a", "b", "x", "y", "lb", "rb", "back", "start", "ls", "rs", "dup", "ddown", "dleft", "dright",
];

/// Every action a button can be bound to.
pub const ACTIONS: &[&str] = &[
    "none", "click", "forward", "back", "turn_left", "turn_right", "look_up", "look_down",
    "turn_around", "snap_left", "snap_right", "level_view", "auto_walk", "street_view",
    "immersive", "zoom_in", "zoom_out", "run", "bookmark_save", "bookmark_next", "bookmark_prev",
    "settings", "guide", "search", "fullscreen", "browser_back", "escape", "reload",
];

pub const MAX_RECENT_SEARCHES: usize = 8;

/// Human-readable action names (single source for the settings page and the on-screen guide).
pub fn action_label(action: &str) -> &'static str {
    match action {
        "none" => "Nothing",
        "click" => "Click at crosshair",
        "forward" => "Walk forward",
        "back" => "Walk back",
        "turn_left" => "Turn left (smooth)",
        "turn_right" => "Turn right (smooth)",
        "look_up" => "Look up",
        "look_down" => "Look down",
        "turn_around" => "Turn around 180°",
        "snap_left" => "Snap turn left",
        "snap_right" => "Snap turn right",
        "level_view" => "Level view & reset zoom",
        "auto_walk" => "Auto-walk on/off",
        "street_view" => "Enter / leave Street View",
        "immersive" => "Immersive mode on/off",
        "zoom_in" => "Zoom in",
        "zoom_out" => "Zoom out",
        "run" => "Run (while held)",
        "bookmark_save" => "Save bookmark",
        "bookmark_next" => "Next bookmark",
        "bookmark_prev" => "Previous bookmark",
        "settings" => "Settings",
        "guide" => "Show button guide",
        "search" => "Search (on-screen keyboard)",
        "fullscreen" => "Fullscreen on/off",
        "browser_back" => "Back (undo jump)",
        "escape" => "Esc key",
        "reload" => "Reload page",
        _ => "?",
    }
}

pub fn button_label(button: &str) -> &'static str {
    match button {
        "a" => "A", "b" => "B", "x" => "X", "y" => "Y", "lb" => "LB", "rb" => "RB",
        "back" => "Back", "start" => "Start", "ls" => "Left stick click", "rs" => "Right stick click",
        "dup" => "D-pad up", "ddown" => "D-pad down", "dleft" => "D-pad left", "dright" => "D-pad right",
        _ => "?",
    }
}

/// Actions that last while the button is held. They can't share a button with a hold action.
pub const CONTINUOUS: &[&str] = &["forward", "back", "turn_left", "turn_right", "look_up", "look_down", "run"];

pub const MAX_BOOKMARKS: usize = 100;
pub const MAX_PROFILES: usize = 50;

/// Controller families: decide button names and the default preset.
pub const FAMILIES: &[&str] = &["xbox", "playstation", "nintendo", "generic"];

/// Remap targets for the setup wizard: every button plus the stick/trigger axes.
pub const AXIS_TARGETS: &[&str] = &["lx", "ly", "rx", "ry", "lt", "rt"];

/// One physical input captured by the remap wizard.
/// Button: `axis = false`, pressed when the raw button is down.
/// Axis: normalised as (raw - rest) / (full - rest); sticks use -1..1, triggers and
/// axis-driven buttons (e.g. a D-pad hat) use 0..1 (pressed above 0.5).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct RawInput {
    pub code: u32,
    pub axis: bool,
    pub rest: f32,
    pub full: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Remap {
    /// Target (button name or axis name) -> physical input.
    pub inputs: BTreeMap<String, RawInput>,
}

/// The control part of the settings, stored per profile.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Controls {
    pub look_speed_x: f32,
    pub look_speed_y: f32,
    pub invert_y: bool,
    pub look_curve: f32,
    pub deadzone: f32,
    pub pan_speed: f32,
    pub move_rate: f32,
    pub run_multiplier: f32,
    pub zoom_rate: f32,
    pub left_x_mode: String,
    pub swap_sticks: bool,
    pub buttons: BTreeMap<String, String>,
    pub hold_buttons: BTreeMap<String, String>,
    pub snap_angle: f32,
    pub vibrate_on_step: bool,
}

impl Default for Controls {
    fn default() -> Self {
        Settings::default().controls()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Profile {
    /// "preset:<family>" for built-ins, "user:<n>" for the user's own profiles.
    pub id: String,
    pub name: String,
    /// Controller family the profile was made for (button names in the UI).
    pub family: String,
    pub controls: Controls,
    pub remap: Remap,
}

impl Default for Profile {
    fn default() -> Self {
        Self { id: String::new(), name: String::new(), family: "generic".into(), controls: Controls::default(), remap: Remap::default() }
    }
}

/// True for https://www.google.<tld>/... or https://maps.google.<tld>/... (host checked exactly,
/// so e.g. https://www.google.evil.com/ is rejected).
pub fn is_google_maps_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some(tld) = host.strip_prefix("www.google.").or_else(|| host.strip_prefix("maps.google.")) else { return false };
    let parts: Vec<&str> = tld.split('.').collect();
    match parts.as_slice() {
        [a] => (2..=3).contains(&a.len()) && a.chars().all(|c| c.is_ascii_lowercase()),
        [a, b] => matches!(*a, "co" | "com") && b.len() == 2 && b.chars().all(|c| c.is_ascii_lowercase()),
        _ => false,
    }
}

pub fn family_name(family: &str) -> &'static str {
    match family {
        "xbox" => "Xbox",
        "playstation" => "PlayStation",
        "nintendo" => "Nintendo Switch",
        _ => "Generic gamepad",
    }
}

/// Built-in presets, one per family. Bindings are by position (the bottom face button is
/// always "a"), so the same layout feels the same on every controller.
pub fn presets() -> Vec<Profile> {
    FAMILIES
        .iter()
        .map(|f| Profile {
            id: format!("preset:{f}"),
            name: format!("{} (preset)", family_name(f)),
            family: f.to_string(),
            controls: Controls::default(),
            remap: Remap::default(),
        })
        .collect()
}

/// Button names as printed on each controller family (positional: "a" = bottom face button).
pub fn button_label_for(button: &str, family: &str) -> &'static str {
    match (family, button) {
        ("playstation", "a") => "✕",
        ("playstation", "b") => "○",
        ("playstation", "x") => "□",
        ("playstation", "y") => "△",
        ("playstation", "lb") => "L1",
        ("playstation", "rb") => "R1",
        ("playstation", "back") => "Share / Create",
        ("playstation", "start") => "Options",
        ("playstation", "ls") => "L3",
        ("playstation", "rs") => "R3",
        ("nintendo", "a") => "B",
        ("nintendo", "b") => "A",
        ("nintendo", "x") => "Y",
        ("nintendo", "y") => "X",
        ("nintendo", "lb") => "L",
        ("nintendo", "rb") => "R",
        ("nintendo", "back") => "−",
        ("nintendo", "start") => "+",
        ("nintendo", "ls") => "Left stick click",
        ("nintendo", "rs") => "Right stick click",
        // Current Xbox controllers print View / Menu (Back / Start on the Xbox 360).
        ("xbox", "back") => "View",
        ("xbox", "start") => "Menu",
        ("generic", "a") => "Bottom button",
        ("generic", "b") => "Right button",
        ("generic", "x") => "Left button",
        ("generic", "y") => "Top button",
        ("generic", "lb") => "Left shoulder",
        ("generic", "rb") => "Right shoulder",
        ("generic", "back") => "Select / Back",
        ("generic", "start") => "Start",
        _ => button_label(button),
    }
}

pub fn trigger_labels(family: &str) -> (&'static str, &'static str) {
    match family {
        "playstation" => ("L2", "R2"),
        "nintendo" => ("ZL", "ZR"),
        "generic" => ("Left trigger", "Right trigger"),
        _ => ("LT", "RT"),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Bookmark {
    pub name: String,
    pub url: String,
}

fn default_buttons() -> BTreeMap<String, String> {
    [
        ("a", "click"), ("b", "turn_around"), ("x", "bookmark_save"), ("y", "browser_back"),
        ("lb", "snap_left"), ("rb", "snap_right"), ("back", "immersive"), ("start", "settings"),
        ("ls", "run"), ("rs", "level_view"), ("dup", "auto_walk"), ("ddown", "back"),
        ("dleft", "bookmark_prev"), ("dright", "bookmark_next"),
    ]
    .into_iter()
    .map(|(b, a)| (b.to_string(), a.to_string()))
    .collect()
}

fn default_hold_buttons() -> BTreeMap<String, String> {
    BUTTONS
        .iter()
        .map(|b| {
            let a = match *b { "x" => "search", "y" => "street_view", "back" => "fullscreen", "start" => "guide", _ => "none" };
            (b.to_string(), a.to_string())
        })
        .collect()
}

/// The v3 layout, used to upgrade buttons the user never changed.
fn v3_default_buttons() -> BTreeMap<String, String> {
    let mut m = default_buttons();
    m.insert("back".into(), "fullscreen".into());
    m.insert("dup".into(), "forward".into());
    m
}

fn v3_default_hold_buttons() -> BTreeMap<String, String> {
    BUTTONS
        .iter()
        .map(|b| {
            let a = match *b { "y" => "escape", "back" => "reload", _ => "none" };
            (b.to_string(), a.to_string())
        })
        .collect()
}

/// The v2 default layout, used to upgrade users who never customised it.
fn v2_default_buttons() -> BTreeMap<String, String> {
    [
        ("a", "click"), ("b", "back"), ("x", "browser_back"), ("y", "escape"),
        ("lb", "zoom_out"), ("rb", "zoom_in"), ("back", "fullscreen"), ("start", "settings"),
        ("ls", "run"), ("rs", "none"), ("dup", "forward"), ("ddown", "back"),
        ("dleft", "turn_left"), ("dright", "turn_right"),
    ]
    .into_iter()
    .map(|(b, a)| (b.to_string(), a.to_string()))
    .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// "auto", "xinput:0".."xinput:3" or "gilrs:<uuid-hex>:<n>"
    pub device: String,
    /// Look speed at full stick, in screen pixels dragged per second.
    pub look_speed_x: f32,
    pub look_speed_y: f32,
    pub invert_y: bool,
    /// Response curve exponent (1 = linear, 2 = fine aim near centre).
    pub look_curve: f32,
    /// Radial dead zone 0..0.5
    pub deadzone: f32,
    /// Map: panning speed at full stick (CSS px/s).
    pub pan_speed: f32,
    /// Steps per second at full forward stick.
    pub move_rate: f32,
    /// Multiplier while the "run" button is held.
    pub run_multiplier: f32,
    /// Zoom notches per second at full trigger.
    pub zoom_rate: f32,
    /// Left stick X: "turn" (rotate view) or "none".
    pub left_x_mode: String,
    pub swap_sticks: bool,
    /// Action on tap (or while held, for continuous actions).
    pub buttons: BTreeMap<String, String>,
    /// Action when the button is held for half a second ("none" = no hold action).
    pub hold_buttons: BTreeMap<String, String>,
    /// Angle of the snap-turn actions, in degrees.
    pub snap_angle: f32,
    pub bookmarks: Vec<Bookmark>,
    /// Index of the last bookmark visited (for next / previous).
    pub bookmark_pos: usize,
    /// "remember" (browser profile in %LOCALAPPDATA%\StreetPad) or "private" (temp profile, wiped on exit).
    pub login_mode: String,
    pub start_at_last_location: bool,
    pub last_url: String,
    /// Start in fullscreen (Back / F11 toggles during a session without changing this).
    pub fullscreen: bool,
    /// Ignore the controller while another window is in front.
    pub only_when_focused: bool,
    pub vibrate_on_step: bool,
    /// Working copy of the active profile's remap (generic controllers).
    pub remap: Remap,
    /// The user's own controller profiles (presets are built in).
    pub profiles: Vec<Profile>,
    /// Device key ("xinput" or "gilrs:<uuid>") -> profile id last used with it.
    pub device_profiles: BTreeMap<String, String>,
    /// Profile currently loaded into the control fields above.
    pub active_profile: String,
    /// Show an aim crosshair in the centre while the controller is in use.
    pub crosshair: bool,
    /// Vibration feedback patterns (Xbox controllers).
    pub vibration: bool,
    /// Vibration strength 0.1..1.
    pub vibration_strength: f32,
    /// Monitor StreetPad was on when it closed (Windows device name, e.g. "\\.\DISPLAY2").
    pub monitor: String,
    /// Last searches typed with the on-screen keyboard (newest first).
    pub recent_searches: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            device: "auto".into(),
            look_speed_x: 700.0,
            look_speed_y: 450.0,
            invert_y: false,
            look_curve: 1.7,
            deadzone: 0.18,
            pan_speed: 1400.0,
            move_rate: 2.0,
            run_multiplier: 2.0,
            zoom_rate: 4.0,
            left_x_mode: "turn".into(),
            swap_sticks: false,
            buttons: default_buttons(),
            hold_buttons: default_hold_buttons(),
            snap_angle: 90.0,
            bookmarks: Vec::new(),
            bookmark_pos: 0,
            login_mode: "remember".into(),
            start_at_last_location: true,
            last_url: String::new(),
            fullscreen: true,
            only_when_focused: false,
            vibrate_on_step: true,
            crosshair: true,
            remap: Remap::default(),
            profiles: Vec::new(),
            device_profiles: BTreeMap::new(),
            active_profile: String::new(),
            recent_searches: Vec::new(),
            monitor: String::new(),
            vibration: true,
            vibration_strength: 1.0,
        }
    }
}

impl Settings {
    /// Clamp values and fill any missing/unknown entries so the rest of the app can trust them.
    pub fn sanitize(&mut self) {
        let d = Settings::default();
        // v1 -> v2: A became "click at crosshair" (walking stays on the stick and D-pad).
        if self.version < 2 && self.buttons.get("a").map(String::as_str) == Some("forward") {
            self.buttons.insert("a".into(), "click".into());
        }
        // v2 -> v3: new full layout. Upgrade each button the user never changed from the v2 default.
        if self.version < 3 {
            let (old, new) = (v2_default_buttons(), default_buttons());
            for b in BUTTONS {
                if self.buttons.get(*b) == old.get(*b) {
                    self.buttons.insert(b.to_string(), new[*b].clone());
                }
            }
            self.hold_buttons = v3_default_hold_buttons();
            self.fullscreen = true; // the app now always starts fullscreen by default
            if self.snap_angle == 0.0 { self.snap_angle = d.snap_angle; }
        }
        // v3 -> v4: auto-walk, immersive mode, Street View toggle, button guide.
        if self.version < 4 {
            let (old, new) = (v3_default_buttons(), default_buttons());
            let (old_h, new_h) = (v3_default_hold_buttons(), default_hold_buttons());
            for b in BUTTONS {
                if self.buttons.get(*b) == old.get(*b) {
                    self.buttons.insert(b.to_string(), new[*b].clone());
                }
                if self.hold_buttons.get(*b) == old_h.get(*b) {
                    self.hold_buttons.insert(b.to_string(), new_h[*b].clone());
                }
            }
        }
        // v4 -> v5: the controller keeps working when StreetPad is not the active window.
        if self.version < 5 {
            self.only_when_focused = false;
        }
        // v5 -> v6: controller profiles. Customised controls become "My Xbox profile".
        if self.version < 6 && self.profiles.is_empty() {
            // Compare with the defaults of that time: hold X = search only came with v7.
            let mut then = Controls::default();
            then.hold_buttons.insert("x".into(), "none".into());
            let mut cur = self.controls();
            if cur.hold_buttons.get("x").map(String::as_str) == Some("search") {
                cur.hold_buttons.insert("x".into(), "none".into());
            }
            let customised = cur != then;
            if customised {
                self.profiles.push(Profile {
                    id: "user:1".into(),
                    name: "My Xbox profile".into(),
                    family: "xbox".into(),
                    controls: self.controls(),
                    remap: Remap::default(),
                });
                self.device_profiles.insert("xinput".into(), "user:1".into());
                self.active_profile = "user:1".into();
            }
        }
        // v6 -> v7: hold X opens the search keyboard (if X has no hold action yet).
        if self.version < 7 && self.hold_buttons.get("x").is_none_or(|a| a == "none") {
            self.hold_buttons.insert("x".into(), "search".into());
            for p in &mut self.profiles {
                if p.controls.hold_buttons.get("x").is_none_or(|a| a == "none") {
                    p.controls.hold_buttons.insert("x".into(), "search".into());
                }
            }
        }
        // v7 -> v8: vibration strength default raised from 70% to 100%.
        if self.version < 8 && (self.vibration_strength - 0.7).abs() < 1e-3 {
            self.vibration_strength = 1.0;
        }
        let clamp = |v: f32, lo: f32, hi: f32, def: f32| if v.is_finite() { v.clamp(lo, hi) } else { def };
        self.look_speed_x = clamp(self.look_speed_x, 50.0, 4000.0, d.look_speed_x);
        self.look_speed_y = clamp(self.look_speed_y, 50.0, 4000.0, d.look_speed_y);
        self.look_curve = clamp(self.look_curve, 1.0, 3.0, d.look_curve);
        self.deadzone = clamp(self.deadzone, 0.0, 0.5, d.deadzone);
        self.move_rate = clamp(self.move_rate, 0.3, 8.0, d.move_rate);
        self.pan_speed = clamp(self.pan_speed, 150.0, 5000.0, d.pan_speed);
        self.run_multiplier = clamp(self.run_multiplier, 1.0, 4.0, d.run_multiplier);
        self.zoom_rate = clamp(self.zoom_rate, 0.5, 15.0, d.zoom_rate);
        if !["turn", "none"].contains(&self.left_x_mode.as_str()) { self.left_x_mode = d.left_x_mode; }
        if !["remember", "private"].contains(&self.login_mode.as_str()) { self.login_mode = d.login_mode; }
        if self.device.is_empty() { self.device = "auto".into(); }
        self.snap_angle = clamp(self.snap_angle, 15.0, 180.0, d.snap_angle);
        self.vibration_strength = clamp(self.vibration_strength, 0.1, 1.0, d.vibration_strength);
        for b in BUTTONS {
            let ok = self.buttons.get(*b).is_some_and(|a| ACTIONS.contains(&a.as_str()));
            if !ok { self.buttons.insert(b.to_string(), d.buttons[*b].clone()); }
            // Hold actions must be one-shot, and a continuous tap action can't have one.
            let tap_continuous = CONTINUOUS.contains(&self.buttons[*b].as_str());
            let hold_ok = self.hold_buttons.get(*b).is_some_and(|a| ACTIONS.contains(&a.as_str()) && !CONTINUOUS.contains(&a.as_str()));
            if !hold_ok || tap_continuous { self.hold_buttons.insert(b.to_string(), "none".into()); }
        }
        self.buttons.retain(|k, _| BUTTONS.contains(&k.as_str()));
        self.hold_buttons.retain(|k, _| BUTTONS.contains(&k.as_str()));
        self.bookmarks.retain(|bm| is_google_maps_url(&bm.url));
        self.bookmarks.truncate(MAX_BOOKMARKS);
        for bm in &mut self.bookmarks {
            bm.name = bm.name.trim().chars().take(120).collect();
            if bm.name.is_empty() { bm.name = "Bookmark".into(); }
        }
        if self.bookmark_pos >= self.bookmarks.len() { self.bookmark_pos = 0; }
        self.recent_searches.retain(|q| !q.trim().is_empty());
        self.recent_searches.truncate(MAX_RECENT_SEARCHES);
        for q in &mut self.recent_searches {
            *q = q.trim().chars().take(200).collect();
        }
        self.profiles.retain(|p| p.id.starts_with("user:"));
        self.profiles.truncate(MAX_PROFILES);
        for p in &mut self.profiles {
            p.name = p.name.trim().chars().take(60).collect();
            if p.name.is_empty() { p.name = "My profile".into(); }
            if !FAMILIES.contains(&p.family.as_str()) { p.family = "generic".into(); }
        }
        let ids: Vec<String> = self.all_profiles().into_iter().map(|p| p.id).collect();
        self.device_profiles.retain(|_, id| ids.contains(id));
        if !ids.contains(&self.active_profile) { self.active_profile.clear(); }
        let valid = |t: &String| BUTTONS.contains(&t.as_str()) || AXIS_TARGETS.contains(&t.as_str());
        self.remap.inputs.retain(|t, _| valid(t));
        if !is_google_maps_url(&self.last_url) {
            self.last_url.clear();
        }
        self.version = SETTINGS_VERSION;
    }

    pub fn load() -> Self {
        let raw = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(REG_PATH)
            .and_then(|k| k.get_value::<String, _>(REG_VALUE))
            .ok();
        let mut s = match raw.as_deref().map(serde_json::from_str::<Settings>) {
            Some(Ok(s)) => s,
            Some(Err(_)) => {
                // Unreadable settings: keep a copy before starting from defaults, so bookmarks
                // and profiles can be recovered.
                if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(REG_PATH) {
                    let _ = key.set_value("Settings.bak", raw.as_ref().unwrap());
                }
                Settings::default()
            }
            None => Settings::default(),
        };
        let old_version = s.version;
        s.sanitize();
        if old_version != SETTINGS_VERSION {
            s.save(); // persist upgraded settings right away
        }
        s
    }

    pub fn save(&self) {
        let Ok(json) = serde_json::to_string(self) else { return };
        if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(REG_PATH) {
            let _ = key.set_value(REG_VALUE, &json);
        }
    }

    /// The control fields as a profile payload.
    pub fn controls(&self) -> Controls {
        Controls {
            look_speed_x: self.look_speed_x,
            look_speed_y: self.look_speed_y,
            invert_y: self.invert_y,
            look_curve: self.look_curve,
            deadzone: self.deadzone,
            pan_speed: self.pan_speed,
            move_rate: self.move_rate,
            run_multiplier: self.run_multiplier,
            zoom_rate: self.zoom_rate,
            left_x_mode: self.left_x_mode.clone(),
            swap_sticks: self.swap_sticks,
            buttons: self.buttons.clone(),
            hold_buttons: self.hold_buttons.clone(),
            snap_angle: self.snap_angle,
            vibrate_on_step: self.vibrate_on_step,
        }
    }

    /// Load a profile's controls (and remap) into the working fields.
    pub fn apply_profile(&mut self, p: &Profile) {
        let c = &p.controls;
        self.look_speed_x = c.look_speed_x;
        self.look_speed_y = c.look_speed_y;
        self.invert_y = c.invert_y;
        self.look_curve = c.look_curve;
        self.deadzone = c.deadzone;
        self.pan_speed = c.pan_speed;
        self.move_rate = c.move_rate;
        self.run_multiplier = c.run_multiplier;
        self.zoom_rate = c.zoom_rate;
        self.left_x_mode = c.left_x_mode.clone();
        self.swap_sticks = c.swap_sticks;
        self.buttons = c.buttons.clone();
        self.hold_buttons = c.hold_buttons.clone();
        self.snap_angle = c.snap_angle;
        self.vibrate_on_step = c.vibrate_on_step;
        self.remap = p.remap.clone();
        self.active_profile = p.id.clone();
    }

    pub fn all_profiles(&self) -> Vec<Profile> {
        let mut v = presets();
        v.extend(self.profiles.iter().cloned());
        v
    }

    pub fn find_profile(&self, id: &str) -> Option<Profile> {
        self.all_profiles().into_iter().find(|p| p.id == id)
    }

    /// A fresh id and a unique name for a new user profile.
    pub fn new_profile_identity(&self, base: &str) -> (String, String) {
        let n = (1..).find(|n| !self.profiles.iter().any(|p| p.id == format!("user:{n}"))).unwrap_or(1);
        let mut name = base.to_string();
        let mut k = 2;
        while self.profiles.iter().any(|p| p.name == name) {
            name = format!("{base} {k}");
            k += 1;
        }
        (format!("user:{n}"), name)
    }

    /// Save the working control fields into the active user profile. When a preset is active,
    /// a personal copy is created first (presets never change), like in games.
    /// Returns the name of a newly created profile, if any.
    pub fn store_controls(&mut self, device_key: Option<&str>, family: &str) -> Option<String> {
        let mut created = None;
        if !self.active_profile.starts_with("user:") || !self.profiles.iter().any(|p| p.id == self.active_profile) {
            if self.profiles.len() >= MAX_PROFILES {
                return None;
            }
            let (id, name) = self.new_profile_identity(&format!("My {} profile", family_name(family)));
            self.profiles.push(Profile { id: id.clone(), name: name.clone(), family: family.into(), controls: self.controls(), remap: self.remap.clone() });
            self.active_profile = id.clone();
            if let Some(k) = device_key {
                self.device_profiles.insert(k.into(), id);
            }
            created = Some(name);
        }
        let (controls, remap, id) = (self.controls(), self.remap.clone(), self.active_profile.clone());
        if let Some(p) = self.profiles.iter_mut().find(|p| p.id == id) {
            p.controls = controls;
            p.remap = remap;
        }
        created
    }

    pub fn action_for(&self, button: &str) -> &str {
        self.buttons.get(button).map(String::as_str).unwrap_or("none")
    }

    pub fn hold_action_for(&self, button: &str) -> &str {
        self.hold_buttons.get(button).map(String::as_str).unwrap_or("none")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upgrade(json: &str) -> Settings {
        let mut s: Settings = serde_json::from_str(json).expect("valid json");
        s.sanitize();
        s
    }

    #[test]
    fn defaults_are_stable_under_sanitize() {
        let d = Settings::default();
        let mut s = d.clone();
        s.sanitize();
        assert_eq!(s, d);
        assert_eq!(d.version, SETTINGS_VERSION);
    }

    #[test]
    fn every_button_has_valid_bindings() {
        let d = Settings::default();
        for b in BUTTONS {
            assert!(ACTIONS.contains(&d.action_for(b)), "tap {b}");
            let h = d.hold_action_for(b);
            assert!(ACTIONS.contains(&h) && !CONTINUOUS.contains(&h), "hold {b}");
            if CONTINUOUS.contains(&d.action_for(b)) {
                assert_eq!(h, "none", "continuous tap on {b} can't have a hold");
            }
        }
        for a in ACTIONS {
            assert_ne!(action_label(a), "?", "label for {a}");
        }
    }

    #[test]
    fn upgrades_v1_to_current_layout() {
        let s = upgrade(r#"{"version":1,"buttons":{"a":"forward","b":"back","x":"browser_back","y":"escape","lb":"zoom_out","rb":"zoom_in","back":"fullscreen","start":"settings","ls":"run","rs":"none","dup":"forward","ddown":"back","dleft":"turn_left","dright":"turn_right"},"fullscreen":false,"only_when_focused":true}"#);
        assert_eq!(s.version, SETTINGS_VERSION);
        assert_eq!(s.action_for("a"), "click");
        assert_eq!(s.action_for("b"), "turn_around");
        assert_eq!(s.action_for("back"), "immersive");
        assert_eq!(s.action_for("dup"), "auto_walk");
        assert_eq!(s.hold_action_for("y"), "street_view");
        assert_eq!(s.hold_action_for("x"), "search");
        assert!(s.fullscreen, "v3 made fullscreen the default start");
        assert!(!s.only_when_focused, "v5 enabled background control");
    }

    #[test]
    fn upgrade_keeps_customised_buttons() {
        // v3 user who rebound RS: their choice must survive the v4+ upgrades.
        let mut v3 = Settings::default();
        v3.version = 3;
        v3.buttons.insert("rs".into(), "reload".into());
        v3.buttons.insert("back".into(), "fullscreen".into()); // v3 default -> upgraded
        let s = upgrade(&serde_json::to_string(&v3).unwrap());
        assert_eq!(s.action_for("rs"), "reload");
        assert_eq!(s.action_for("back"), "immersive");
    }

    #[test]
    fn v6_upgrade_turns_custom_controls_into_a_profile() {
        let mut v5 = Settings::default();
        v5.version = 5;
        v5.look_speed_x = 1234.0;
        let s = upgrade(&serde_json::to_string(&v5).unwrap());
        assert_eq!(s.profiles.len(), 1);
        assert_eq!(s.profiles[0].controls.look_speed_x, 1234.0);
        assert_eq!(s.device_profiles.get("xinput").map(String::as_str), Some("user:1"));
        let untouched = upgrade(r#"{"version":5}"#);
        assert!(untouched.profiles.is_empty(), "default controls need no profile");
    }

    #[test]
    fn editing_a_preset_creates_a_personal_copy_once() {
        let mut s = Settings::default();
        let preset = s.find_profile("preset:playstation").unwrap();
        s.apply_profile(&preset);
        s.invert_y = true;
        let created = s.store_controls(Some("gilrs:abc"), "playstation");
        assert_eq!(created.as_deref(), Some("My PlayStation profile"));
        assert!(s.active_profile.starts_with("user:"));
        assert_eq!(s.device_profiles.get("gilrs:abc"), Some(&s.active_profile));
        // Second edit updates the same profile.
        s.invert_y = false;
        assert!(s.store_controls(Some("gilrs:abc"), "playstation").is_none());
        assert_eq!(s.profiles.len(), 1);
        assert!(!s.profiles[0].controls.invert_y);
        // Presets themselves never change.
        assert_eq!(s.find_profile("preset:playstation").unwrap().controls, Controls::default());
    }

    #[test]
    fn profile_names_are_unique() {
        let mut s = Settings::default();
        let (id1, n1) = s.new_profile_identity("My Xbox profile");
        s.profiles.push(Profile { id: id1.clone(), name: n1.clone(), ..Profile::default() });
        let (id2, n2) = s.new_profile_identity("My Xbox profile");
        assert_ne!(id1, id2);
        assert_eq!(n1, "My Xbox profile");
        assert_eq!(n2, "My Xbox profile 2");
    }

    #[test]
    fn sanitize_clamps_and_cleans() {
        let mut s = Settings::default();
        s.look_speed_x = f32::NAN;
        s.deadzone = 9.0;
        s.snap_angle = 1.0;
        s.buttons.insert("a".into(), "explode".into());
        s.hold_buttons.insert("ls".into(), "settings".into()); // ls taps "run" (continuous)
        s.last_url = "https://evil.example/".into();
        s.recent_searches = vec!["  dam  ".into(), "".into()];
        s.profiles.push(Profile { id: "preset:hack".into(), ..Profile::default() });
        s.device_profiles.insert("x".into(), "user:404".into());
        s.sanitize();
        assert_eq!(s.look_speed_x, Settings::default().look_speed_x);
        assert_eq!(s.deadzone, 0.5);
        assert_eq!(s.snap_angle, 15.0);
        assert_eq!(s.action_for("a"), "click");
        assert_eq!(s.hold_action_for("ls"), "none");
        assert!(s.last_url.is_empty());
        assert_eq!(s.recent_searches, vec!["dam".to_string()]);
        assert!(s.profiles.is_empty(), "only user profiles are stored");
        assert!(s.device_profiles.is_empty(), "links to missing profiles are dropped");
    }

    #[test]
    fn old_default_settings_get_no_spurious_profile() {
        for v in [1, 3, 5] {
            let s = upgrade(&format!(r#"{{"version":{v}}}"#));
            assert!(s.profiles.is_empty(), "v{v} defaults must not become a 'customised' profile");
        }
    }

    #[test]
    fn google_url_check_is_exact() {
        assert!(is_google_maps_url("https://www.google.com/maps/@1,2,3z"));
        assert!(is_google_maps_url("https://www.google.co.uk/maps"));
        assert!(is_google_maps_url("https://maps.google.nl/"));
        assert!(!is_google_maps_url("https://www.google.evil.com/maps"));
        assert!(!is_google_maps_url("http://www.google.com/maps"));
        assert!(!is_google_maps_url("https://evil.com/?https://www.google.com"));
    }

    #[test]
    fn button_names_follow_the_controller() {
        assert_eq!(button_label_for("a", "playstation"), "✕");
        assert_eq!(button_label_for("a", "nintendo"), "B");
        assert_eq!(button_label_for("a", "xbox"), "A");
        assert_eq!(button_label_for("back", "xbox"), "View");
        assert_eq!(button_label_for("start", "playstation"), "Options");
        assert_eq!(trigger_labels("playstation"), ("L2", "R2"));
        assert_eq!(presets().len(), FAMILIES.len());
    }
}
