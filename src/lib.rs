//! `gasleak` — surface stale AWS EC2 instances.
//!
//! The library exposes a single async entry point, [`run`], invoked by
//! `src/main.rs`. It dispatches one of three subcommands (`list`, `stale`,
//! `explain`) against the AWS account resolved from the process environment,
//! applies the rule set in [`staleness`], and writes a human table or
//! structured JSON via [`report`] / [`json`].
//!
//! Module layout:
//! - [`aws`] — SDK wrappers (EC2 + CloudWatch) and user-facing error
//!   classification.
//! - [`cli`] — clap-derived command-line interface.
//! - [`config`] — TOML config-file loading with precedence.
//! - [`contract`] — parse the tag contract (`ManagedBy`, `Owner`,
//!   `OwnerSlack`, `ExpiresAt`) into a `ContractView`.
//! - [`error`] — library error type.
//! - [`identity`] — best-effort attribution of an instance to a human.
//! - [`json`] — structured JSON envelopes for `--json`.
//! - [`model`] — core domain types (`InstanceRecord`, `CpuSummary`, …).
//! - [`pricing`] — on-demand pricing lookups and the regeneration helper.
//! - [`report`] — human-readable table renderers.
//! - [`staleness`] — rule engine, verdicts, severity.

pub mod aws;
pub mod cli;
pub mod config;
pub mod contract;
pub mod error;
pub mod identity;
pub mod json;
pub mod model;
pub mod pricing;
pub mod report;
pub mod staleness;

use anyhow::Context;
use aws_config::{Region, SdkConfig};
use aws_sdk_cloudwatch::Client as CwClient;
use aws_sdk_ec2::Client as Ec2Client;
use futures::stream::{self, StreamExt, TryStreamExt};
use jiff::Timestamp;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use crate::aws::errors::map_aws_operation_error;
use crate::cli::{Cli, Command, ExplainArgs};
use crate::config::FileConfig;
use crate::contract::ContractView;
use crate::error::Error;
use crate::model::{InstanceRecord, InstanceState};
use crate::staleness::{
    Config as StaleConfig, SECS_PER_DAY, SECS_PER_HOUR, Severity, Verdict, evaluate, trace,
    worst_severity,
};

/// Default region used to call `DescribeRegions` when `--all-regions` is set
/// and the environment has no region configured. Any enabled region works;
/// `us-east-1` is universally available.
const DEFAULT_DISCOVERY_REGION: &str = "us-east-1";

/// Max regions queried in parallel when `--all-regions` is set. Each target
/// already fans out internally for CloudWatch batches, so keep this modest.
const REGION_CONCURRENCY: usize = 8;

/// A single region plus the per-region `SdkConfig` to use against it.
#[derive(Clone)]
struct Target {
    region: String,
    sdk: SdkConfig,
}

const RUNNING_STATE: &[InstanceState] = &[InstanceState::Running];

/// All non-terminal states (`explain` works on stopped instances too).
const ALL_STATES: &[InstanceState] = &[
    InstanceState::Pending,
    InstanceState::Running,
    InstanceState::ShuttingDown,
    InstanceState::Terminated,
    InstanceState::Stopping,
    InstanceState::Stopped,
    InstanceState::Other,
];

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

    let targets = resolve_targets(&sdk_config, cli.all_regions).await?;

    match cli.command.unwrap_or(Command::List) {
        Command::List => run_list(&targets, &file_cfg, cli.json).await,
        Command::Stale => run_stale(&targets, &file_cfg, cli.json).await,
        Command::Explain(args) => run_explain(&targets, args, &file_cfg, cli.json).await,
    }
}

/// Resolve the regions gasleak will query. When `--all-regions` is set, call
/// `ec2:DescribeRegions` to enumerate regions enabled for the account and
/// derive a per-region `SdkConfig` by cloning the base config with the region
/// overridden. Otherwise require the single region resolved by the default
/// credential/region chain.
async fn resolve_targets(base: &SdkConfig, all_regions: bool) -> anyhow::Result<Vec<Target>> {
    if !all_regions {
        let region = base
            .region()
            .map(|r| r.as_ref().to_string())
            .ok_or(Error::RegionNotConfigured)?;
        return Ok(vec![Target {
            region,
            sdk: base.clone(),
        }]);
    }

    // DescribeRegions needs *some* region for signing; reuse whatever the
    // environment provided, falling back to us-east-1.
    let discovery_region = base
        .region()
        .map(|r| r.as_ref().to_string())
        .unwrap_or_else(|| DEFAULT_DISCOVERY_REGION.to_string());
    let discovery_sdk = base
        .clone()
        .to_builder()
        .region(Region::new(discovery_region.clone()))
        .build();
    let ec2 = Ec2Client::new(&discovery_sdk);

    info!(discovery_region = %discovery_region, "discovering enabled regions");
    let resp = ec2
        .describe_regions()
        .send()
        .await
        .map_err(|e| map_aws_operation_error(Error::aws("ec2:DescribeRegions", e), "list AWS regions"))?;

    let mut targets: Vec<Target> = resp
        .regions()
        .iter()
        .filter_map(|r| r.region_name().map(str::to_string))
        .map(|region| {
            let sdk = base
                .clone()
                .to_builder()
                .region(Region::new(region.clone()))
                .build();
            Target { region, sdk }
        })
        .collect();
    targets.sort_by(|a, b| a.region.cmp(&b.region));

    if targets.is_empty() {
        return Err(anyhow::anyhow!(
            "ec2:DescribeRegions returned no enabled regions"
        ));
    }

    info!(count = targets.len(), "enabled regions discovered");
    Ok(targets)
}

