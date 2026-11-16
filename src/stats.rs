use crate::sample::Sample;

/// A retention depth drifts by less than this fraction of its own median, and
/// by less than the absolute floor below, it counts as a fixed window.
const FIXED_DRIFT_FRACTION: f64 = 0.05;
const FIXED_DRIFT_MIN_SLOTS: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// The window neither grows nor shrinks: a fixed buffer.
    Fixed,
    /// The window is filling up or the provider is loosening the floor.
    Growing,
    /// The window is being trimmed under us.
    Shrinking,
    /// Too little usable data to say.
    Unknown,
}

impl Shape {
    pub fn as_str(self) -> &'static str {
        match self {
            Shape::Fixed => "fixed window",
            Shape::Growing => "growing window",
            Shape::Shrinking => "shrinking window",
            Shape::Unknown => "unknown",
        }
    }
}

/// min / median / max over one quantity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub min: f64,
    pub median: f64,
    pub max: f64,
}

impl Range {
    fn of(mut values: Vec<f64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in measured values"));
        let min = values[0];
        let max = values[values.len() - 1];
        Some(Self {
            min,
            median: percentile(&values, 50.0)?,
            max,
        })
    }
}

/// Nearest-rank percentile over an already sorted slice, clamped to the ends.
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    if !(0.0..=100.0).contains(&p) {
        return None;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    Some(sorted[idx])
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub samples: usize,
    pub usable: usize,
    pub errors: usize,
    pub span_secs: f64,
    /// Slots per second observed at the head, over the whole run.
    pub head_rate: Option<f64>,
    pub retention_slots: Option<Range>,
    /// Retention depth converted to minutes using the observed head rate.
    pub retention_minutes: Option<Range>,
    pub drift_slots: Option<i64>,
    pub drift_slots_per_hour: Option<f64>,
    pub shape: Shape,
    pub head_call_ms: Option<Range>,
    pub replay_call_ms: Option<Range>,
    pub first_head: Option<u64>,
    pub last_head: Option<u64>,
    pub first_available_min: Option<u64>,
    pub first_available_max: Option<u64>,
}

/// Reduces a probe log to the numbers the README quotes.
///
/// Only samples that carry both slots count towards the retention statistics;
/// failed samples still count towards `samples` and `errors` so a partial run
/// cannot be mistaken for a clean one.
pub fn summarize(samples: &[Sample]) -> Summary {
    let usable: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.head.is_some() && s.first_available.is_some())
        .collect();

    let span_secs = match (samples.first(), samples.last()) {
        (Some(a), Some(b)) if b.ts_ms > a.ts_ms => (b.ts_ms - a.ts_ms) as f64 / 1000.0,
        _ => 0.0,
    };

    let head_rate = head_rate(&usable);
    let retention_slots = Range::of(
        usable
            .iter()
            .filter_map(|s| s.delta_slots)
            .map(|d| d as f64)
            .collect(),
    );
    let retention_minutes = match (retention_slots, head_rate) {
        (Some(_), Some(rate)) if rate > 0.0 => Range::of(
            usable
                .iter()
                .filter_map(|s| s.delta_slots)
                .map(|d| d as f64 / rate / 60.0)
                .collect(),
        ),
        _ => None,
    };

    let first = usable.first().and_then(|s| s.delta_slots);
    let last = usable.last().and_then(|s| s.delta_slots);
    // Drift is a difference between two observations, so a single usable
    // sample cannot have one, whatever its depth.
    let drift_slots = match (first, last) {
        (Some(f), Some(l)) if usable.len() >= 2 => Some(l - f),
        _ => None,
    };
    let drift_slots_per_hour = match (drift_slots, span_secs) {
        (Some(d), span) if span > 0.0 => Some(d as f64 / span * 3600.0),
        _ => None,
    };

    Summary {
        samples: samples.len(),
        usable: usable.len(),
        errors: samples.iter().filter(|s| s.error.is_some()).count(),
        span_secs,
        head_rate,
        retention_slots,
        retention_minutes,
        drift_slots,
        drift_slots_per_hour,
        shape: classify(drift_slots, retention_slots),
        head_call_ms: Range::of(
            usable
                .iter()
                .filter_map(|s| s.head_ms)
                .map(|v| v as f64)
                .collect(),
        ),
        replay_call_ms: Range::of(
            usable
                .iter()
                .filter_map(|s| s.replay_ms)
                .map(|v| v as f64)
                .collect(),
        ),
        first_head: usable.first().and_then(|s| s.head),
        last_head: usable.last().and_then(|s| s.head),
        first_available_min: usable.iter().filter_map(|s| s.first_available).min(),
        first_available_max: usable.iter().filter_map(|s| s.first_available).max(),
    }
}

