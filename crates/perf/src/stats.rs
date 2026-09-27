//! Small statistics helpers.

/// Nearest-rank percentile (`q` in 0..=1). Returns 0 for no values.
pub fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

pub fn max(values: &[f64]) -> f64 {
    values.iter().copied().fold(0.0, f64::max)
}

/// Share of frame intervals longer than 1.5 refresh intervals (`refresh_ms`): frames the
/// display showed twice, which people see as stutter.
pub fn missed_frames_pct(intervals: &[f64], refresh_ms: f64) -> f64 {
    if intervals.is_empty() {
        return 0.0;
    }
    let missed = intervals.iter().filter(|&&i| i > refresh_ms * 1.5).count();
    missed as f64 * 100.0 / intervals.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 0.5), 50.0);
        assert_eq!(percentile(&v, 0.99), 99.0);
        assert_eq!(percentile(&v, 1.0), 100.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
    }

    #[test]
    fn missed_frames() {
        let mut v = vec![6.94; 98];
        v.extend([13.9, 13.9]);
        assert!((missed_frames_pct(&v, 6.94) - 2.0).abs() < 1e-9);
        assert_eq!(missed_frames_pct(&[], 6.94), 0.0);
        // Drawing every other refresh of a 144 Hz monitor misses every frame.
        assert_eq!(missed_frames_pct(&[13.9; 10], 6.94), 100.0);
    }
}
