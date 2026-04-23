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
pub mod format;
pub mod identity;
pub mod json;
pub mod model;
pub mod pricing;
pub mod report;
pub mod slack;
pub mod staleness;

use anyhow::Context;
use aws_config::{Region, SdkConfig};
use aws_sdk_cloudwatch::Client as CwClient;
use aws_sdk_ec2::Client as Ec2Client;
use futures::stream::{self, StreamExt, TryStreamExt};
use jiff::Timestamp;
use std::collections::HashMap;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::aws::errors::map_aws_operation_error;
use crate::cli::{Cli, Command, ExplainArgs};
use crate::config::FileConfig;
use crate::contract::ContractView;
use crate::error::Error;
use crate::model::{BurnRate, CostBreakdown, InstanceRecord, InstanceState, VolumeCost, VolumeInfo};
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
            "Updated EC2 pricing table with {count} instance types in ec2_prices.json"
        );
        return Ok(0);
    }

    let file_cfg = config::load(cli.config.as_deref())?;

    let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;

    let targets = resolve_targets(&sdk_config, cli.all_regions).await?;

    // Fail fast on a bad Slack config, before any AWS queries run.
    let slack_cfg: Option<slack::SlackRuntimeConfig> = if cli.slack || cli.slack_only {
        Some(slack::SlackRuntimeConfig::resolve(
            &file_cfg.slack,
            std::env::var("GASLEAK_SLACK_WEBHOOK").ok(),
        )?)
    } else {
        None
    };
    let slack_only = cli.slack_only;

    match cli.command.unwrap_or(Command::List) {
        Command::List => {
            run_list(&targets, &file_cfg, cli.json, slack_cfg.as_ref(), slack_only).await
        }
        Command::Stale => {
            run_stale(&targets, &file_cfg, cli.json, slack_cfg.as_ref(), slack_only).await
        }
        Command::Explain(args) => {
            run_explain(
                &targets,
                args,
                &file_cfg,
                cli.json,
                slack_cfg.as_ref(),
                slack_only,
            )
            .await
        }
    }
}

/// Exit code when `--slack-only` POST fails. Distinct from severity codes
/// (0/1/2) so cron wrappers can distinguish "scan succeeded, Slack broke"
/// from "scan itself flagged severity N".
const SLACK_POST_FAILED_EXIT: i32 = 3;

