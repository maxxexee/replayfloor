use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// One observation of the endpoint: the live head, the oldest servable slot,
/// and how long the two calls took. Written as one JSON object per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub ts_ms: i64,
    pub head: Option<u64>,
    pub first_available: Option<u64>,
    pub delta_slots: Option<i64>,
    pub head_ms: Option<u64>,
    pub replay_ms: Option<u64>,
    pub error: Option<String>,
}

impl Sample {
    /// Builds a sample and derives the retention depth from the two slots.
    pub fn new(
        ts_ms: i64,
        head: Option<u64>,
        first_available: Option<u64>,
        head_ms: Option<u64>,
        replay_ms: Option<u64>,
        error: Option<String>,
    ) -> Self {
        let delta_slots = match (head, first_available) {
            (Some(h), Some(f)) => Some(h as i64 - f as i64),
            _ => None,
        };
        Self {
            ts_ms,
            head,
            first_available,
            delta_slots,
            head_ms,
            replay_ms,
            error,
        }
    }
}

/// Appends one record to the JSONL file, creating the file if needed.
pub fn append<T: Serialize>(path: &Path, record: &T) -> Result<()> {
    let line = serde_json::to_string(record).context("serialising record")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Reads a JSONL file back. Blank lines are skipped, malformed lines are an
/// error naming the line number.
pub fn read_all(path: &Path) -> Result<Vec<Sample>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let sample: Sample = serde_json::from_str(line)
            .with_context(|| format!("{} line {}", path.display(), i + 1))?;
        out.push(sample);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_delta_from_slots() {
        let s = Sample::new(1, Some(900), Some(400), Some(12), Some(30), None);
        assert_eq!(s.delta_slots, Some(500));
    }

    #[test]
    fn delta_is_none_when_either_side_is_missing() {
        assert_eq!(
            Sample::new(1, Some(900), None, None, None, None).delta_slots,
            None
        );
        assert_eq!(
            Sample::new(1, None, Some(400), None, None, None).delta_slots,
            None
        );
    }

    #[test]
    fn failed_sample_carries_the_error_and_no_numbers() {
        let s = Sample::new(7, None, None, None, None, Some("unavailable".to_string()));
        assert_eq!(s.error.as_deref(), Some("unavailable"));
        assert_eq!(s.head, None);
        assert_eq!(s.delta_slots, None);
    }

    #[test]
    fn jsonl_round_trips() {
        // Appending is generic, so the same path serves samples and attempts.
        let dir = std::env::temp_dir().join(format!("replayfloor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let _ = std::fs::remove_file(&path);

        let a = Sample::new(1, Some(900), Some(400), Some(1), Some(2), None);
        let b = Sample::new(2, None, None, None, None, Some("boom".to_string()));
        append(&path, &a).unwrap();
        append(&path, &b).unwrap();

        let back = read_all(&path).unwrap();
        assert_eq!(back, vec![a, b]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
