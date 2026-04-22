pub mod aws;
pub mod cli;
pub mod config;
pub mod contract;
pub mod error;
pub mod json;
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

use crate::cli::{Cli, Command, ExplainArgs, ListArgs, StaleArgs};
use crate::config::FileConfig;
use crate::contract::ContractView;
use crate::error::Error;
use crate::model::{InstanceRecord, InstanceState};
use crate::staleness::{
    Config as StaleConfig, SECS_PER_DAY, SECS_PER_HOUR, Severity, Verdict, evaluate, trace,
    worst_severity,
};

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

    let file_cfg = config::load(cli.config.as_deref())?;

    let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let region = sdk_config
        .region()
        .map(|r| r.as_ref().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    match cli.command.unwrap_or(Command::List(ListArgs::default())) {
        Command::List(args) => run_list(&sdk_config, &region, args, &file_cfg, cli.json).await,
        Command::Stale(args) => run_stale(&sdk_config, &region, args, &file_cfg, cli.json).await,
        Command::Explain(args) => {
            run_explain(&sdk_config, &region, args, &file_cfg, cli.json).await
        }
    }
}

async fn run_list(
    sdk: &SdkConfig,
    region: &str,
    _args: ListArgs,
    file_cfg: &FileConfig,
    json: bool,
) -> anyhow::Result<i32> {
    let ec2 = Ec2Client::new(sdk);
    info!(region = %region, "listing EC2 instances");
    let instances = aws::ec2::list_instances(&ec2)
        .await
        .map_err(|e| map_aws_operation_error(e, "list EC2 instances"))?;
    debug!(count = instances.len(), "raw instances returned");

    let now = Timestamp::now();
    let mut records = aws::ec2::to_records(instances, region, now, RUNNING_STATE)
        .context("failed to transform EC2 instances into records")?;

    populate_estimated_cost(&mut records);

    let cfg = build_stale_config(file_cfg, now);
    attach_cpu_soft(sdk, &mut records, cfg.cpu_lookback_secs() / SECS_PER_DAY).await;

    records.sort_by(|a, b| {
        b.estimated_cost_usd
            .unwrap_or(-1.0)
            .total_cmp(&a.estimated_cost_usd.unwrap_or(-1.0))
            .then_with(|| b.total_age_seconds.cmp(&a.total_age_seconds))
            .then_with(|| a.instance_id.cmp(&b.instance_id))
    });

    if json {
        json::emit(
            std::io::stdout(),
            &json::ListOutput::new(region, &records),
        )?;
    } else {
        report::print_table(&records);
    }
    Ok(0)
}

async fn run_stale(
    sdk: &SdkConfig,
    region: &str,
    _args: StaleArgs,
    file_cfg: &FileConfig,
    json: bool,
) -> anyhow::Result<i32> {
    let ec2 = Ec2Client::new(sdk);
    info!(region = %region, "listing EC2 instances");
    let instances = aws::ec2::list_instances(&ec2)
        .await
        .map_err(|e| map_aws_operation_error(e, "list EC2 instances"))?;

    let now = Timestamp::now();
    let mut records = aws::ec2::to_records(instances, region, now, RUNNING_STATE)
        .context("failed to transform EC2 instances into records")?;

    populate_estimated_cost(&mut records);

    let cfg = build_stale_config(file_cfg, now);
    attach_cpu_soft(sdk, &mut records, cfg.cpu_lookback_secs() / SECS_PER_DAY).await;

    let mut evaluated: Vec<(InstanceRecord, ContractView, Vec<Verdict>)> = records
        .into_iter()
        .map(|r| {
            let c = ContractView::from_tags(&r.tags);
            let verdicts = evaluate(&r, &c, &cfg);
            (r, c, verdicts)
        })
        .collect();

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

    if json {
        json::emit(
            std::io::stdout(),
            &json::StaleOutput::from_evaluated(region, &evaluated),
        )?;
    } else {
        report::print_stale(&evaluated);
    }
    Ok(worst.exit_code())
}

