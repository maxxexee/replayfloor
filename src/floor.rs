use anyhow::{Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use yellowstone_grpc_client::{GeyserGrpcClient, GeyserGrpcClientError, GeyserStream};
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestFilterSlots,
};

use crate::probe::{self, CALL_TIMEOUT};
use crate::sample;

/// What one replay attempt found.
///
/// `Served` means the stream started at the slot that was asked for. `Clamped`
/// means data arrived but from a later slot, which is how a server hides a
/// missed floor: something always arrives, so a probe that only checks for data
/// never notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Served,
    Clamped,
    OutOfRange,
    Silent,
    Failed,
}

impl Verdict {
    fn failed(self) -> bool {
        self != Verdict::Served
    }
}

/// One attempt at a given age, written to the JSONL file as it happens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub ts_ms: i64,
    pub from_slot: u64,
    pub age: u64,
    pub verdict: Verdict,
    pub first_slot: Option<u64>,
    /// `firstAvailable` read right after this attempt. The floor moves forward
    /// as the endpoint purges, so a claim read once at the start drifts out of
    /// date by a few hundred slots over a run and makes the comparison lie.
    pub claimed_floor: Option<u64>,
    pub ms: u64,
    pub error: Option<String>,
}

/// Narrows the oldest servable age. Pure: no network, no clock, so the search
/// strategy is testable on its own.
///
/// Ages are probed by growing 1.5x while they serve, then bisecting between the
/// largest age that served and the smallest that did not.
#[derive(Debug, Clone)]
pub struct Boundary {
    start: u64,
    tolerance: u64,
    max_age: u64,
    served: Option<u64>,
    failed: Option<u64>,
}

impl Boundary {
    pub fn new(start: u64, tolerance: u64, max_age: u64) -> Self {
        Self {
            start,
            tolerance,
            max_age: max_age.max(start),
            served: None,
            failed: None,
        }
    }

    /// The next age worth testing, or None when the answer is already pinned
    /// down to within the tolerance.
    pub fn next(&self) -> Option<u64> {
        match (self.served, self.failed) {
            (None, None) => Some(self.start),
            (Some(served), None) => {
                if served >= self.max_age {
                    return None;
                }
                let grown = (served.saturating_mul(3) / 2).max(served + 1);
                Some(grown.min(self.max_age))
            }
            (served, Some(failed)) => {
                let low = served.unwrap_or(0);
                if failed.saturating_sub(low) <= self.tolerance {
                    return None;
                }
                let mid = low + (failed - low) / 2;
                (mid > low && mid < failed).then_some(mid)
            }
        }
    }

    pub fn record(&mut self, age: u64, verdict: Verdict) {
        if verdict.failed() {
            self.failed = Some(self.failed.map_or(age, |f| f.min(age)));
        } else {
            self.served = Some(self.served.map_or(age, |s| s.max(age)));
        }
    }

    pub fn served(&self) -> Option<u64> {
        self.served
    }

    pub fn failed(&self) -> Option<u64> {
        self.failed
    }
}

#[derive(Debug, Clone)]
pub struct FloorArgs {
    pub endpoint: String,
    pub api_key: String,
    pub out: PathBuf,
    /// How long to wait before calling an attempt a silent drop.
    pub wait: Duration,
    pub max_age: u64,
    pub tolerance: u64,
    /// How far past the requested slot a stream may start and still count as
    /// served. Above this it is a clamp.
    pub clamp_tolerance: u64,
    pub max_attempts: usize,
    /// Retries for a transient transport failure. A network blip must never be
    /// recorded as the bottom of the replay window.
    pub retries: usize,
}

