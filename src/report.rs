use crate::stats::{Shape, Summary};

/// The documented figure this probe exists to check.
const DOCUMENTED_SLOTS: f64 = 3000.0;
const DOCUMENTED_MINUTES: f64 = 20.0;

fn num(value: f64, decimals: usize) -> String {
    format!("{value:.*}", decimals)
}

fn range(range: Option<crate::stats::Range>, decimals: usize, suffix: &str) -> String {
    match range {
        Some(r) => format!(
            "{} / {} / {}{suffix}",
            num(r.min, decimals),
            num(r.median, decimals),
            num(r.max, decimals)
        ),
        None => "not measured".to_string(),
    }
}

/// Human summary of a probe log, in the shape the README quotes.
pub fn markdown(summary: &Summary) -> String {
    let mut out = String::new();
    out.push_str("| | |\n|---|---|\n");
    out.push_str(&format!(
        "| samples | {} total, {} usable, {} failed |\n",
        summary.samples, summary.usable, summary.errors
    ));
    out.push_str(&format!(
        "| span | {} min |\n",
        num(summary.span_secs / 60.0, 1)
    ));
    out.push_str(&format!(
        "| head rate | {} |\n",
        match summary.head_rate {
            Some(rate) => format!("{} slots/s observed", num(rate, 2)),
            None => "not measurable from this run".to_string(),
        }
    ));
    out.push_str(&format!(
        "| documented retention | {} slots, about {} min |\n",
        num(DOCUMENTED_SLOTS, 0),
        num(DOCUMENTED_MINUTES, 0)
    ));
    out.push_str(&format!(
        "| observed retention | {} slots (min / median / max) |\n",
        range(summary.retention_slots, 0, "")
    ));
    out.push_str(&format!(
        "| same, in minutes | {} |\n",
        range(summary.retention_minutes, 1, " min")
    ));
    out.push_str(&format!(
        "| drift over the run | {} |\n",
        match (summary.drift_slots, summary.drift_slots_per_hour) {
            (Some(total), Some(per_hour)) =>
                format!("{:+} slots, {:+} slots/h", total, per_hour.round() as i64),
            (Some(total), None) => format!("{total:+} slots"),
            _ => "not measured".to_string(),
        }
    ));
    out.push_str(&format!("| window shape | {} |\n", summary.shape.as_str()));
    out.push_str(&format!(
        "| first available | {} |\n",
        match (summary.first_available_min, summary.first_available_max) {
            (Some(a), Some(b)) if a == b => a.to_string(),
            (Some(a), Some(b)) => format!("{a} to {b}"),
            _ => "not measured".to_string(),
        }
    ));
    out.push_str(&format!(
        "| head | {} |\n",
        match (summary.first_head, summary.last_head) {
            (Some(a), Some(b)) => format!("{a} to {b}"),
            _ => "not measured".to_string(),
        }
    ));
    out.push_str(&format!(
        "| get_slot latency | {} ms (min / median / max) |\n",
        range(summary.head_call_ms, 0, "")
    ));
    out.push_str(&format!(
        "| replay_info latency | {} ms (min / median / max) |\n",
        range(summary.replay_call_ms, 0, "")
    ));

    if summary.shape == Shape::Unknown {
        out.push_str("\nTwo or more samples are needed before the shape means anything.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::Sample;
    use crate::stats::summarize;

    fn run() -> Summary {
        let samples: Vec<Sample> = (0..60)
            .map(|i| {
                let head = 500_000_000 + i as u64 * 150;
                Sample::new(
                    i * 60_000,
                    Some(head),
                    Some(head - 3000),
                    Some(20),
                    Some(30),
                    None,
                )
            })
            .collect();
        summarize(&samples)
    }

    #[test]
    fn table_carries_the_measured_and_the_documented_figures() {
        let text = markdown(&run());
        assert!(text.contains("| 3000 slots, about 20 min |"), "{text}");
        assert!(text.contains("| 3000 / 3000 / 3000 slots"), "{text}");
        assert!(text.contains("fixed window"), "{text}");
        assert!(text.contains("2.50 slots/s observed"), "{text}");
    }

    #[test]
    fn empty_summary_says_so_instead_of_printing_zero() {
        let text = markdown(&summarize(&[]));
        assert!(text.contains("not measured"), "{text}");
        assert!(text.contains("unknown"), "{text}");
        assert!(text.contains("Two or more samples"), "{text}");
    }
}
