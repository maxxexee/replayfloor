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
    /// Summarise a JSONL file written by probe
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
struct ReportCli {
    /// JSONL file written by probe
    file: PathBuf,
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
        Command::Report(args) => {
            let samples = sample::read_all(&args.file)
                .with_context(|| format!("reading {}", args.file.display()))?;
            let summary = stats::summarize(&samples);
            print!("{}", report::markdown(&summary));
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