/// Asks for `from_slot` and reports where the stream actually starts.
async fn attempt(
    client: &mut GeyserGrpcClient,
    head: u64,
    age: u64,
    wait: Duration,
    clamp_tolerance: u64,
) -> Attempt {
    let from_slot = head.saturating_sub(age);
    let started = Instant::now();

    let mut slots = HashMap::new();
    slots.insert(
        "any".to_string(),
        SubscribeRequestFilterSlots {
            filter_by_commitment: Some(false),
            interslot_updates: Some(false),
        },
    );
    let request = SubscribeRequest {
        slots,
        from_slot: Some(from_slot),
        ..Default::default()
    };

    let (verdict, first_slot, error) = match client.subscribe_once(request).await {
        Err(e) => classify_error(e),
        Ok(mut stream) => read_first(&mut stream, from_slot, wait, clamp_tolerance).await,
    };

    let claimed_floor = tokio::time::timeout(CALL_TIMEOUT, client.subscribe_replay_info())
        .await
        .ok()
        .and_then(|r| r.ok())
        .and_then(|r| r.first_available);

    Attempt {
        ts_ms: probe::now_ms(),
        from_slot,
        age,
        verdict,
        first_slot,
        claimed_floor,
        ms: started.elapsed().as_millis() as u64,
        error,
    }
}

/// Watches the stream for its first slot update, and only that. Dropping the
/// stream afterwards cancels the subscription.
async fn read_first(
    stream: &mut GeyserStream,
    from_slot: u64,
    wait: Duration,
    clamp_tolerance: u64,
) -> (Verdict, Option<u64>, Option<String>) {
    loop {
        match tokio::time::timeout(wait, stream.next()).await {
            Err(_) => return (Verdict::Silent, None, None),
            Ok(None) => return (Verdict::Failed, None, Some("stream closed".to_string())),
            Ok(Some(Err(status))) => {
                return (classify_status(&status), None, status_message(&status))
            }
            Ok(Some(Ok(update))) => match update.update_oneof {
                Some(UpdateOneof::Slot(slot)) => {
                    let verdict = if slot.slot > from_slot + clamp_tolerance {
                        Verdict::Clamped
                    } else {
                        Verdict::Served
                    };
                    return (verdict, Some(slot.slot), None);
                }
                // Pings and anything the filter did not ask for are ignored.
                _ => continue,
            },
        }
    }
}

fn classify_error(e: GeyserGrpcClientError) -> (Verdict, Option<u64>, Option<String>) {
    match e {
        GeyserGrpcClientError::TonicStatus(status) => {
            (classify_status(&status), None, status_message(&status))
        }
        other => (Verdict::Failed, None, Some(other.to_string())),
    }
}

fn classify_status(status: &tonic::Status) -> Verdict {
    if status.code() == tonic::Code::OutOfRange {
        Verdict::OutOfRange
    } else {
        Verdict::Failed
    }
}

fn status_message(status: &tonic::Status) -> Option<String> {
    (status.code() != tonic::Code::OutOfRange)
        .then(|| format!("{}: {}", status.code(), status.message()))
}

pub async fn run(args: FloorArgs) -> Result<()> {
    let mut client = probe::connect(&args.endpoint, &args.api_key).await?;

    let head = tokio::time::timeout(
        CALL_TIMEOUT,
        client.get_slot(Some(CommitmentLevel::Processed)),
    )
    .await
    .context("head lookup timed out")?
    .context("head lookup")?
    .slot;
    let claimed = tokio::time::timeout(CALL_TIMEOUT, client.subscribe_replay_info())
        .await
        .context("replay info timed out")?
        .context("replay info")?
        .first_available;

    println!("head {head}, claimed floor {claimed:?}");
    let mut boundary = Boundary::new(10_000, args.tolerance, args.max_age);
    let mut attempts = 0usize;
    let mut rows: Vec<Attempt> = Vec::new();
    let mut stopped: Option<String> = None;

    while let Some(age) = boundary.next() {
        if attempts >= args.max_attempts {
            stopped = Some(format!("hit the {} attempt cap", args.max_attempts));
            break;
        }
        attempts += 1;

        let mut row = attempt(&mut client, head, age, args.wait, args.clamp_tolerance).await;
        for retry in 0..args.retries {
            let transient = matches!(row.verdict, Verdict::Failed)
                || (matches!(row.verdict, Verdict::Silent) && retry == 0);
            if !transient {
                break;
            }
            if matches!(row.verdict, Verdict::Failed) {
                // A transport error usually means the channel is done, so a
                // retry on the same one would fail the same way.
                client = probe::connect(&args.endpoint, &args.api_key).await?;
            }
            row = attempt(&mut client, head, age, args.wait, args.clamp_tolerance).await;
        }

        println!(
            "age {:>8} from {} -> {:?}{} in {}ms{}",
            row.age,
            row.from_slot,
            row.verdict,
            row.first_slot
                .map(|s| format!(" at {s}"))
                .unwrap_or_default(),
            row.ms,
            row.error
                .as_deref()
                .map(|e| format!("  {e}"))
                .unwrap_or_default()
        );
        boundary.record(age, row.verdict);
        sample::append(&args.out, &row)?;

        // A transport failure is not a boundary. Recording it as one would
        // report a network blip as the end of the replay window.
        if row.verdict == Verdict::Failed {
            stopped = Some(format!(
                "transport kept failing at age {age}: {}",
                row.error.as_deref().unwrap_or("no message")
            ));
            rows.push(row);
            break;
        }
        rows.push(row);
    }

    let served = boundary.served().unwrap_or(0);
    println!();
    println!(
        "{}",
        summary(
            head,
            served,
            boundary.failed(),
            stopped,
            args.tolerance,
            &rows
        )
    );
    println!("wrote {}", args.out.display());
    Ok(())
}

