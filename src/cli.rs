use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "gasleak",
    version,
    about = "Identify stale AWS EC2 instances: owner, uptime, and (optionally) recent CPU load."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Verbosity: -v=info, -vv=debug.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List running EC2 instances with owner and uptime.
    List(ListArgs),
    /// Evaluate staleness rules and exit non-zero on High/Medium verdicts.
    Stale(StaleArgs),
}

#[derive(Debug, Args, Default)]
pub struct ListArgs {
    /// Fetch CloudWatch CPU metrics per instance (incurs GetMetricData cost).
    #[arg(long)]
    pub with_cpu: bool,
}

#[derive(Debug, Args, Default)]
pub struct StaleArgs {
    /// Skip CloudWatch CPU fetching (the `idle` rule will be silenced).
    #[arg(long)]
    pub no_cpu: bool,

    /// Migration deadline (RFC 3339). After this date, `non_compliant` upgrades to High.
    #[arg(long, value_name = "RFC3339")]
    pub migration_deadline: Option<String>,
}