async fn run_list(
    targets: &[Target],
    file_cfg: &FileConfig,
    json: bool,
) -> anyhow::Result<i32> {
    let now = Timestamp::now();
    let cfg = build_stale_config(file_cfg, now);

    let mut records = gather_records_multi(targets, RUNNING_STATE, &cfg, now).await?;

    records.sort_by(|a, b| {
        b.estimated_cost_usd
            .unwrap_or(-1.0)
            .total_cmp(&a.estimated_cost_usd.unwrap_or(-1.0))
            .then_with(|| b.total_age_seconds.cmp(&a.total_age_seconds))
            .then_with(|| a.instance_id.cmp(&b.instance_id))
    });

    let region_names: Vec<&str> = targets.iter().map(|t| t.region.as_str()).collect();

    if json {
        json::emit(
            std::io::stdout(),
            &json::ListOutput::new(&region_names, &records),
        )?;
    } else {
        report::print_table(&records);
    }
    Ok(0)
}

async fn run_stale(
    targets: &[Target],
    file_cfg: &FileConfig,
    json: bool,
) -> anyhow::Result<i32> {
    let now = Timestamp::now();
    let cfg = build_stale_config(file_cfg, now);

    let records = gather_records_multi(targets, RUNNING_STATE, &cfg, now).await?;

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

    let region_names: Vec<&str> = targets.iter().map(|t| t.region.as_str()).collect();

    if json {
        json::emit(
            std::io::stdout(),
            &json::StaleOutput::from_evaluated(&region_names, &evaluated),
        )?;
    } else {
        report::print_stale(&evaluated);
    }
    Ok(worst.exit_code())
}

async fn run_explain(
    targets: &[Target],
    args: ExplainArgs,
    file_cfg: &FileConfig,
    json: bool,
) -> anyhow::Result<i32> {
    let now = Timestamp::now();
    let cfg = build_stale_config(file_cfg, now);

    // List across all targets (no cost/CPU — we only enrich the matched record).
    let per_region: Vec<Vec<InstanceRecord>> = stream::iter(targets.iter().cloned())
        .map(|t| list_region_records(t, ALL_STATES, now))
        .buffer_unordered(REGION_CONCURRENCY)
        .try_collect()
        .await?;
    let mut all_records: Vec<InstanceRecord> = per_region.into_iter().flatten().collect();

    let Some(idx) = all_records
        .iter()
        .position(|r| r.instance_id == args.instance_id)
    else {
        let regions_label = targets
            .iter()
            .map(|t| t.region.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::InstanceNotFound {
            id: args.instance_id,
            region: regions_label,
        }
        .into());
    };

    let mut record = all_records.swap_remove(idx);

    // Enrich against the SDK for the region the instance actually lives in.
    let sdk_for_record = targets
        .iter()
        .find(|t| t.region == record.region)
        .map(|t| t.sdk.clone())
        .expect("record.region came from a target");

    populate_estimated_cost(std::slice::from_mut(&mut record));
    attach_cpu_soft(
        &sdk_for_record,
        std::slice::from_mut(&mut record),
        cfg.cpu_lookback_secs() / SECS_PER_DAY,
    )
    .await;

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

/// Gather enriched records (cost + CPU) from every target, running at most
/// `REGION_CONCURRENCY` regions in parallel. Returns the flattened set.
async fn gather_records_multi(
    targets: &[Target],
    keep_states: &[InstanceState],
    cfg: &StaleConfig,
    now: Timestamp,
) -> anyhow::Result<Vec<InstanceRecord>> {
    let lookback_days = cfg.cpu_lookback_secs() / SECS_PER_DAY;
    let per_region: Vec<Vec<InstanceRecord>> = stream::iter(targets.iter().cloned())
        .map(|t| gather_region_records(t, keep_states, lookback_days, now))
        .buffer_unordered(REGION_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(per_region.into_iter().flatten().collect())
}

/// List + transform + cost + CPU for a single region.
async fn gather_region_records(
    target: Target,
    keep_states: &[InstanceState],
    lookback_days: i64,
    now: Timestamp,
) -> anyhow::Result<Vec<InstanceRecord>> {
    let mut records = list_region_records(target.clone(), keep_states, now).await?;
    populate_estimated_cost(&mut records);
    attach_cpu_soft(&target.sdk, &mut records, lookback_days).await;
    Ok(records)
}

/// List + transform for a single region, skipping cost/CPU enrichment.
async fn list_region_records(
    target: Target,
    keep_states: &[InstanceState],
    now: Timestamp,
) -> anyhow::Result<Vec<InstanceRecord>> {
    let ec2 = Ec2Client::new(&target.sdk);
    info!(region = %target.region, "listing EC2 instances");
    let instances = aws::ec2::list_instances(&ec2)
        .await
        .map_err(|e| map_aws_operation_error(e, "list EC2 instances"))?;
    debug!(region = %target.region, count = instances.len(), "raw instances returned");
    aws::ec2::to_records(instances, &target.region, now, keep_states)
        .context("failed to transform EC2 instances into records")
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
