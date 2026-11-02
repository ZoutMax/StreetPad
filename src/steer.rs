//! Precise, closed-loop view changes: turn by an exact angle, turn around, level the view.
//!
//! Google Maps keeps the Street View camera in the page URL:
//!   https://www.google.com/maps/@lat,lng,3a,75y,123.4h,92.1t/...
//!   (y = field of view, h = heading, t = tilt where 90 = level)
//! We plan a drag from the field of view, animate it, wait for Maps to update the URL, measure
//! what actually happened, learn the real degrees-per-pixel, and correct until close enough.

use crate::control::Driver;
use std::time::{Duration, Instant};

const HEADING_TOLERANCE: f64 = 2.5;
const TILT_TOLERANCE: f64 = 2.0;
const LEVEL_FOV: f64 = 75.0;
const FOV_TOLERANCE: f64 = 8.0;
const MAX_PASSES: u8 = 6;
/// Each sweep first moves this far the opposite way (then that much extra forward): Google's
/// drag dead zone is used up there, so even small corrections move the view (same net drag).
const PRE_MOVE_PX: f64 = 40.0;
/// Largest drag per tick. Big jumps make Google lose part of the drag (and teach a wrong scale).
const MAX_STEP_PX: f64 = 50.0;

/// Degrees per pixel near the picture centre for a rectilinear view of `fov` degrees.
fn base_degpp(fov: f64, width: f64) -> f64 {
    (2.0 * (fov.to_radians() / 2.0).tan() / width).to_degrees()
}
const MAX_ZOOM_STEPS: u8 = 10;
/// How long to wait for Maps to write the new camera into the URL.
const URL_TIMEOUT: Duration = Duration::from_millis(1600);
/// Extra settle time after the URL changed (Maps may update it in several small steps).
const URL_SETTLE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
pub struct View {
    pub heading: f64,
    pub tilt: f64,
    pub fov: f64,
}

/// Parse the Street View camera from a Maps URL. None when not in Street View.
pub fn parse_view(url: &str) -> Option<View> {
    let at = url.find("/@")? + 2;
    let rest = &url[at..];
    let end = rest.find('/').unwrap_or(rest.len());
    let (mut h, mut t, mut y) = (None, None, None);
    for tok in rest[..end].split(',').skip(2) {
        let (num, unit) = tok.split_at(tok.len().saturating_sub(1));
        let Ok(v) = num.parse::<f64>() else { continue };
        match unit {
            "h" => h = Some(v),
            "t" => t = Some(v),
            "y" => y = Some(v),
            _ => {}
        }
    }
    Some(View { heading: h?, tilt: t.unwrap_or(90.0), fov: y.unwrap_or(LEVEL_FOV) })
}

/// Signed smallest angle a -> b, in (-180, 180].
fn wrap(d: f64) -> f64 {
    let mut d = d % 360.0;
    if d > 180.0 { d -= 360.0 }
    if d <= -180.0 { d += 360.0 }
    d
}

#[derive(Clone, Copy, Debug)]
enum Goal {
    Heading(f64),
    Level,
}

#[derive(Debug)]
enum Phase {
    Plan,
    /// Controlled sweeps: `left` px still to drag along `dir` (unit vector); within a sweep the
    /// pointer is at `pos` and has `sweep_left` px to go; `settle` = still ticks before release.
    Sweep { dir: (f64, f64), left: f64, pos: (f64, f64), sweep_left: f64, settle: u8, pressed: bool },
    /// Wait until the URL shows a camera different from `pre` and has stopped changing.
    WaitUrl { pre: View, since: Instant, last_url: String, last_change: Instant },
}

#[derive(Debug)]
struct Maneuver {
    goal: Goal,
    phase: Phase,
    passes: u8,
    zoom_steps: u8,
    /// View and planned (degrees, pixels) of the last drag, for calibration.
    planned: Option<(View, f64, f64, bool)>,
}

pub struct Steer {
    active: Option<Maneuver>,
    /// Learned correction on top of the field-of-view estimate.
    k: f64,
    /// The first real measurement replaces `k` fully; later ones are blended.
    calibrated: bool,
}