/// Slots per second at the head, from the first and last usable sample.
/// Returns None when the two are less than a second apart, because a short
/// baseline gives a rate that is all sampling noise.
fn head_rate(usable: &[&Sample]) -> Option<f64> {
    let first = usable.first()?;
    let last = usable.last()?;
    let span = (last.ts_ms - first.ts_ms) as f64 / 1000.0;
    if span < 1.0 {
        return None;
    }
    let head_delta = last.head? as f64 - first.head? as f64;
    if head_delta <= 0.0 {
        return None;
    }
    Some(head_delta / span)
}

fn classify(drift_slots: Option<i64>, retention: Option<Range>) -> Shape {
    let (Some(drift), Some(retention)) = (drift_slots, retention) else {
        return Shape::Unknown;
    };
    let tolerance = (retention.median * FIXED_DRIFT_FRACTION).max(FIXED_DRIFT_MIN_SLOTS as f64);
    if (drift as f64).abs() <= tolerance {
        Shape::Fixed
    } else if drift > 0 {
        Shape::Growing
    } else {
        Shape::Shrinking
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good(ts_ms: i64, head: u64, first_available: u64) -> Sample {
        Sample::new(
            ts_ms,
            Some(head),
            Some(first_available),
            Some(3),
            Some(9),
            None,
        )
    }

    #[test]
    fn percentile_uses_nearest_rank_and_clamps() {
        let sorted = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&sorted, 0.0), Some(1.0));
        assert_eq!(percentile(&sorted, 50.0), Some(2.0));
        assert_eq!(percentile(&sorted, 100.0), Some(4.0));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&sorted, 101.0), None);
    }

    #[test]
    fn constant_depth_is_a_fixed_window() {
        // 2.5 slots/s, 3000 slots deep, for an hour.
        let samples: Vec<Sample> = (0..60)
            .map(|i| {
                let head = 10_000 + i as u64 * 150;
                good(i * 60_000, head, head - 3000)
            })
            .collect();
        let s = summarize(&samples);
        assert_eq!(s.shape, Shape::Fixed);
        assert_eq!(s.retention_slots.unwrap().median, 3000.0);
        assert_eq!(s.drift_slots, Some(0));
    }

    #[test]
    fn growing_depth_is_flagged() {
        let samples: Vec<Sample> = (0..60)
            .map(|i| {
                let head = 10_000 + i as u64 * 150;
                good(i * 60_000, head, head - (3000 + i as u64 * 10))
            })
            .collect();
        assert_eq!(summarize(&samples).shape, Shape::Growing);
    }

    #[test]
    fn shrinking_depth_is_flagged() {
        let samples: Vec<Sample> = (0..60)
            .map(|i| {
                let head = 10_000 + i as u64 * 150;
                good(i * 60_000, head, head - (9000 - i as u64 * 10))
            })
            .collect();
        assert_eq!(summarize(&samples).shape, Shape::Shrinking);
    }

    #[test]
    fn head_rate_comes_from_the_observed_slots() {
        let samples = vec![good(0, 1000, 0), good(60_000, 1150, 0)];
        let rate = summarize(&samples).head_rate.unwrap();
        assert!((rate - 2.5).abs() < 1e-9, "got {rate}");
    }

    #[test]
    fn retention_minutes_use_the_observed_rate() {
        // 150 slots in 60s is 2.5 slots/s, so 3750 slots is 25 minutes.
        let samples = vec![good(0, 110_000, 106_250), good(60_000, 110_150, 106_400)];
        let s = summarize(&samples);
        let minutes = s.retention_minutes.unwrap().median;
        assert!((minutes - 25.0).abs() < 1e-6, "got {minutes}");
    }

    #[test]
    fn failed_samples_count_as_errors_and_stay_out_of_the_stats() {
        let samples = vec![
            good(0, 1000, 500),
            Sample::new(1, None, None, None, None, Some("timeout".to_string())),
        ];
        let s = summarize(&samples);
        assert_eq!(s.samples, 2);
        assert_eq!(s.usable, 1);
        assert_eq!(s.errors, 1);
        assert_eq!(s.retention_slots.unwrap().median, 500.0);
    }

    #[test]
    fn a_single_sample_has_no_shape_and_no_rate() {
        let s = summarize(&[good(0, 1000, 500)]);
        assert_eq!(s.shape, Shape::Unknown);
        assert_eq!(s.head_rate, None);
        assert_eq!(s.span_secs, 0.0);
    }

    #[test]
    fn empty_input_summarises_to_nothing() {
        let s = summarize(&[]);
        assert_eq!(s.samples, 0);
        assert!(s.retention_slots.is_none());
        assert_eq!(s.shape, Shape::Unknown);
    }
}
