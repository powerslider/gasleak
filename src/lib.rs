pub mod aws;
pub mod cli;
pub mod error;
pub mod model;
pub mod report;

use aws_sdk_cloudwatch::Client as CwClient;
use aws_sdk_ec2::Client as Ec2Client;
use jiff::Timestamp;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::error::Result;
use crate::model::InstanceState;

pub fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("gasleak={default}")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

pub async fn run(cli: Cli) -> Result<()> {
    let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let region = sdk_config
        .region()
        .map(|r| r.as_ref().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let ec2 = Ec2Client::new(&sdk_config);
    info!(region = %region, "listing EC2 instances");

    let instances = aws::ec2::list_instances(&ec2).await?;
    debug!(count = instances.len(), "raw instances returned");

    let keep_states = parse_states(&cli.states);
    let now = Timestamp::now();
    let mut records = aws::ec2::to_records(instances, &region, now, &keep_states)?;

    if cli.with_cpu && !records.is_empty() {
        info!(
            lookback_days = cli.cpu_lookback_days,
            instances = records.len(),
            "fetching CloudWatch CPU metrics"
        );
        let cw = CwClient::new(&sdk_config);
        let ids: Vec<String> = records.iter().map(|r| r.instance_id.clone()).collect();
        let fetcher = aws::cloudwatch::CpuFetcher::new(cw);
        let cpu_map = fetcher.fetch(&ids, cli.cpu_lookback_days).await?;
        for r in &mut records {
            r.cpu = cpu_map.get(&r.instance_id).cloned();
        }
    }

    report::print_table(&records, cli.with_cpu);
    Ok(())
}

fn parse_states(raw: &[String]) -> Vec<InstanceState> {
    raw.iter()
        .filter_map(|s| match s.to_ascii_lowercase().as_str() {
            "pending" => Some(InstanceState::Pending),
            "running" => Some(InstanceState::Running),
            "shutting-down" | "shutting_down" => Some(InstanceState::ShuttingDown),
            "terminated" => Some(InstanceState::Terminated),
            "stopping" => Some(InstanceState::Stopping),
            "stopped" => Some(InstanceState::Stopped),
            other => {
                tracing::warn!(state = other, "ignoring unknown --state value");
                None
            }
        })
        .collect()
}