pub enum StartError {
    NotStreetView,
}

impl Steer {
    pub fn new() -> Self {
        // Measured: Google turns ~1.4x the centre-of-view estimate per dragged pixel.
        Self { active: None, k: 1.4, calibrated: false }
    }

    pub fn busy(&self) -> bool {
        self.active.is_some()
    }

    pub fn cancel(&mut self, driver: &mut Driver) {
        if self.active.take().is_some() {
            driver.release();
        }
    }

    /// Turn right by `deg` (negative = left).
    pub fn turn(&mut self, deg: f64, url: &str) -> Result<(), StartError> {
        let v = parse_view(url).ok_or(StartError::NotStreetView)?;
        // Chain quick presses: add to the current target instead of the stale URL heading.
        let base = match self.active.as_ref().map(|m| m.goal) {
            Some(Goal::Heading(t)) => t,
            _ => v.heading,
        };
        self.start(Goal::Heading((base + deg).rem_euclid(360.0)));
        Ok(())
    }

    pub fn level(&mut self, url: &str) -> Result<(), StartError> {
        parse_view(url).ok_or(StartError::NotStreetView)?;
        self.start(Goal::Level);
        Ok(())
    }

    fn start(&mut self, goal: Goal) {
        // Keep the current motion if one is running; it will re-plan towards the new goal.
        if let Some(m) = self.active.as_mut() {
            m.goal = goal;
            m.passes = 0;
            return;
        }
        self.active = Some(Maneuver { goal, phase: Phase::Plan, passes: 0, zoom_steps: 0, planned: None });
    }

