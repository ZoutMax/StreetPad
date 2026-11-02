//! Map geometry: Google Maps URLs <-> coordinates <-> screen positions.
//!
//! Google Maps (web) uses Web Mercator with 256 CSS-pixel tiles: at zoom z the whole world is
//! 256 * 2^z CSS pixels wide, and the URL's `@lat,lng,Zz` is the point at the viewport centre.

use std::f64::consts::PI;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapView {
    pub lat: f64,
    pub lng: f64,
    pub zoom: f64,
}

/// The map view from a 2D map URL (`/@lat,lng,ZOOMz`). None for Street View, satellite/3D
/// (`...m` altitude) or URLs without coordinates.
pub fn map_view(url: &str) -> Option<MapView> {
    let rest = url.split("/@").nth(1)?;
    let rest = &rest[..rest.find(['/', '?']).unwrap_or(rest.len())];
    let mut it = rest.split(',');
    let lat = it.next()?.parse().ok()?;
    let lng = it.next()?.parse().ok()?;
    let zoom = it.next()?.strip_suffix('z')?.parse().ok()?;
    Some(MapView { lat, lng, zoom })
}

/// Latitude/longitude at the start of any Maps URL with `/@lat,lng,...` (map or Street View).
pub fn position(url: &str) -> Option<(f64, f64)> {
    let rest = url.split("/@").nth(1)?;
    let mut it = rest.split(',');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn world_px(lat: f64, lng: f64, zoom: f64) -> (f64, f64) {
    let size = 256.0 * 2f64.powf(zoom);
    let s = lat.clamp(-85.051_128_78, 85.051_128_78).to_radians().sin();
    let x = (lng + 180.0) / 360.0 * size;
    let y = (0.5 - ((1.0 + s) / (1.0 - s)).ln() / (4.0 * PI)) * size;
    (x, y)
}

fn world_to_latlng(x: f64, y: f64, zoom: f64) -> (f64, f64) {
    let size = 256.0 * 2f64.powf(zoom);
    let lng = x / size * 360.0 - 180.0;
    let n = PI - 2.0 * PI * y / size;
    let lat = n.sinh().atan().to_degrees();
    (lat, lng)
}

/// Coordinates under a screen point (CSS px) of a viewport of size (w, h).
pub fn screen_to_latlng(view: MapView, (x, y): (f64, f64), (w, h): (f64, f64)) -> (f64, f64) {
    let (cx, cy) = world_px(view.lat, view.lng, view.zoom);
    world_to_latlng(cx + (x - w / 2.0), cy + (y - h / 2.0), view.zoom)
}

/// Screen point (CSS px) of coordinates in a viewport of size (w, h).
pub fn latlng_to_screen(view: MapView, (lat, lng): (f64, f64), (w, h): (f64, f64)) -> (f64, f64) {
    let (cx, cy) = world_px(view.lat, view.lng, view.zoom);
    let (px, py) = world_px(lat, lng, view.zoom);
    let size = 256.0 * 2f64.powf(view.zoom);
    // Take the shortest way around the antimeridian.
    let mut dx = px - cx;
    if dx > size / 2.0 { dx -= size }
    if dx < -size / 2.0 { dx += size }
    (w / 2.0 + dx, h / 2.0 + (py - cy))
}

/// Great-circle distance in metres.
pub fn distance_m((lat1, lng1): (f64, f64), (lat2, lng2): (f64, f64)) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lng2 - lng1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6_371_000.0 * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: (f64, f64) = (3440.0, 1440.0);

    #[test]
    fn parses_map_urls() {
        let v = map_view("https://www.google.com/maps/@52.3731,4.8926,16z?entry=ttu").unwrap();
        assert_eq!(v, MapView { lat: 52.3731, lng: 4.8926, zoom: 16.0 });
        assert_eq!(map_view("https://www.google.com/maps/@52.37,4.89,15.5z/data=!5m1").unwrap().zoom, 15.5);
        assert!(map_view("https://www.google.com/maps/@52.37,4.89,3a,75y,90h,90t/data").is_none(), "Street View");
        assert!(map_view("https://www.google.com/maps/@52.37,4.89,1200m/data").is_none(), "satellite");
        assert_eq!(position("https://www.google.com/maps/@52.37,4.89,3a,75y,90h,90t"), Some((52.37, 4.89)));
    }

    #[test]
    fn centre_maps_to_the_url_coordinates() {
        let v = MapView { lat: 52.3731, lng: 4.8926, zoom: 17.0 };
        let (lat, lng) = screen_to_latlng(v, (VIEW.0 / 2.0, VIEW.1 / 2.0), VIEW);
        assert!((lat - v.lat).abs() < 1e-9 && (lng - v.lng).abs() < 1e-9);
    }

    #[test]
    fn screen_and_coordinates_round_trip() {
        let v = MapView { lat: 52.3731, lng: 4.8926, zoom: 18.0 };
        for p in [(0.0, 0.0), (3440.0, 1440.0), (123.4, 987.6), (2000.0, 100.0)] {
            let ll = screen_to_latlng(v, p, VIEW);
            let back = latlng_to_screen(v, ll, VIEW);
            assert!((back.0 - p.0).abs() < 1e-6 && (back.1 - p.1).abs() < 1e-6, "{p:?} -> {back:?}");
        }
    }

    #[test]
    fn scale_matches_web_mercator() {
        // At zoom 19 one CSS pixel is ~0.3 m * cos(lat); 100 px right of centre in Amsterdam
        // is ~18.2 m east.
        let v = MapView { lat: 52.3731, lng: 4.8926, zoom: 19.0 };
        let ll = screen_to_latlng(v, (VIEW.0 / 2.0 + 100.0, VIEW.1 / 2.0), VIEW);
        let d = distance_m((v.lat, v.lng), ll);
        assert!((d - 18.2).abs() < 0.3, "{d}");
        // Up on screen = north.
        let up = screen_to_latlng(v, (VIEW.0 / 2.0, VIEW.1 / 2.0 - 100.0), VIEW);
        assert!(up.0 > v.lat);
    }

    #[test]
    fn distances() {
        assert!(distance_m((52.0, 4.0), (52.0, 4.0)) < 1e-9);
        let d = distance_m((52.3731, 4.8926), (52.3601, 4.8852)); // Dam -> Rijksmuseum ~1.54 km
        assert!((d - 1540.0).abs() < 60.0, "{d}");
    }
}
