mod floor;
mod probe;
mod report;
mod sample;
mod stats;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_ENDPOINT: &str = "https://laserstream-devnet-ewr.helius-rpc.com";

/// Accepts a full URL, a bare region name such as `devnet-ewr`, or nothing.
fn resolve_endpoint(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        DEFAULT_ENDPOINT.to_string()
    } else if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("https://laserstream-{raw}.helius-rpc.com")
    }
}

#[derive(Parser)]
#[command(
    name = "replayfloor",
    version,
    about = "Measures the LaserStream replay window instead of trusting the documented one"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Poll the live head and replay-info on an interval, appending to a JSONL file
    Probe(ProbeCli),
    /// Ask the endpoint for progressively older slots and find the true floor
    Floor(FloorCli),
    /// Summarise a JSONL file written by probe or floor
    Report(ReportCli),
}

#[derive(Parser)]
struct ProbeCli {
    /// gRPC endpoint, for example https://laserstream-mainnet-ewr.helius-rpc.com
    #[arg(long, env = "LASERSTREAM_ENDPOINT", default_value = DEFAULT_ENDPOINT)]
    endpoint: String,

    /// API key. Read from HELIUS_API_KEY when the flag is absent
    #[arg(long, env = "HELIUS_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Where samples are appended, one JSON object per line
    #[arg(long, default_value = "replayfloor.jsonl")]
    out: PathBuf,

    /// Seconds between samples
    #[arg(long, default_value_t = 60)]
    interval_secs: u64,

    /// Stop after this many hours. Omit to run until Ctrl-C
    #[arg(long)]
    hours: Option<f64>,
}

impl ProbeCli {
    fn into_args(self) -> Result<probe::ProbeArgs> {
        let Some(api_key) = self.api_key else {
            bail!("no API key: pass --api-key or set HELIUS_API_KEY");
        };
        if self.interval_secs == 0 {
            bail!("--interval-secs must be at least 1");
        }
        let duration = match self.hours {
            Some(h) if h > 0.0 => Some(Duration::from_secs_f64(h * 3600.0)),
            Some(_) => bail!("--hours must be positive"),
            None => None,
        };
        Ok(probe::ProbeArgs {
            endpoint: resolve_endpoint(&self.endpoint),
            api_key,
            out: self.out,
            interval: Duration::from_secs(self.interval_secs),
            duration,
        })
    }
}

#[derive(Parser)]
struct FloorCli {
    /// gRPC endpoint, for example https://laserstream-mainnet-ewr.helius-rpc.com
    #[arg(long, env = "LASERSTREAM_ENDPOINT", default_value = DEFAULT_ENDPOINT)]
    endpoint: String,

    /// API key. Read from HELIUS_API_KEY when the flag is absent
    #[arg(long, env = "HELIUS_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Where attempts are appended, one JSON object per line
    #[arg(long, default_value = "floor.jsonl")]
    out: PathBuf,

    /// Seconds to wait before calling an attempt a silent drop
    #[arg(long, default_value_t = 5)]
    wait_secs: u64,

    /// Oldest age worth asking for, in slots
    #[arg(long, default_value_t = 2_000_000)]
    max_age: u64,

    /// Stop bisecting once the served and refused ages are this close
    #[arg(long, default_value_t = 1000)]
    tolerance: u64,

    /// Slot offset above which an answer counts as clamped rather than served
    #[arg(long, default_value_t = 200)]
    clamp_tolerance: u64,

    /// Hard cap on attempts, so a hostile endpoint cannot loop forever
    #[arg(long, default_value_t = 40)]
    max_attempts: usize,

    /// Retries for a transient transport failure before calling the run inconclusive
    #[arg(long, default_value_t = 3)]
    retries: usize,

    /// What to subscribe for. Transaction depth need not match slot depth
    #[arg(long, default_value = "slots", value_parser = ["slots", "transactions"])]
    filter: String,
}

impl FloorCli {
    fn into_args(self) -> Result<floor::FloorArgs> {
        let Some(api_key) = self.api_key else {
            bail!("no API key: pass --api-key or set HELIUS_API_KEY");
        };
        if self.wait_secs == 0 {
            bail!("--wait-secs must be at least 1");
        }
        if self.tolerance == 0 {
            bail!("--tolerance must be at least 1 slot, or the search cannot stop");
        }
        let filter = if self.filter == "transactions" {
            floor::Filter::Transactions
        } else {
            floor::Filter::Slots
        };
        Ok(floor::FloorArgs {
            endpoint: resolve_endpoint(&self.endpoint),
            api_key,
            out: self.out,
            wait: Duration::from_secs(self.wait_secs),
            max_age: self.max_age,
            tolerance: self.tolerance,
            clamp_tolerance: self.clamp_tolerance,
            max_attempts: self.max_attempts,
            retries: self.retries,
            filter,
        })
    }
}

#[derive(Parser)]
struct ReportCli {
    /// JSONL file written by probe or floor
    file: PathBuf,

    /// Read the file as floor attempts instead of probe samples
    #[arg(long)]
    attempts: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Probe(args) => {
            let args = args.into_args()?;
            eprintln!(
                "probing {} every {}s, appending to {}",
                args.endpoint,
                args.interval.as_secs(),
                args.out.display()
            );
            tokio::select! {
                result = probe::run(args) => result,
                _ = tokio::signal::ctrl_c() => {
                    println!("interrupted, samples already written");
                    Ok(())
                }
            }
        }
        Command::Floor(args) => floor::run(args.into_args()?).await,
        Command::Report(args) => {
            if args.attempts {
                let rows: Vec<floor::Attempt> = sample::read_all(&args.file)
                    .with_context(|| format!("reading {}", args.file.display()))?;
                print!("{}", floor::from_attempts(&rows)?);
            } else {
                let samples: Vec<sample::Sample> = sample::read_all(&args.file)
                    .with_context(|| format!("reading {}", args.file.display()))?;
                let summary = stats::summarize(&samples);
                print!("{}", report::markdown(&summary));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_region_name_expands_to_the_endpoint() {
        assert_eq!(
            resolve_endpoint("mainnet-tyo"),
            "https://laserstream-mainnet-tyo.helius-rpc.com"
        );
    }

    #[test]
    fn a_full_url_is_left_alone() {
        let url = "https://example.com:443";
        assert_eq!(resolve_endpoint(url), url);
    }

    #[test]
    fn an_empty_value_falls_back_to_the_default() {
        assert_eq!(resolve_endpoint("  "), DEFAULT_ENDPOINT);
    }
}
