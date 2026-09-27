//! Frame timing statistics for the on-screen HUD.

use std::collections::VecDeque;

/// Timing of one frame that was drawn while something was moving.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameSample {
    /// Time since the previous frame started.
    pub interval_ms: f32,
    /// CPU time the UI thread spent on the previous frame.
    pub cpu_ms: f32,
}

/// The most recent frame samples, up to a fixed count.
pub struct FrameStats {
    samples: VecDeque<FrameSample>,
    capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameSummary {
    pub frames: usize,
    /// Average frames per second.
    pub fps: f32,
    /// The rate frames were drawn at, from the median frame interval.
    pub cadence_hz: f32,
    /// The monitor's refresh rate, when the platform reports it. Without it the cadence
    /// stands in, so an app steadily drawing every other refresh would look smooth.
    pub display_hz: Option<f32>,
    pub interval_p50_ms: f32,
    pub interval_p99_ms: f32,
    pub interval_max_ms: f32,
    /// Frames that took more than 1.5 refresh intervals, i.e. the screen showed the
    /// previous frame again. These are the stutters people notice.
    pub missed_frames: usize,
    pub cpu_avg_ms: f32,
    pub cpu_p99_ms: f32,
}

/// CPU budget for the UI thread per frame, leaving the rest of a 144 Hz frame for the GPU.
pub const CPU_BUDGET_MS: f32 = 3.0;

/// Fewer frames than this are not enough to judge smoothness.
pub const MIN_FRAMES: usize = 60;

impl FrameSummary {
    /// The monitor's refresh rate, or the measured cadence when it is unknown.
    pub fn refresh_hz(&self) -> f32 {
        self.display_hz.unwrap_or(self.cadence_hz)
    }

    /// Time available per frame at the monitor's refresh rate.
    pub fn frame_budget_ms(&self) -> f32 {
        1000.0 / self.refresh_hz()
    }

    pub fn missed_percent(&self) -> f32 {
        self.missed_frames as f32 * 100.0 / self.frames as f32
    }

    /// Pass mark: at most 1% missed frames and UI CPU time within budget at the 99th
    /// percentile.
    pub fn is_smooth(&self) -> bool {
        self.frames >= MIN_FRAMES
            && self.missed_percent() <= 1.0
            && self.cpu_p99_ms <= CPU_BUDGET_MS
    }
}

impl FrameStats {
    pub fn new(capacity: usize) -> Self {
        FrameStats {
            // Grows as needed; a large limit must not allocate up front.
            samples: VecDeque::with_capacity(capacity.min(1024)),
            capacity: capacity.max(1),
        }
    }

    pub fn record(&mut self, sample: FrameSample) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Summary of the samples; `display_hz` is the monitor's refresh rate if known.
    pub fn summary(&self, display_hz: Option<f32>) -> Option<FrameSummary> {
        if self.samples.is_empty() {
            return None;
        }
        let mut intervals: Vec<f32> = self.samples.iter().map(|s| s.interval_ms).collect();
        let mut cpu: Vec<f32> = self.samples.iter().map(|s| s.cpu_ms).collect();
        intervals.sort_by(f32::total_cmp);
        cpu.sort_by(f32::total_cmp);

        let frames = intervals.len();
        let mean_interval = intervals.iter().sum::<f32>() / frames as f32;
        let median = percentile(&intervals, 0.5);
        let refresh_interval = display_hz.map_or(median, |hz| 1000.0 / hz);
        Some(FrameSummary {
            frames,
            fps: 1000.0 / mean_interval,
            cadence_hz: 1000.0 / median,
            display_hz,
            interval_p50_ms: median,
            interval_p99_ms: percentile(&intervals, 0.99),
            interval_max_ms: intervals[frames - 1],
            missed_frames: intervals
                .iter()
                .filter(|&&i| i > refresh_interval * 1.5)
                .count(),
            cpu_avg_ms: cpu.iter().sum::<f32>() / frames as f32,
            cpu_p99_ms: percentile(&cpu, 0.99),
        })
    }
}

/// Nearest-rank percentile of sorted values.
fn percentile(sorted: &[f32], q: f32) -> f32 {
    let rank = (q * sorted.len() as f32).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_144: f32 = 1000.0 / 144.0;

    fn stats(intervals: &[f32], cpu_ms: f32) -> FrameStats {
        let mut stats = FrameStats::new(1000);
        for &interval_ms in intervals {
            stats.record(FrameSample {
                interval_ms,
                cpu_ms,
            });
        }
        stats
    }

    #[test]
    fn steady_144_hz_is_smooth() {
        let summary = stats(&[FRAME_144; 300], 0.8).summary(None).unwrap();
        assert!((summary.cadence_hz - 144.0).abs() < 0.1);
        assert!((summary.fps - 144.0).abs() < 0.1);
        assert_eq!(summary.missed_frames, 0);
        assert!((summary.frame_budget_ms() - 6.94).abs() < 0.01);
        assert!(summary.is_smooth());
    }

    #[test]
    fn a_few_skipped_frames_fail_the_pass_mark() {
        let mut intervals = vec![FRAME_144; 295];
        intervals.extend([FRAME_144 * 2.0; 5]);
        let summary = stats(&intervals, 0.8).summary(None).unwrap();
        assert_eq!(summary.missed_frames, 5);
        assert!(summary.interval_p99_ms > 13.0);
        assert!(!summary.is_smooth());
    }

    #[test]
    fn one_skipped_frame_in_three_hundred_is_allowed() {
        let mut intervals = vec![FRAME_144; 299];
        intervals.push(FRAME_144 * 2.0);
        let summary = stats(&intervals, 0.8).summary(None).unwrap();
        assert_eq!(summary.missed_frames, 1);
        assert!(summary.is_smooth());
    }

    #[test]
    fn half_rate_on_a_144_hz_monitor_is_not_smooth() {
        // Evenly spaced, so the cadence alone would call it smooth.
        let half = stats(&[FRAME_144 * 2.0; 300], 0.8);
        assert_eq!(half.summary(None).unwrap().missed_frames, 0);
        let summary = half.summary(Some(144.0)).unwrap();
        assert_eq!(summary.missed_frames, 300);
        assert!((summary.frame_budget_ms() - 6.94).abs() < 0.01);
        assert!(!summary.is_smooth());
    }

    #[test]
    fn too_much_ui_cpu_time_fails() {
        let summary = stats(&[FRAME_144; 300], 4.0).summary(None).unwrap();
        assert!(!summary.is_smooth());
    }

    #[test]
    fn too_few_frames_cannot_pass() {
        let summary = stats(&[FRAME_144; 30], 0.5).summary(None).unwrap();
        assert!(!summary.is_smooth());
    }

    #[test]
    fn a_huge_limit_does_not_allocate_up_front() {
        let mut stats = FrameStats::new(usize::MAX);
        stats.record(FrameSample {
            interval_ms: 7.0,
            cpu_ms: 1.0,
        });
        assert_eq!(stats.len(), 1);
    }

    #[test]
    fn keeps_only_the_newest_samples() {
        let mut stats = FrameStats::new(3);
        for i in 0..5 {
            stats.record(FrameSample {
                interval_ms: i as f32,
                cpu_ms: 0.0,
            });
        }
        assert_eq!(stats.len(), 3);
        assert_eq!(stats.summary(None).unwrap().interval_max_ms, 4.0);
        assert!(FrameStats::new(3).summary(None).is_none());
    }
}
