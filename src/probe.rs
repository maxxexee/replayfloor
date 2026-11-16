use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::MissedTickBehavior;
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::CommitmentLevel;

use crate::sample::{self, Sample};

#[derive(Debug, Clone)]
pub struct ProbeArgs {
    pub endpoint: String,
    pub api_key: String,
    pub out: PathBuf,
    pub interval: Duration,
    pub duration: Option<Duration>,
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Host part of an endpoint URL, used as the TLS server name.
fn host_of(endpoint: &str) -> &str {
    let rest = endpoint.split("://").nth(1).unwrap_or(endpoint);
    rest.split('/').next().unwrap_or(rest)
}

async fn connect(endpoint: &str, api_key: &str) -> Result<GeyserGrpcClient> {
    let mut builder =
        GeyserGrpcClient::build_from_shared(endpoint.to_string())?.x_token(Some(api_key))?;

    // The client crate leaves TLS to the caller, so an https endpoint fails to
    // connect until a TLS config is supplied here.
    if endpoint.starts_with("https://") {
        builder = builder.tls_config(
            tonic::transport::ClientTlsConfig::new()
                .with_webpki_roots()
                .domain_name(host_of(endpoint).to_string()),
        )?;
    }

    builder
        .connect()
        .await
        .with_context(|| format!("connecting to {endpoint}"))
}

/// A single hung call must not stall the loop, so every call is capped.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// One head plus replay-info pair.
///
/// This never returns an error. A failed poll is a data point too, and a probe
/// that exits on the first timeout cannot measure a 48 hour window.
async fn observe(client: &mut GeyserGrpcClient) -> Sample {
    let mut errors: Vec<String> = Vec::new();

    let started = Instant::now();
    let head = match tokio::time::timeout(
        CALL_TIMEOUT,
        client.get_slot(Some(CommitmentLevel::Processed)),
    )
    .await
    {
        Ok(Ok(response)) => Some(response.slot),
        Ok(Err(e)) => {
            errors.push(format!("get_slot: {e}"));
            None
        }
        Err(_) => {
            errors.push(format!("get_slot: no answer in {CALL_TIMEOUT:?}"));
            None
        }
    };
    let head_ms = started.elapsed().as_millis() as u64;

    let started = Instant::now();
    let first_available =
        match tokio::time::timeout(CALL_TIMEOUT, client.subscribe_replay_info()).await {
            Ok(Ok(response)) => response.first_available,
            Ok(Err(e)) => {
                errors.push(format!("replay_info: {e}"));
                None
            }
            Err(_) => {
                errors.push(format!("replay_info: no answer in {CALL_TIMEOUT:?}"));
                None
            }
        };
    let replay_ms = started.elapsed().as_millis() as u64;

    Sample::new(
        now_ms(),
        head,
        first_available,
        Some(head_ms),
        Some(replay_ms),
        if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        },
    )
}

fn log_line(sample: &Sample) {
    match (sample.head, sample.first_available, sample.delta_slots) {
        (Some(head), Some(first), Some(delta)) => println!(
            "[{}] head {head}  first {first}  depth {delta} slots  {}ms",
            sample.ts_ms,
            sample.head_ms.unwrap_or_default()
        ),
        _ => println!(
            "[{}] failed: {}",
            sample.ts_ms,
            sample.error.as_deref().unwrap_or("unknown")
        ),
    }
}

/// Polls head and replay-info on an interval until the duration elapses.
pub async fn run(args: ProbeArgs) -> Result<()> {
    let started = Instant::now();
    let mut client = connect(&args.endpoint, &args.api_key).await?;
    let mut tick = tokio::time::interval(args.interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tick.tick().await;
        let sample = observe(&mut client).await;
        sample::append(&args.out, &sample)?;
        log_line(&sample);

        // Both calls failing usually means the channel is wedged, not that the
        // endpoint is rejecting us. Rebuilding it costs one connection.
        if sample.head.is_none() && sample.first_available.is_none() {
            match connect(&args.endpoint, &args.api_key).await {
                Ok(fresh) => client = fresh,
                Err(e) => eprintln!("[reconnect] {e:#}"),
            }
        }

        if let Some(duration) = args.duration {
            if started.elapsed() >= duration {
                break;
            }
        }
    }

    println!("wrote {}", args.out.display());
    Ok(())
}
