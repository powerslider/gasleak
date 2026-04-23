//! Command-line interface.
//!
//! [`Cli`] is the top-level `clap::Parser` struct; [`Command`] is the
//! subcommand enum. All flags that apply across subcommands are marked
//! `global = true` so they work whether written before or after the
//! subcommand.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "gasleak",
    version,
    about = "Identify stale AWS EC2 instances: owner, uptime, and recent CPU load."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Path to a gasleak.toml config file. Overrides $GASLEAK_CONFIG and the
    /// default path at $HOME/.config/gasleak/gasleak.toml.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Verbosity: -v=info, -vv=debug.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Emit structured JSON on stdout instead of the human table. Tracing still
    /// goes to stderr so the stdout stream stays pipe-safe.
    #[arg(long, global = true)]
    pub json: bool,

    /// Scan every region enabled on the account. Mirrors `aws ec2 ... --all-regions`:
    /// discovers regions via `DescribeRegions`, then fans out per-region.
    #[arg(long, global = true)]
    pub all_regions: bool,

    /// Post a Block Kit summary to Slack in addition to stdout. Webhook URL
    /// comes from `[slack] webhook_url` in `gasleak.toml` or
    /// `$GASLEAK_SLACK_WEBHOOK`. Errors if neither is set.
    #[arg(long, global = true, group = "slack_mode")]
    pub slack: bool,

    /// Post to Slack, suppress stdout. Intended for cron. Exit code reflects
    /// the scan's severity on success; non-zero if the Slack POST itself fails.
    #[arg(long, global = true, group = "slack_mode")]
    pub slack_only: bool,

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
    List,
    /// Evaluate staleness rules and exit non-zero on High/Medium verdicts.
    Stale,
    /// Show the full rule trace and tag context for one instance.
    Explain(ExplainArgs),
}

#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// Instance ID to explain, for example `i-0abc123`.
    pub instance_id: String,
}
