//! Zoom and scroll position of the document view.
//!
//! Scroll positions are in PDF points; `zoom` is a percentage where 100% shows the page at
//! its real size on a 96 DPI screen. Screen values are egui points relative to the
//! viewport's top-left corner.

use crate::SCREEN_PER_PT_AT_100;
use crate::scroll::SmoothScroll;

pub const ZOOM_MIN: f32 = 10.0;
pub const ZOOM_MAX: f32 = 1600.0;

/// Zoom levels used by the +/- buttons and Ctrl+plus/minus, like Acrobat's.
pub const ZOOM_STEPS: [f32; 16] = [
    10.0, 25.0, 33.33, 50.0, 66.67, 75.0, 100.0, 125.0, 150.0, 200.0, 300.0, 400.0, 600.0, 800.0,
    1200.0, 1600.0,
];

/// How long the zoom must stay still before tiles are re-rendered at the new zoom.
pub const SETTLE_SECONDS: f64 = 0.12;

#[derive(Clone, Copy, Debug)]
pub struct Camera {
    zoom: f32,
    pub x: SmoothScroll,
    pub y: SmoothScroll,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            zoom: 100.0,
            x: SmoothScroll::default(),
            y: SmoothScroll::default(),
        }
    }
}

impl Camera {
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Screen points per PDF point.
    pub fn screen_per_pt(&self) -> f32 {
        SCREEN_PER_PT_AT_100 * self.zoom / 100.0
    }

    /// Distance in points from the viewport's left edge to the document's left edge. A
    /// document narrower than the view is centered; a wider one scrolls horizontally.
    pub fn doc_left_pt(&self, content_w_pt: f32, view_w: f32) -> f32 {
        let view_w_pt = view_w / self.screen_per_pt();
        if content_w_pt <= view_w_pt {
            (view_w_pt - content_w_pt) / 2.0
        } else {
            -self.x.position()
        }
    }

    /// Largest scroll positions (points) for a document of `content` size in a `view`.
    pub fn max_scroll(&self, content: (f32, f32), view: (f32, f32)) -> (f32, f32) {
        let s = self.screen_per_pt();
        (
            (content.0 - view.0 / s).max(0.0),
            (content.1 - view.1 / s).max(0.0),
        )
    }

    pub fn clamp(&mut self, content: (f32, f32), view: (f32, f32)) {
        let (max_x, max_y) = self.max_scroll(content, view);
        self.x.clamp(max_x);
        self.y.clamp(max_y);
    }

    /// Changes the zoom so the document point under `anchor` stays under it.
    pub fn zoom_around(
        &mut self,
        new_zoom: f32,
        anchor: (f32, f32),
        content: (f32, f32),
        view: (f32, f32),
    ) {
        let s0 = self.screen_per_pt();
        let doc_x = anchor.0 / s0 - self.doc_left_pt(content.0, view.0);
        let doc_y = anchor.1 / s0 + self.y.position();

        self.zoom = new_zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        let s1 = self.screen_per_pt();
        self.y.jump_to(doc_y - anchor.1 / s1);
        if content.0 > view.0 / s1 {
            self.x.jump_to(doc_x - anchor.0 / s1);
        } else {
            self.x.jump_to(0.0);
        }
        self.clamp(content, view);
    }

    /// Advances both scroll animations. Returns `true` while either is moving.
    pub fn update(&mut self, dt: f32) -> bool {
        let x = self.x.update(dt);
        let y = self.y.update(dt);
        x || y
    }
}

/// Next zoom step above (`direction > 0`) or below the current zoom.
pub fn step_zoom(zoom: f32, direction: i32) -> f32 {
    if direction > 0 {
        ZOOM_STEPS
            .iter()
            .copied()
            .find(|&z| z > zoom + 0.01)
            .unwrap_or(ZOOM_MAX)
    } else {
        ZOOM_STEPS
            .iter()
            .rev()
            .copied()
            .find(|&z| z < zoom - 0.01)
            .unwrap_or(ZOOM_MIN)
    }
}

/// Zoom at which a page `width_pt` wide fills `view_w` screen points.
pub fn fit_width_zoom(width_pt: f32, view_w: f32) -> f32 {
    (view_w / (width_pt.max(1.0) * SCREEN_PER_PT_AT_100) * 100.0).clamp(ZOOM_MIN, ZOOM_MAX)
}

/// Zoom at which a whole page fits inside the view.
pub fn fit_page_zoom(page_pt: (f32, f32), view: (f32, f32)) -> f32 {
    fit_width_zoom(page_pt.0, view.0).min(fit_width_zoom(page_pt.1, view.1))
}

/// Decides the zoom that tiles are rendered at. While the zoom keeps changing (a pinch or
/// Ctrl+wheel in progress), the existing tiles are stretched on the GPU; once it has been
/// still for [`SETTLE_SECONDS`], sharp tiles are rendered for the new zoom.
#[derive(Clone, Copy, Debug)]
pub struct ZoomSettle {
    render: f32,
    last_seen: f32,
    last_change: f64,
}

impl ZoomSettle {
    pub fn new(zoom: f32) -> Self {
        ZoomSettle {
            render: zoom,
            last_seen: zoom,
            last_change: 0.0,
        }
    }

