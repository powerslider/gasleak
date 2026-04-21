use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "gasleak",
    version,
    about = "Identify stale AWS EC2 instances: owner, uptime, and (optionally) recent CPU load."
)]
pub struct Cli {
    /// Include only instances in these states (repeatable). Defaults to `running`.
    #[arg(long = "state", value_name = "STATE", default_values_t = default_states())]
    pub states: Vec<String>,

    /// Fetch CloudWatch CPU metrics per instance (incurs GetMetricData cost).
    #[arg(long)]
    pub with_cpu: bool,

    /// Lookback window in days for CPU metrics.
    #[arg(long, value_name = "DAYS", default_value_t = 14, requires = "with_cpu")]
    pub cpu_lookback_days: i64,

    /// Verbosity: -v=info, -vv=debug.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

fn default_states() -> Vec<String> {
    vec!["running".to_string()]
}
