//! Scrolling that looks the same at 60 Hz and 144 Hz.
//!
//! Positions are in PDF points from the top of the document, so a later zoom change keeps
//! the same part of the document on screen.

/// How fast the view catches up with the scroll target (per second). At 18, a wheel step
/// is 95% done after about 170 ms.
const CATCH_UP_RATE: f32 = 18.0;

/// Below this distance (in points) the animation snaps to its target and stops.
const SNAP_PT: f32 = 0.05;

#[derive(Clone, Copy, Debug, Default)]
pub struct SmoothScroll {
    position: f32,
    target: f32,
}

impl SmoothScroll {
    pub fn position(&self) -> f32 {
        self.position
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    pub fn is_moving(&self) -> bool {
        self.position != self.target
    }

    /// Animated scroll, for mouse wheels and keys.
    pub fn scroll_by(&mut self, delta: f32) {
        self.target += delta;
    }

    /// Animated scroll to a position.
    pub fn scroll_to(&mut self, position: f32) {
        self.target = position;
    }

    /// Immediate scroll, for touchpads and scrollbar drags, whose input is already smooth.
    /// A running animation keeps its remaining distance.
    pub fn jump_by(&mut self, delta: f32) {
        self.position += delta;
        self.target += delta;
    }

    pub fn jump_to(&mut self, position: f32) {
        self.position = position;
        self.target = position;
    }

    /// Keeps the position and the target inside `0..=max`.
    pub fn clamp(&mut self, max: f32) {
        let max = max.max(0.0);
        self.position = self.position.clamp(0.0, max);
        self.target = self.target.clamp(0.0, max);
    }

    /// Advances the animation by `dt` seconds. Returns `true` while it is still moving.
    ///
    /// The remaining distance shrinks by `exp(-rate * dt)` per frame. Because that factor
    /// multiplies, 144 small steps end up exactly where 60 larger steps do.
    pub fn update(&mut self, dt: f32) -> bool {
        let remaining = self.target - self.position;
        if remaining.abs() < SNAP_PT {
            self.position = self.target;
            return false;
        }
        let keep = (-CATCH_UP_RATE * dt.max(0.0)).exp();
        self.position = self.target - remaining * keep;
        if (self.target - self.position).abs() < SNAP_PT {
            self.position = self.target;
        }
        self.is_moving()
    }
}

/// Scrolls up and down at a constant speed for a fixed time, to measure frame pacing.
#[derive(Clone, Copy, Debug)]
pub struct AutoScroll {
    remaining_s: f32,
    speed_pt_per_s: f32,
    direction: f32,
}

impl AutoScroll {
    pub fn new(duration_s: f32, speed_pt_per_s: f32) -> Self {
        AutoScroll {
            remaining_s: duration_s,
            speed_pt_per_s,
            direction: 1.0,
        }
    }

    /// Moves `scroll` for one frame, turning around at either end of `0..=max`.
    /// Returns `false` once the time is up.
    pub fn step(&mut self, dt: f32, scroll: &mut SmoothScroll, max: f32) -> bool {
        if self.remaining_s <= 0.0 {
            return false;
        }
        self.remaining_s -= dt;
        scroll.jump_by(self.direction * self.speed_pt_per_s * dt);
        if scroll.position() >= max {
            scroll.jump_to(max.max(0.0));
            self.direction = -1.0;
        } else if scroll.position() <= 0.0 {
            scroll.jump_to(0.0);
            self.direction = 1.0;
        }
        self.remaining_s > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(scroll: &mut SmoothScroll, hz: f32, seconds: f32) {
        let frames = (hz * seconds).round() as usize;
        for _ in 0..frames {
            scroll.update(1.0 / hz);
        }
    }

    #[test]
    fn same_motion_at_60_and_144_hz() {
        // Multiples of 1/12 s are whole frames at both rates (5 and 12 frames).
        for seconds in [1.0 / 12.0, 2.0 / 12.0, 3.0 / 12.0] {
            let mut at_60 = SmoothScroll::default();
            let mut at_144 = SmoothScroll::default();
            at_60.scroll_by(1000.0);
            at_144.scroll_by(1000.0);
            run(&mut at_60, 60.0, seconds);
            run(&mut at_144, 144.0, seconds);
            let (a, b) = (at_60.position(), at_144.position());
            assert!(
                (a - b).abs() < 0.5,
                "after {seconds}s: 60 Hz at {a}, 144 Hz at {b}"
            );
        }
    }

    #[test]
    fn settles_exactly_on_the_target_and_stops() {
        let mut scroll = SmoothScroll::default();
        scroll.scroll_by(300.0);
        assert!(scroll.update(1.0 / 144.0));
        run(&mut scroll, 144.0, 2.0);
        assert_eq!(scroll.position(), 300.0);
        assert!(!scroll.update(1.0 / 144.0));
    }

    #[test]
    fn a_wheel_step_is_mostly_done_within_200_ms() {
        let mut scroll = SmoothScroll::default();
        scroll.scroll_by(100.0);
        run(&mut scroll, 144.0, 0.2);
        assert!(scroll.position() > 95.0, "{}", scroll.position());
    }

    #[test]
    fn touchpad_jump_keeps_a_running_animation() {
        let mut scroll = SmoothScroll::default();
        scroll.scroll_by(100.0);
        scroll.update(1.0 / 144.0);
        let remaining = scroll.target() - scroll.position();
        scroll.jump_by(20.0);
        assert_eq!(scroll.target() - scroll.position(), remaining);
    }

    #[test]
    fn clamp_keeps_both_ends_in_range() {
        let mut scroll = SmoothScroll::default();
        scroll.scroll_by(-50.0);
        scroll.clamp(1000.0);
        assert_eq!(scroll.target(), 0.0);
        scroll.jump_to(5000.0);
        scroll.clamp(1000.0);
        assert_eq!((scroll.position(), scroll.target()), (1000.0, 1000.0));
        scroll.clamp(-10.0); // document shorter than the window
        assert_eq!(scroll.position(), 0.0);
    }

    #[test]
    fn auto_scroll_bounces_and_stops_after_its_duration() {
        let mut scroll = SmoothScroll::default();
        let mut test = AutoScroll::new(1.0, 500.0);
        let mut steps = 0;
        let mut max_seen: f32 = 0.0;
        while test.step(1.0 / 144.0, &mut scroll, 200.0) {
            steps += 1;
            max_seen = max_seen.max(scroll.position());
            assert!((0.0..=200.0).contains(&scroll.position()));
        }
        assert!((140..=145).contains(&steps), "{steps}");
        assert_eq!(max_seen, 200.0);
        assert!(!test.step(1.0 / 144.0, &mut scroll, 200.0));
    }
}