    pub fn render_zoom(&self) -> f32 {
        self.render
    }

    /// Use the new zoom right away, e.g. after opening a file or clicking Fit Page.
    pub fn snap(&mut self, zoom: f32) {
        self.render = zoom;
        self.last_seen = zoom;
    }

    /// Call every frame with the displayed zoom and the current time in seconds. Returns
    /// how long to wait before calling again when a re-render is still pending.
    pub fn update(&mut self, zoom: f32, now: f64) -> Option<f64> {
        if zoom != self.last_seen {
            self.last_seen = zoom;
            self.last_change = now;
        }
        if self.render == zoom {
            return None;
        }
        let waited = now - self.last_change;
        if waited >= SETTLE_SECONDS {
            self.render = zoom;
            None
        } else {
            Some(SETTLE_SECONDS - waited)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTENT: (f32, f32) = (595.0, 50_000.0);
    const VIEW: (f32, f32) = (1000.0, 800.0);

    fn doc_point(camera: &Camera, anchor: (f32, f32)) -> (f32, f32) {
        let s = camera.screen_per_pt();
        (
            anchor.0 / s - camera.doc_left_pt(CONTENT.0, VIEW.0),
            anchor.1 / s + camera.y.position(),
        )
    }

    #[test]
    fn zooming_keeps_the_point_under_the_cursor() {
        let mut camera = Camera::default();
        camera.y.jump_to(3000.0);
        for (zoom, anchor) in [
            (250.0, (400.0, 300.0)),
            (800.0, (120.0, 700.0)),
            (60.0, (500.0, 100.0)),
        ] {
            let before = doc_point(&camera, anchor);
            camera.zoom_around(zoom, anchor, CONTENT, VIEW);
            let after = doc_point(&camera, anchor);
            // Horizontally the page is centered again when it becomes narrower than the view.
            if CONTENT.0 * camera.screen_per_pt() > VIEW.0 {
                assert!((before.0 - after.0).abs() < 0.01, "{before:?} {after:?}");
            }
            assert!((before.1 - after.1).abs() < 0.01, "{before:?} {after:?}");
        }
    }

    #[test]
    fn narrow_documents_are_centered_and_wide_ones_scroll() {
        let mut camera = Camera::default();
        let left = camera.doc_left_pt(CONTENT.0, VIEW.0);
        assert!((left - (750.0 - 595.0) / 2.0).abs() < 0.01);

        camera.zoom_around(400.0, (500.0, 400.0), CONTENT, VIEW);
        let (max_x, _) = camera.max_scroll(CONTENT, VIEW);
        assert!(max_x > 0.0);
        assert_eq!(camera.doc_left_pt(CONTENT.0, VIEW.0), -camera.x.position());
    }

    #[test]
    fn zoom_is_limited_and_scroll_stays_in_range() {
        let mut camera = Camera::default();
        camera.zoom_around(100_000.0, (0.0, 0.0), CONTENT, VIEW);
        assert_eq!(camera.zoom(), ZOOM_MAX);
        camera.zoom_around(0.001, (1000.0, 800.0), CONTENT, VIEW);
        assert_eq!(camera.zoom(), ZOOM_MIN);
        assert!(camera.x.position() >= 0.0 && camera.y.position() >= 0.0);
    }

    #[test]
    fn steps_go_to_the_next_preset() {
        assert_eq!(step_zoom(100.0, 1), 125.0);
        assert_eq!(step_zoom(100.0, -1), 75.0);
        assert_eq!(step_zoom(110.0, -1), 100.0);
        assert_eq!(step_zoom(1600.0, 1), ZOOM_MAX);
        assert_eq!(step_zoom(10.0, -1), ZOOM_MIN);
    }

    #[test]
    fn fit_zooms() {
        // An A4 page is 793 screen points wide at 100%.
        assert!((fit_width_zoom(595.0, 793.33) - 100.0).abs() < 0.1);
        let fit = fit_page_zoom((595.0, 842.0), (2000.0, 561.5));
        assert!((fit - 50.0).abs() < 0.1, "{fit}");
    }

    #[test]
    fn render_zoom_waits_until_the_gesture_stops() {
        let mut settle = ZoomSettle::new(100.0);
        assert_eq!(settle.update(100.0, 0.0), None);
        // A pinch changes the zoom every frame for half a second.
        let mut t = 1.0;
        let mut zoom = 100.0;
        while t < 1.5 {
            zoom += 2.0;
            assert!(settle.update(zoom, t).is_some());
            assert_eq!(settle.render_zoom(), 100.0);
            t += 1.0 / 144.0;
        }
        // Still for a moment, but not long enough yet.
        assert!(settle.update(zoom, t + 0.05).is_some());
        assert_eq!(settle.update(zoom, t + SETTLE_SECONDS), None);
        assert_eq!(settle.render_zoom(), zoom);
    }

    #[test]
    fn snap_skips_the_wait() {
        let mut settle = ZoomSettle::new(100.0);
        settle.snap(250.0);
        assert_eq!(settle.render_zoom(), 250.0);
        assert_eq!(settle.update(250.0, 0.0), None);
    }
}