/// POST a pre-built payload to Slack and translate the outcome into an
/// optional override exit code. Callers already guard with `if let Some`
/// on the runtime config, so this takes a `&SlackRuntimeConfig` directly.
async fn post_to_slack(
    cfg: &slack::SlackRuntimeConfig,
    payload: &serde_json::Value,
    slack_only: bool,
) -> Option<i32> {
    let client = slack::SlackClient::new(cfg.webhook_url.clone());
    match client.post(payload).await {
        Ok(()) => None,
        Err(e) if slack_only => {
            error!(error = %e, "Slack POST failed and --slack-only is set");
            Some(SLACK_POST_FAILED_EXIT)
        }
        Err(e) => {
            warn!(error = %e, "Slack POST failed; keeping the scan's exit code");
            None
        }
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
    slack_cfg: Option<&slack::SlackRuntimeConfig>,
    slack_only: bool,
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
    let burn_rate = compute_fleet_burn_rate(records.iter());

    if !slack_only {
        if json {
            json::emit(
                std::io::stdout(),
                &json::ListOutput::new(&region_names, burn_rate.clone(), &records),
            )?;
        } else {
            report::print_table(&records, &burn_rate);
        }
    }

    if let Some(cfg) = slack_cfg {
        let payload = slack::render_list(&records, &region_names, &burn_rate, cfg, now);
        if let Some(code) = post_to_slack(cfg, &payload, slack_only).await {
            return Ok(code);
        }
    }

    Ok(0)
}

async fn run_stale(
    targets: &[Target],
    file_cfg: &FileConfig,
    json: bool,
    slack_cfg: Option<&slack::SlackRuntimeConfig>,
    slack_only: bool,
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
    // Single is_flagged pass. The filtered row refs drive both the flagged
    // burn-rate aggregation and the Slack render, so we avoid doing the
    // classification twice.
    let flagged_rows = slack::render::collect_flagged(&evaluated);
    let fleet_burn = compute_fleet_burn_rate(evaluated.iter().map(|(r, _, _)| r));
    let flagged_burn = compute_fleet_burn_rate(flagged_rows.iter().map(|(r, _, _)| r));

    if !slack_only {
        if json {
            json::emit(
                std::io::stdout(),
                &json::StaleOutput::from_evaluated(
                    &region_names,
                    &evaluated,
                    fleet_burn.clone(),
                    flagged_burn.clone(),
                ),
            )?;
        } else {
            report::print_stale(&evaluated, &fleet_burn, &flagged_burn);
        }
    }

    if let Some(cfg) = slack_cfg {
        let payload = slack::render_stale(
            &flagged_rows,
            evaluated.len(),
            &region_names,
            &fleet_burn,
            &flagged_burn,
            cfg,
            now,
        );
        if let Some(code) = post_to_slack(cfg, &payload, slack_only).await {
            return Ok(code);
        }
    }

    Ok(worst.exit_code())
}

async fn run_explain(
    targets: &[Target],
    args: ExplainArgs,
    file_cfg: &FileConfig,
    json: bool,
    slack_cfg: Option<&slack::SlackRuntimeConfig>,
    slack_only: bool,
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
    attach_volume_costs_soft(&sdk_for_record, std::slice::from_mut(&mut record), now).await;

    let contract = ContractView::from_tags(&record.tags);
    let rule_trace = trace(&record, &contract, &cfg);

    if !slack_only {
        if json {
            json::emit(
                std::io::stdout(),
                &json::ExplainOutput::from_parts(&record, &contract, &rule_trace),
            )?;
        } else {
            report::print_explain(&record, &contract, &rule_trace);
        }
    }

    if let Some(cfg) = slack_cfg {
        let payload = slack::render_explain(&record, &contract, &rule_trace, cfg, now);
        if let Some(code) = post_to_slack(cfg, &payload, slack_only).await {
            return Ok(code);
        }
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

/// List + transform + cost (compute + EBS) + CPU for a single region. Each
/// enrichment runs against the target's own SDK, so `--all-regions` gets
/// per-region DescribeVolumes + GetMetricData calls rather than one global
/// pass against the discovery region.
async fn gather_region_records(
    target: Target,
    keep_states: &[InstanceState],
    lookback_days: i64,
    now: Timestamp,
) -> anyhow::Result<Vec<InstanceRecord>> {
    let mut records = list_region_records(target.clone(), keep_states, now).await?;
    populate_estimated_cost(&mut records);
    attach_cpu_soft(&target.sdk, &mut records, lookback_days).await;
    attach_volume_costs_soft(&target.sdk, &mut records, now).await;
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

/// Fetch EBS volumes and roll their cost into each record. Soft-fails: on
/// error, `cost_breakdown` stays `None` and `estimated_cost_usd` keeps its
/// compute-only value. Mirrors the `attach_cpu_soft` failure mode.
async fn attach_volume_costs_soft(
    sdk: &SdkConfig,
    records: &mut [InstanceRecord],
    now: Timestamp,
) {
    if records.is_empty() {
        return;
    }
    let ec2 = Ec2Client::new(sdk);
    match aws::ec2::list_volumes(&ec2).await {
        Ok(volumes) => {
            info!(
                volumes = volumes.len(),
                instances = records.len(),
                "computing EBS cost breakdown"
            );
            populate_cost_breakdown(records, &volumes, now);
        }
        Err(e) => {
            warn!(
                error = %e,
                "DescribeVolumes failed. Continuing with compute-only cost. \
                 `cost_breakdown` will be None and `cost_usd` will exclude EBS."
            );
        }
    }
}

/// Populate `cost_breakdown` per record and fold storage into
/// `estimated_cost_usd`. Call after `populate_estimated_cost` so the compute
/// figure is already present.
fn populate_cost_breakdown(
    records: &mut [InstanceRecord],
    volumes: &[VolumeInfo],
    now: Timestamp,
) {
    // Group volumes by attached instance id. A volume attached to N instances
    // (multi-attach io1/io2) appears in N buckets; each instance row then
    // reports 100% of the volume's cost, so the total across rows overstates
    // the account-level bill. Documented as a known limitation in Phase 9c.
    let mut by_instance: HashMap<&str, Vec<&VolumeInfo>> = HashMap::new();
    for v in volumes {
        for inst_id in &v.attached_instance_ids {
            by_instance.entry(inst_id.as_str()).or_default().push(v);
        }
    }

    for r in records.iter_mut() {
        let attached = by_instance
            .remove(r.instance_id.as_str())
            .unwrap_or_default();
        let compute_usd = r.estimated_cost_usd.unwrap_or(0.0);

        let mut storage_usd = 0.0;
        let mut run_rate = 0.0;
        let mut volume_costs = Vec::with_capacity(attached.len());
        for v in attached {
            let vc = compute_volume_cost(v, now);
            storage_usd += vc.total_usd;
            run_rate += compute_volume_run_rate_usd_per_month(v);
            volume_costs.push(vc);
        }

        r.cost_breakdown = Some(CostBreakdown {
            compute_usd,
            storage_usd,
            storage_run_rate_usd_per_month: run_rate,
            volumes: volume_costs,
        });
        r.estimated_cost_usd = Some(compute_usd + storage_usd);
    }
}

fn compute_volume_cost(v: &VolumeInfo, now: Timestamp) -> VolumeCost {
    let age_secs = now
        .as_second()
        .saturating_sub(v.create_time.as_second())
        .max(0);
    let months = pricing::seconds_to_months(age_secs);

    let capacity_usd = pricing::ebs_capacity_cost_usd(&v.volume_type, v.size_gib, months);
    let iops_usd = v
        .iops
        .map(|i| pricing::ebs_iops_cost_usd(&v.volume_type, i, months))
        .unwrap_or(0.0);
    let throughput_usd = v
        .throughput_mibps
        .map(|t| pricing::ebs_throughput_cost_usd(&v.volume_type, t, months))
        .unwrap_or(0.0);
    let total_usd = capacity_usd + iops_usd + throughput_usd;

    // Flag cost components that are known-unmodeled so the explain output
    // doesn't look like a fully-attributed figure.
    let excluded_reason = if v.volume_type == "standard" {
        Some("standard: per-IO charges not modeled (capacity counted)")
    } else if pricing::lookup_ebs_per_gb_month(&v.volume_type).is_none() {
        Some("no EBS rate in price table for this volume type")
    } else {
        None
    };

    VolumeCost {
        volume_id: v.volume_id.clone(),
        volume_type: v.volume_type.clone(),
        size_gib: v.size_gib,
        iops: v.iops,
        throughput_mibps: v.throughput_mibps,
        age_secs,
        capacity_usd,
        iops_usd,
        throughput_usd,
        total_usd,
        excluded_reason,
    }
}

/// Sum projected spend across an iterator of records, returning the rate in
/// five time units. Compute is counted only for instances in `Running` state
/// (stopped boxes don't accrue compute charges); storage is counted whenever
/// `cost_breakdown` is present, since EBS bills regardless of power state.
pub fn compute_fleet_burn_rate<'a>(
    records: impl IntoIterator<Item = &'a InstanceRecord>,
) -> BurnRate {
    let mut hourly = 0.0_f64;
    for r in records {
        let compute_per_hour = if r.state == InstanceState::Running {
            pricing::lookup_price_per_minute(&r.instance_type)
                .map(|per_min| per_min * 60.0)
                .unwrap_or(0.0)
        } else {
            0.0
        };
        let storage_per_hour = r
            .cost_breakdown
            .as_ref()
            .map(|bd| bd.storage_run_rate_usd_per_month / 730.0)
            .unwrap_or(0.0);
        hourly += compute_per_hour + storage_per_hour;
    }
    BurnRate::from_hourly(hourly)
}

/// Projected monthly storage cost at the current provisioning. Same formulas
/// as [`compute_volume_cost`] with `months = 1`.
fn compute_volume_run_rate_usd_per_month(v: &VolumeInfo) -> f64 {
    let capacity = pricing::ebs_capacity_cost_usd(&v.volume_type, v.size_gib, 1.0);
    let iops = v
        .iops
        .map(|i| pricing::ebs_iops_cost_usd(&v.volume_type, i, 1.0))
        .unwrap_or(0.0);
    let throughput = v
        .throughput_mibps
        .map(|t| pricing::ebs_throughput_cost_usd(&v.volume_type, t, 1.0))
        .unwrap_or(0.0);
    capacity + iops + throughput
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LaunchedBySource;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid RFC 3339")
    }

    fn bare_record(id: &str, compute_usd: f64) -> InstanceRecord {
        InstanceRecord {
            instance_id: id.into(),
            launched_by: None,
            launched_by_source: LaunchedBySource::Unknown,
            launch_time: ts("2026-03-23T00:00:00Z"),
            created_at: ts("2026-03-23T00:00:00Z"),
            last_uptime_seconds: 0,
            total_age_seconds: 0,
            instance_type: "t3.micro".into(),
            state: InstanceState::Running,
            region: "us-east-1".into(),
            az: None,
            iam_instance_profile: None,
            key_name: None,
            tags: Default::default(),
            estimated_cost_usd: Some(compute_usd),
            cost_breakdown: None,
            cpu: None,
        }
    }

    fn gp3_volume(
        id: &str,
        attached_to: &str,
        size_gib: i32,
        iops: i32,
        mibps: i32,
        create_time: &str,
    ) -> VolumeInfo {
        VolumeInfo {
            volume_id: id.into(),
            volume_type: "gp3".into(),
            size_gib,
            iops: Some(iops),
            throughput_mibps: Some(mibps),
            create_time: ts(create_time),
            attached_instance_ids: vec![attached_to.into()],
        }
    }

    #[test]
    fn populate_cost_breakdown_sets_total_to_compute_plus_storage() {
        // 30 days since volume CreateTime.
        let now = ts("2026-04-22T00:00:00Z");
        let mut records = vec![bare_record("i-1", 10.0)];
        let volumes = vec![gp3_volume(
            "vol-1",
            "i-1",
            100,
            3000,
            125,
            "2026-03-23T00:00:00Z",
        )];
        populate_cost_breakdown(&mut records, &volumes, now);

        let r = &records[0];
        let bd = r.cost_breakdown.as_ref().expect("breakdown populated");
        assert_eq!(bd.compute_usd, 10.0);
        assert_eq!(bd.volumes.len(), 1);
        // Baseline gp3: no IOPS or throughput overage. Storage = capacity only.
        let v = &bd.volumes[0];
        assert!(v.iops_usd.abs() < 1e-9);
        assert!(v.throughput_usd.abs() < 1e-9);
        assert!((v.total_usd - v.capacity_usd).abs() < 1e-9);
        // estimated_cost_usd = compute + storage exactly.
        let expected = 10.0 + bd.storage_usd;
        assert!((r.estimated_cost_usd.unwrap() - expected).abs() < 1e-9);
    }

    #[test]
    fn populate_cost_breakdown_sums_multiple_volumes_per_instance() {
        let now = ts("2026-04-22T00:00:00Z");
        let mut records = vec![bare_record("i-1", 0.0)];
        let volumes = vec![
            gp3_volume("vol-a", "i-1", 100, 3000, 125, "2026-03-23T00:00:00Z"),
            gp3_volume("vol-b", "i-1", 200, 3000, 125, "2026-03-23T00:00:00Z"),
        ];
        populate_cost_breakdown(&mut records, &volumes, now);

        let bd = records[0].cost_breakdown.as_ref().unwrap();
        assert_eq!(bd.volumes.len(), 2);
        let summed: f64 = bd.volumes.iter().map(|v| v.total_usd).sum();
        assert!((bd.storage_usd - summed).abs() < 1e-9);
    }

    #[test]
    fn populate_cost_breakdown_leaves_empty_breakdown_for_instance_without_volumes() {
        let now = ts("2026-04-22T00:00:00Z");
        let mut records = vec![bare_record("i-orphan", 5.0)];
        populate_cost_breakdown(&mut records, &[], now);

        let bd = records[0].cost_breakdown.as_ref().unwrap();
        assert!(bd.volumes.is_empty());
        assert_eq!(bd.storage_usd, 0.0);
        assert_eq!(bd.compute_usd, 5.0);
        assert_eq!(records[0].estimated_cost_usd, Some(5.0));
    }

    #[test]
    fn compute_volume_cost_flags_standard_with_excluded_reason() {
        let now = ts("2026-04-22T00:00:00Z");
        let v = VolumeInfo {
            volume_id: "vol-magnetic".into(),
            volume_type: "standard".into(),
            size_gib: 50,
            iops: None,
            throughput_mibps: None,
            create_time: ts("2026-03-23T00:00:00Z"),
            attached_instance_ids: vec!["i-1".into()],
        };
        let vc = compute_volume_cost(&v, now);
        assert!(vc.capacity_usd > 0.0, "standard capacity is counted");
        assert_eq!(vc.iops_usd, 0.0);
        assert_eq!(vc.throughput_usd, 0.0);
        assert!(vc.excluded_reason.is_some(), "standard flagged as partial");
    }

    #[test]
    fn compute_volume_cost_flags_unknown_type_with_excluded_reason() {
        let now = ts("2026-04-22T00:00:00Z");
        let v = VolumeInfo {
            volume_id: "vol-future".into(),
            volume_type: "io7".into(),
            size_gib: 100,
            iops: Some(5000),
            throughput_mibps: None,
            create_time: ts("2026-03-23T00:00:00Z"),
            attached_instance_ids: vec!["i-1".into()],
        };
        let vc = compute_volume_cost(&v, now);
        assert_eq!(vc.capacity_usd, 0.0);
        assert_eq!(vc.iops_usd, 0.0);
        assert!(vc.total_usd.abs() < 1e-9);
        assert!(vc.excluded_reason.is_some());
    }

    fn record_with(id: &str, state: InstanceState, storage_per_mo: f64) -> InstanceRecord {
        let mut r = bare_record(id, 0.0);
        r.state = state;
        r.cost_breakdown = Some(CostBreakdown {
            compute_usd: 0.0,
            storage_usd: 0.0,
            storage_run_rate_usd_per_month: storage_per_mo,
            volumes: Vec::new(),
        });
        r
    }

    #[test]
    fn fleet_burn_rate_counts_storage_for_stopped_but_not_compute() {
        // t3.micro has a real on-demand price in the shipped table. Pick a
        // type with no storage breakdown for the compute-only case and one
        // stopped instance to prove its compute rate is skipped but storage
        // still counts.
        let t3_hourly = pricing::lookup_price_per_minute("t3.micro")
            .expect("t3.micro rate present")
            * 60.0;

        let records = [
            record_with("i-run", InstanceState::Running, 73.0),
            record_with("i-stop", InstanceState::Stopped, 73.0),
        ];

        let burn = compute_fleet_burn_rate(records.iter());
        // Running: compute + storage; stopped: storage only.
        // Storage hourly per record: 73 / 730 = 0.1.
        let expected_hourly = t3_hourly + 0.1 + 0.1;
        assert!(
            (burn.hour - expected_hourly).abs() < 1e-9,
            "got {} expected {}",
            burn.hour,
            expected_hourly
        );
    }

    #[test]
    fn fleet_burn_rate_zero_for_empty() {
        let burn = compute_fleet_burn_rate(std::iter::empty());
        assert_eq!(burn.hour, 0.0);
        assert_eq!(burn.month, 0.0);
    }
}