fn median(values: &[u64]) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    match sorted.len() {
        0 => 0,
        n if n % 2 == 1 => sorted[n / 2],
        n => (sorted[n / 2 - 1] + sorted[n / 2]) / 2,
    }
}

/// Rebuilds the table from a recorded attempts file, so the evidence in
/// `fixtures/` can be checked without an API key or a network.
///
/// The head is fixed for a run, so it comes from `from_slot + age`. The
/// resolution is the gap the search stopped at, which is exactly how the live
/// run computed it.
pub fn from_attempts(rows: &[Attempt]) -> Result<String> {
    let first = rows.first().context("no attempts in this file")?;
    let head = first.from_slot + first.age;
    let served = rows
        .iter()
        .filter(|r| r.verdict == Verdict::Served)
        .map(|r| r.age)
        .max()
        .context("no attempt in this file was served, so there is no floor")?;
    let failed = rows
        .iter()
        .filter(|r| r.verdict.failed())
        .map(|r| r.age)
        .min();
    let tolerance = failed.map(|f| f.saturating_sub(served)).unwrap_or(0);

    Ok(summary(head, served, failed, None, tolerance, rows))
}

fn summary(
    head: u64,
    served: u64,
    failed: Option<u64>,
    stopped: Option<String>,
    tolerance: u64,
    rows: &[Attempt],
) -> String {
    let min = |v: &[u64]| v.iter().copied().min().unwrap_or(0);

    // The claim moves as the endpoint purges, so the comparison uses the last
    // reading taken next to an attempt rather than the one from the start.
    let latest_claim = rows.iter().rev().find_map(|r| r.claimed_floor);
    // What the search actually achieved, which is never wider than the tolerance
    // it was asked for.
    let tolerance = failed
        .map(|f| f.saturating_sub(served))
        .unwrap_or(tolerance);
    let claimed_age = latest_claim.map(|c| head.saturating_sub(c));
    let verdict = match (&stopped, claimed_age, failed) {
        (Some(reason), _, _) => format!("inconclusive: {reason}"),
        (None, Some(claim), Some(fail)) if claim > fail && claim - fail > tolerance => {
            format!(
                "claim is optimistic by {} slots: the stream stops serving before firstAvailable",
                claim - fail
            )
        }
        (None, Some(claim), Some(_)) if claim < served && served - claim > tolerance => {
            format!("claim is conservative by {} slots", served - claim)
        }
        (None, Some(_), Some(_)) => {
            format!("claim holds within the {tolerance} slot resolution of the search")
        }
        (None, Some(_), None) => "the stream was still serving at the age cap".to_string(),
        (None, None, _) => {
            "endpoint reports no floor, so only the served range is known".to_string()
        }
    };
    let resolution = format!("{tolerance} slots (bisection tolerance)");

    let silent = rows.iter().filter(|r| r.verdict == Verdict::Silent).count();
    let clamped = rows
        .iter()
        .filter(|r| r.verdict == Verdict::Clamped)
        .count();
    let latencies: Vec<u64> = rows.iter().map(|r| r.ms).collect();
    let slowest = latencies.iter().copied().max().unwrap_or(0);

    format!(
        "| | |\n|---|---|\n\
         | head at start | {head} |\n\
         | claimed floor | {} |\n\
         | claimed depth | {} slots |\n\
         | verified depth | {served} slots (largest age served exactly) |\n\
         | first failing age | {} |\n\
         | resolution | {resolution} |\n\
         | verdict | {verdict} |\n\
         | attempts | {} ({} silent, {} clamped) |\n\
         | attempt latency | {} / {} / {} ms (min / median / max) |\n",
        latest_claim
            .map(|c| c.to_string())
            .unwrap_or_else(|| "none".to_string()),
        claimed_age
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        failed
            .map(|f| format!("{f} slots (refused or silent)"))
            .unwrap_or_else(|| "not reached".to_string()),
        rows.len(),
        silent,
        clamped,
        min(&latencies),
        median(&latencies),
        slowest,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_starts_at_the_start_age() {
        let b = Boundary::new(1_000, 100, 1_000_000);
        assert_eq!(b.next(), Some(1_000));
    }

    #[test]
    fn served_ages_grow_until_the_cap() {
        let mut b = Boundary::new(1_000, 100, 3_000);
        assert_eq!(b.next(), Some(1_000));
        b.record(1_000, Verdict::Served);
        assert_eq!(b.next(), Some(1_500));
        b.record(1_500, Verdict::Served);
        assert_eq!(b.next(), Some(2_250));
        b.record(2_250, Verdict::Served);
        assert_eq!(b.next(), Some(3_000));
        b.record(3_000, Verdict::Served);
        assert_eq!(b.next(), None);
    }

    #[test]
    fn a_failure_bisects_towards_the_boundary() {
        let mut b = Boundary::new(1_000, 100, 1_000_000);
        b.record(1_000, Verdict::Served);
        b.record(2_000, Verdict::Silent);
        assert_eq!(b.next(), Some(1_500));
        b.record(1_500, Verdict::Served);
        assert_eq!(b.next(), Some(1_750));
        b.record(1_750, Verdict::OutOfRange);
        assert_eq!(b.next(), Some(1_625));
        b.record(1_625, Verdict::Served);
        assert_eq!(
            b.next(),
            Some(1_687),
            "125 slots left to close, still above tolerance"
        );
        b.record(1_687, Verdict::Served);
        assert_eq!(
            b.next(),
            None,
            "63 slots left, inside the 100 slot tolerance"
        );
    }

    #[test]
    fn a_clamp_counts_as_a_failure() {
        let mut b = Boundary::new(1_000, 100, 1_000_000);
        b.record(1_000, Verdict::Clamped);
        assert_eq!(b.failed(), Some(1_000));
        assert_eq!(b.next(), Some(500));
    }

    #[test]
    fn a_first_attempt_that_fails_searches_downwards() {
        let mut b = Boundary::new(1_000, 100, 1_000_000);
        b.record(1_000, Verdict::Silent);
        assert_eq!(b.next(), Some(500));
        b.record(500, Verdict::Served);
        assert_eq!(b.next(), Some(750));
    }

    fn attempt(age: u64, verdict: Verdict) -> Attempt {
        Attempt {
            ts_ms: 0,
            from_slot: 1_000 - age,
            age,
            verdict,
            first_slot: None,
            claimed_floor: Some(100),
            ms: 10,
            error: None,
        }
    }

    #[test]
    fn a_recorded_run_rebuilds_the_same_table() {
        let rows = vec![
            attempt(500, Verdict::Served),
            attempt(900, Verdict::Served),
            attempt(950, Verdict::OutOfRange),
        ];
        let table = from_attempts(&rows).unwrap();
        assert!(table.contains("| head at start | 1000 |"), "{table}");
        assert!(table.contains("| verified depth | 900 slots"), "{table}");
        assert!(table.contains("| resolution | 50 slots"), "{table}");
    }

    #[test]
    fn a_run_with_no_served_attempt_is_not_a_measurement() {
        let rows = vec![attempt(900, Verdict::Silent)];
        assert!(from_attempts(&rows).is_err());
    }

    #[test]
    fn the_result_is_the_largest_served_age() {
        let mut b = Boundary::new(1_000, 100, 1_000_000);
        b.record(1_000, Verdict::Served);
        b.record(5_000, Verdict::Served);
        b.record(9_000, Verdict::Silent);
        assert_eq!(b.served(), Some(5_000));
        assert_eq!(b.failed(), Some(9_000));
    }
}
