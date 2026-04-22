pub mod aws;
pub mod cli;
pub mod contract;
pub mod error;
pub mod model;
pub mod pricing;
pub mod report;
pub mod staleness;

use anyhow::Context;
use aws_config::SdkConfig;
use aws_sdk_cloudwatch::Client as CwClient;
use aws_sdk_ec2::Client as Ec2Client;
use jiff::Timestamp;
use std::error::Error as StdError;
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

pub async fn run(cli: Cli) -> anyhow::Result<i32> {
    if cli.regenerate_pricing_table {
        let count =
            pricing::regenerate_price_table_json(cli.pricing_offer_source.as_deref()).await?;
        println!(
            "Updated instance pricing table with {count} instance types in instance_prices.json"
        );
        return Ok(0);
    }

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

async fn run_list(sdk: &SdkConfig, region: &str, _args: ListArgs) -> anyhow::Result<i32> {
    let ec2 = Ec2Client::new(sdk);
    info!(region = %region, "listing EC2 instances");
    let instances = aws::ec2::list_instances(&ec2)
        .await
        .map_err(|e| map_aws_operation_error(e, "list EC2 instances"))?;
    debug!(count = instances.len(), "raw instances returned");

    let now = Timestamp::now();
    let mut records = aws::ec2::to_records(instances, region, now, RUNNING_STATE)
        .context("failed to transform EC2 instances into records")?;

    for r in &mut records {
        let uptime_minutes = (r.last_uptime_seconds as f64) / 60.0;
        if uptime_minutes.is_sign_negative() {
            r.estimated_cost_usd = None;
            continue;
        }

        r.estimated_cost_usd = pricing::lookup_price_per_minute(&r.instance_type)
            .map(|price_per_minute| uptime_minutes * price_per_minute);
    }

    attach_cpu_soft(sdk, &mut records, DEFAULT_CPU_LOOKBACK_DAYS).await;

    records.sort_by(|a, b| {
        b.estimated_cost_usd
            .unwrap_or(-1.0)
            .total_cmp(&a.estimated_cost_usd.unwrap_or(-1.0))
            .then_with(|| b.total_age_seconds.cmp(&a.total_age_seconds))
            .then_with(|| a.instance_id.cmp(&b.instance_id))
    });

    report::print_table(&records);
    Ok(0)
}

async fn run_stale(sdk: &SdkConfig, region: &str, args: StaleArgs) -> anyhow::Result<i32> {
    let ec2 = Ec2Client::new(sdk);
    info!(region = %region, "listing EC2 instances");
    let instances = aws::ec2::list_instances(&ec2)
        .await
        .map_err(|e| map_aws_operation_error(e, "list EC2 instances"))?;

    let now = Timestamp::now();
    let mut records = aws::ec2::to_records(instances, region, now, RUNNING_STATE)
        .context("failed to transform EC2 instances into records")?;
    attach_cpu_soft(sdk, &mut records, DEFAULT_CPU_LOOKBACK_DAYS).await;

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
                "no migration_deadline set. Legacy `non_compliant` verdicts stay at Low forever. \
                 Pass --migration-deadline to escalate."
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

    report::print_stale(&evaluated);
    Ok(worst.exit_code())
}

async fn attach_cpu_soft(
    sdk: &SdkConfig,
    records: &mut [InstanceRecord],
    lookback_days: i64,
) {
    if records.is_empty() {
        return;
    }
    info!(
        lookback_days = lookback_days,
        instances = records.len(),
        "fetching CloudWatch CPU metrics"
    );
    let cw = CwClient::new(sdk);
    let ids: Vec<String> = records.iter().map(|r| r.instance_id.clone()).collect();
    let fetcher = aws::cloudwatch::CpuFetcher::new(cw);
    match fetcher.fetch(&ids, lookback_days).await {
        Ok(cpu_map) => {
            for r in records.iter_mut() {
                r.cpu = cpu_map.get(&r.instance_id).cloned();
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                "CloudWatch CPU fetch failed. Continuing without CPU data. \
                 `idle` rule will not fire and `last_active` will show `-`."
            );
        }
    }
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

fn map_aws_operation_error(err: crate::error::Error, operation: &str) -> anyhow::Error {
    let chain = error_chain_text(&err).to_ascii_lowercase();

    if is_expired_credentials_error(&chain) {
        return anyhow::anyhow!(
            "AWS credentials appear to be expired while trying to {operation}.\n\
\n\
How to fix:\n\
1. Re-authenticate your profile (SSO): `aws sso login --profile <profile>`\n\
2. Or refresh static credentials: `aws configure --profile <profile>`\n\
3. Verify identity: `aws sts get-caller-identity --profile <profile>`\n\
4. If this persists, confirm your system clock is correct (clock skew can trigger RequestExpired)."
        );
    }

    if is_missing_or_invalid_credentials_error(&chain) {
        return anyhow::anyhow!(
            "AWS credentials are missing or invalid while trying to {operation}.\n\
\n\
How to authenticate correctly:\n\
1. Choose a profile and export it: `export AWS_PROFILE=<profile>`\n\
2. Authenticate with SSO: `aws sso login --profile <profile>`\n\
3. Or configure access keys: `aws configure --profile <profile>`\n\
4. Verify access: `aws sts get-caller-identity --profile <profile>`"
        );
    }

    anyhow::Error::new(err).context(format!("failed to {operation}"))
}

fn error_chain_text(err: &(dyn StdError + 'static)) -> String {
    let mut out = String::new();
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&e.to_string());
        cur = e.source();
    }
    out
}

fn is_expired_credentials_error(chain: &str) -> bool {
    let chain = chain.to_ascii_lowercase();
    chain.contains("requestexpired")
        || chain.contains("request has expired")
        || chain.contains("expiredtoken")
        || chain.contains("token is expired")
}

fn is_missing_or_invalid_credentials_error(chain: &str) -> bool {
    let chain = chain.to_ascii_lowercase();
    chain.contains("authfailure")
        || chain.contains("invalidclienttokenid")
        || chain.contains("unrecognizedclient")
        || chain.contains("unable to locate credentials")
        || chain.contains("no valid credential sources")
        || chain.contains("could not load credentials")
        || chain.contains("aws was not able to validate the provided access credentials")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_expired_credentials_messages() {
        let chain = "service error | unhandled error (RequestExpired) | Request has expired";
        assert!(is_expired_credentials_error(chain));
    }

    #[test]
    fn classifies_invalid_credentials_messages() {
        let chain = "service error | unhandled error (AuthFailure) | validate access credentials";
        assert!(is_missing_or_invalid_credentials_error(chain));
    }

    #[test]
    fn does_not_misclassify_unrelated_messages() {
        let chain = "throttling: rate exceeded";
        assert!(!is_expired_credentials_error(chain));
        assert!(!is_missing_or_invalid_credentials_error(chain));
    }
}