async fn run_explain(
    sdk: &SdkConfig,
    region: &str,
    args: ExplainArgs,
    file_cfg: &FileConfig,
    json: bool,
) -> anyhow::Result<i32> {
    let ec2 = Ec2Client::new(sdk);
    info!(region = %region, id = %args.instance_id, "explaining instance");
    let instances = aws::ec2::list_instances(&ec2)
        .await
        .map_err(|e| map_aws_operation_error(e, "list EC2 instances"))?;

    let now = Timestamp::now();
    // Don't filter by state so `explain` works on stopped instances too.
    let all_states = &[
        InstanceState::Pending,
        InstanceState::Running,
        InstanceState::ShuttingDown,
        InstanceState::Terminated,
        InstanceState::Stopping,
        InstanceState::Stopped,
        InstanceState::Other,
    ];
    let mut records = aws::ec2::to_records(instances, region, now, all_states)
        .context("failed to transform EC2 instances into records")?;

    let idx = records
        .iter()
        .position(|r| r.instance_id == args.instance_id);
    let Some(idx) = idx else {
        return Err(Error::NotFound(format!(
            "instance '{}' not found in region '{}'",
            args.instance_id, region
        ))
        .into());
    };

    // Pull the record out of the vec first, then fetch CPU for just that one
    // record as a 1-element slice. Avoids `split_at_mut` (disallowed).
    let record = records.swap_remove(idx);
    let mut single = vec![record];
    populate_estimated_cost(&mut single);
    let cfg = build_stale_config(file_cfg, now);
    attach_cpu_soft(sdk, &mut single, cfg.cpu_lookback_secs() / SECS_PER_DAY).await;
    let record = single
        .pop()
        .expect("single is the vec we just pushed to and did not drain");

    let contract = ContractView::from_tags(&record.tags);
    let rule_trace = trace(&record, &contract, &cfg);

    if json {
        json::emit(
            std::io::stdout(),
            &json::ExplainOutput::from_parts(&record, &contract, &rule_trace),
        )?;
    } else {
        report::print_explain(&record, &contract, &rule_trace);
    }
    Ok(0)
}

/// Attach CPU data to records. Soft-fails: on error, log a warning and leave
/// records with `cpu = None`, which keeps the `inactive` rule from firing and
/// renders as `-` in the output.
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
                 `inactive` rule will not fire and `last_active` will show `-`."
            );
        }
    }
}

/// Populate `estimated_cost_usd` on each record using the shipped pricing
/// table. Upper-bound best-effort figure: on-demand Linux rate times
/// `last_uptime_seconds`. Unknown (type, region) pairs stay `None`.
fn populate_estimated_cost(records: &mut [InstanceRecord]) {
    for r in records.iter_mut() {
        let uptime_minutes = (r.last_uptime_seconds as f64) / 60.0;
        if uptime_minutes.is_sign_negative() {
            r.estimated_cost_usd = None;
            continue;
        }
        r.estimated_cost_usd = pricing::lookup_price_per_minute(&r.instance_type)
            .map(|price_per_minute| uptime_minutes * price_per_minute);
    }
}

/// Layer file config over code defaults to build the final `StaleConfig`.
fn build_stale_config(file_cfg: &FileConfig, now: Timestamp) -> StaleConfig {
    let mut cfg = StaleConfig::defaults(now);

    if let Some(v) = file_cfg.inactive.low_days {
        cfg.inactive_low_secs = v.saturating_mul(SECS_PER_DAY);
    }
    if let Some(v) = file_cfg.inactive.medium_days {
        cfg.inactive_medium_secs = v.saturating_mul(SECS_PER_DAY);
    }
    if let Some(v) = file_cfg.inactive.high_days {
        cfg.inactive_high_secs = v.saturating_mul(SECS_PER_DAY);
    }
    if let Some(v) = file_cfg.inactive.min_samples {
        cfg.min_cpu_samples = v;
    }
    if let Some(v) = file_cfg.long_lived.age_days {
        cfg.long_lived_age_secs = v.saturating_mul(SECS_PER_DAY);
    }
    if let Some(v) = file_cfg.underutilized.p95_threshold_pct {
        cfg.p95_underutilized_threshold = v;
    }
    if let Some(v) = file_cfg.warn.window_hours {
        cfg.warn_window_secs = v.saturating_mul(SECS_PER_HOUR);
    }

    cfg
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
