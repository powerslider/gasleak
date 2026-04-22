pub mod aws;
pub mod cli;
pub mod contract;
pub mod error;
pub mod model;
pub mod report;
pub mod staleness;

use aws_config::SdkConfig;
use aws_sdk_cloudwatch::Client as CwClient;
use aws_sdk_ec2::Client as Ec2Client;
use jiff::Timestamp;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, ListArgs, StaleArgs};
use crate::contract::ContractView;
use crate::error::{Error, Result};
use crate::model::{InstanceRecord, InstanceState};
use crate::staleness::{Config as StaleConfig, Severity, Verdict, evaluate, worst_severity};

const DEFAULT_CPU_LOOKBACK_DAYS: i64 = 14;
const RUNNING_STATE: &[InstanceState] = &[InstanceState::Running];

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

pub async fn run(cli: Cli) -> Result<i32> {
    let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let region = sdk_config
        .region()
        .map(|r| r.as_ref().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    match cli.command.unwrap_or(Command::List(ListArgs::default())) {
        Command::List(args) => run_list(&sdk_config, &region, args).await,
        Command::Stale(args) => run_stale(&sdk_config, &region, args).await,
    }
}

async fn run_list(sdk: &SdkConfig, region: &str, args: ListArgs) -> Result<i32> {
    let ec2 = Ec2Client::new(sdk);
    info!(region = %region, "listing EC2 instances");
    let instances = aws::ec2::list_instances(&ec2).await?;
    debug!(count = instances.len(), "raw instances returned");

    let now = Timestamp::now();
    let mut records = aws::ec2::to_records(instances, region, now, RUNNING_STATE)?;

    if args.with_cpu && !records.is_empty() {
        info!(
            lookback_days = DEFAULT_CPU_LOOKBACK_DAYS,
            instances = records.len(),
            "fetching CloudWatch CPU metrics"
        );
        attach_cpu(sdk, &mut records, DEFAULT_CPU_LOOKBACK_DAYS).await?;
    }

    report::print_table(&records, args.with_cpu);
    Ok(0)
}

async fn run_stale(sdk: &SdkConfig, region: &str, args: StaleArgs) -> Result<i32> {
    let ec2 = Ec2Client::new(sdk);
    info!(region = %region, "listing EC2 instances");
    let instances = aws::ec2::list_instances(&ec2).await?;

    let now = Timestamp::now();
    let mut records = aws::ec2::to_records(instances, region, now, RUNNING_STATE)?;

    let cpu_fetched = if args.no_cpu {
        warn!("--no-cpu set: `idle` rule is disabled (no CloudWatch calls).");
        false
    } else if records.is_empty() {
        false
    } else {
        info!(
            lookback_days = DEFAULT_CPU_LOOKBACK_DAYS,
            instances = records.len(),
            "fetching CloudWatch CPU metrics"
        );
        attach_cpu(sdk, &mut records, DEFAULT_CPU_LOOKBACK_DAYS).await?;
        true
    };

    let cfg = build_stale_config(&args, now)?;

    let mut evaluated: Vec<(InstanceRecord, ContractView, Vec<Verdict>)> = records
        .into_iter()
        .map(|r| {
            let c = ContractView::from_tags(&r.tags);
            let verdicts = evaluate(&r, &c, &cfg);
            (r, c, verdicts)
        })
        .collect();

    if cfg.migration_deadline.is_none() {
        let has_legacy = evaluated.iter().any(|(_, _, verdicts)| {
            verdicts.iter().any(|v| {
                matches!(
                    v,
                    Verdict::NonCompliant {
                        tampered: false,
                        ..
                    }
                )
            })
        });
        if has_legacy {
            warn!(
                "no migration_deadline set — legacy `non_compliant` verdicts stay at Low forever; \
                 pass --migration-deadline to escalate."
            );
        }
    }

    evaluated.sort_by(|a, b| {
        let sa = worst_severity(&a.2);
        let sb = worst_severity(&b.2);
        sb.cmp(&sa)
            .then_with(|| b.0.total_age_seconds.cmp(&a.0.total_age_seconds))
            .then_with(|| a.0.instance_id.cmp(&b.0.instance_id))
    });

    let worst = evaluated
        .iter()
        .filter_map(|(_, _, v)| worst_severity(v))
        .max()
        .unwrap_or(Severity::Info);

    report::print_stale(&evaluated, cpu_fetched);
    Ok(worst.exit_code())
}

async fn attach_cpu(
    sdk: &SdkConfig,
    records: &mut [InstanceRecord],
    lookback_days: i64,
) -> Result<()> {
    let cw = CwClient::new(sdk);
    let ids: Vec<String> = records.iter().map(|r| r.instance_id.clone()).collect();
    let fetcher = aws::cloudwatch::CpuFetcher::new(cw);
    let cpu_map = fetcher.fetch(&ids, lookback_days).await?;
    for r in records.iter_mut() {
        r.cpu = cpu_map.get(&r.instance_id).cloned();
    }
    Ok(())
}

fn build_stale_config(args: &StaleArgs, now: Timestamp) -> Result<StaleConfig> {
    let mut cfg = StaleConfig::defaults(now);
    if let Some(raw) = &args.migration_deadline {
        let ts: Timestamp = raw
            .parse()
            .map_err(|e: jiff::Error| Error::InvalidTimestamp(format!("--migration-deadline: {e}")))?;
        cfg.migration_deadline = Some(ts);
    }
    Ok(cfg)
}
