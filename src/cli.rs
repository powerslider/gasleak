use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "gasleak",
    version,
    about = "Identify stale AWS EC2 instances: owner, uptime, and recent CPU load."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Verbosity: -v=info, -vv=debug.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Regenerate checked-in instance pricing table JSON and exit.
    #[arg(long)]
    pub regenerate_pricing_table: bool,

    /// Pricing source for regeneration (supports file:///..., local paths, or https://...).
    #[arg(long, value_name = "URL_OR_PATH", requires = "regenerate_pricing_table")]
    pub pricing_offer_source: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List running EC2 instances with owner, age, and 14-day CPU activity.
    List(ListArgs),
    /// Evaluate staleness rules and exit non-zero on High/Medium verdicts.
    Stale(StaleArgs),
}

#[derive(Debug, Args, Default)]
pub struct ListArgs {}

#[derive(Debug, Args, Default)]
pub struct StaleArgs {
    /// Migration deadline (RFC 3339). After this date, `non_compliant` upgrades to High.
    #[arg(long, value_name = "RFC3339")]
    pub migration_deadline: Option<String>,
}