    /// Advance the maneuver. `url` reads the current page URL.
    pub fn tick(&mut self, driver: &mut Driver, url: &dyn Fn() -> Option<String>) {
        let Some(m) = self.active.as_mut() else { return };
        let width = driver.size.0.max(1.0);
        match &mut m.phase {
            Phase::Plan => {
                let Some(v) = url().as_deref().and_then(parse_view) else {
                    self.active = None;
                    return;
                };
                if m.passes >= MAX_PASSES {
                    self.active = None;
                    return;
                }
                let degpp = base_degpp(v.fov, width) * self.k;
                let (deg, horizontal) = match m.goal {
                    Goal::Heading(target) => (wrap(target - v.heading), true),
                    Goal::Level => {
                        if (v.fov - LEVEL_FOV).abs() > FOV_TOLERANCE && m.zoom_steps < MAX_ZOOM_STEPS {
                            // Narrow fov = zoomed in -> zoom out, then re-check. Small smooth wheel
                            // steps (a whole notch is ~25 degrees and overshoots).
                            let err = v.fov - LEVEL_FOV; // + = too wide -> zoom in
                            driver.zoom_smooth((err * 4.0).clamp(-100.0, 100.0));
                            m.zoom_steps += 1;
                            m.planned = None;
                            m.phase = Phase::wait(v);
                            return;
                        }
                        (90.0 - v.tilt, false)
                    }
                };
                let tol = if horizontal { HEADING_TOLERANCE } else { TILT_TOLERANCE };
                if deg.abs() < tol {
                    self.active = None;
                    driver.release();
                    return;
                }
                // Turning right (heading up) = dragging left. Looking up (tilt up) = dragging down.
                // Small corrections are damped so they approach the target instead of overshooting.
                let gain = if deg.abs() < 25.0 { 0.85 } else { 1.0 };
                let px = deg * gain / degpp;
                let total = if horizontal { (-px, 0.0) } else { (0.0, px) };
                let len = total.0.hypot(total.1);
                m.planned = Some((v, deg, px, horizontal));
                m.phase = Phase::Sweep { dir: (total.0 / len, total.1 / len), left: len, pos: (0.0, 0.0), sweep_left: 0.0, settle: 0, pressed: false };
            }
            Phase::Sweep { dir, left, pos, sweep_left, settle, pressed } => {
                let (w, h) = driver.size();
                if !*pressed {
                    if *left < 0.5 {
                        m.passes += 1;
                        let pre = m.planned.map(|p| p.0).unwrap_or(View { heading: 0.0, tilt: 90.0, fov: LEVEL_FOV });
                        m.phase = Phase::wait(pre);
                        return;
                    }
                    // Start a sweep so it runs across the middle of the picture.
                    let span = if dir.0.abs() > dir.1.abs() { w * 0.75 } else { h * 0.6 };
                    let this = left.min(span);
                    *pos = (w / 2.0 - dir.0 * this / 2.0, h / 2.0 - dir.1 * this / 2.0);
                    *sweep_left = this + PRE_MOVE_PX;
                    *left -= this;
                    *settle = 3;
                    *pressed = true;
                    driver.sweep_press(*pos);
                    // Pre-move: the opposite way first (absorbs the drag dead zone).
                    *pos = (pos.0 - dir.0 * PRE_MOVE_PX, pos.1 - dir.1 * PRE_MOVE_PX);
                    driver.sweep_move(*pos);
                } else if *sweep_left > 0.0 {
                    let d = sweep_left.min(MAX_STEP_PX);
                    *pos = (pos.0 + dir.0 * d, pos.1 + dir.1 * d);
                    *sweep_left -= d;
                    driver.sweep_move(*pos);
                } else if *settle > 0 {
                    // Hold still before letting go, so Google adds no fling.
                    *settle -= 1;
                    driver.sweep_move(*pos);
                } else {
                    driver.sweep_release(*pos);
                    *pressed = false;
                }
            }
            Phase::WaitUrl { pre, since, last_url, last_change } => {
                let now_url = url().unwrap_or_default();
                if now_url != *last_url {
                    *last_url = now_url.clone();
                    *last_change = Instant::now();
                }
                let cur = parse_view(&now_url);
                let moved = cur.is_some_and(|c| {
                    (c.heading - pre.heading).abs() > 0.05 || (c.tilt - pre.tilt).abs() > 0.05 || (c.fov - pre.fov).abs() > 0.05
                });
                if moved && last_change.elapsed() >= URL_SETTLE {
                    // Learn the real degrees-per-pixel from what the drag achieved.
                    if let (Some((v0, deg, px, horizontal)), Some(v1)) = (m.planned, cur) {
                        let achieved = if horizontal { wrap(v1.heading - v0.heading) } else { v1.tilt - v0.tilt };
                        if deg.abs() > 20.0 && achieved * deg > 0.0 && px.abs() > 1.0 {
                            let k = (achieved.abs() / px.abs()) / base_degpp(v0.fov, width);
                            if k.is_finite() {
                                let k = k.clamp(0.7, 2.5);
                                self.k = if self.calibrated { self.k * 0.5 + k * 0.5 } else { k };
                                self.calibrated = true;
                            }
                        }
                    }
                    m.phase = Phase::Plan;
                } else if since.elapsed() > URL_TIMEOUT {
                    // Maps didn't report a change: stop rather than guess (avoids overshooting).
                    self.active = None;
                }
            }
        }
    }
}

impl Phase {
    fn wait(pre: View) -> Self {
        let now = Instant::now();
        Phase::WaitUrl { pre, since: now, last_url: String::new(), last_change: now }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_street_view_url() {
        let v = parse_view("https://www.google.com/maps/@52.3731,4.8926,3a,75y,123.45h,95.6t/data=!3m6").unwrap();
        assert_eq!(v.heading, 123.45);
        assert_eq!(v.tilt, 95.6);
        assert_eq!(v.fov, 75.0);
    }

    #[test]
    fn map_url_is_not_street_view() {
        assert!(parse_view("https://www.google.com/maps/@52.37,4.89,15z").is_none());
        assert!(parse_view("https://www.google.com/maps").is_none());
    }

    #[test]
    fn wraps_angles() {
        assert_eq!(wrap(190.0), -170.0);
        assert_eq!(wrap(-190.0), 170.0);
        assert_eq!(wrap(360.0 + 10.0), 10.0);
    }
}
