//! Block Kit render layer. Pure functions return `serde_json::Value` so
//! renderers are trivially unit-testable without HTTP stubs.
//!
//! Layout:
//! - [`render_stale`] / [`render_list`] / [`render_explain`] are the public
//!   per-subcommand entry points.
//! - Internal builders (`*_block`, `*_attachment`) assemble Block Kit
//!   primitives in a deliberate visual order: header → money → scan context
//!   → per-severity attachments → footer.
//! - Formatting helpers (`severity_emoji`, `format_reason_tag`, …) keep
//!   presentation concerns out of the domain types.

use jiff::Timestamp;
use serde_json::{Value, json};

use super::SlackRuntimeConfig;
#[cfg(test)]
use super::MentionThreshold;
use crate::contract::ContractView;
use crate::format::usd_compact;
use crate::model::{BurnRate, CostBreakdown, InstanceRecord, VolumeCost};
use crate::staleness::{RuleTrace, Severity, Verdict, is_flagged};

/// Hard ceiling on full row blocks per render call. Slack's per-message
/// limit is 50 blocks; we leave headroom for headers/summaries/footers. If
/// High+Medium alone exceed this, we render the first `MAX_ROW_BLOCKS` and
/// surface the overflow in a banner rather than silently truncating.
const MAX_ROW_BLOCKS: usize = 40;

/// Upper bound on a single mrkdwn section text. Slack's hard limit is 3000
/// chars; we keep a comfortable buffer for the Low-severity rollup.
const TEXT_FIELD_LIMIT: usize = 2500;

type FlaggedRowRef<'a> = &'a (InstanceRecord, ContractView, Vec<Verdict>);

/// Per-severity bucket output from [`bucket_by_severity`]. Tuple: severity,
/// rows rendered as full section blocks, rows compressed into the rollup.
/// Only the Low bucket ever populates the rollup field.
type SeverityBucket<'a> = (Severity, Vec<FlaggedRowRef<'a>>, Vec<FlaggedRowRef<'a>>);

// ───────────────────────── Top-level renderers ─────────────────────────

/// Render a `stale` alert. Callers pass pre-filtered `flagged_rows` (single
/// `is_flagged` pass, no re-filtering here) plus the total `scanned` count.
pub fn render_stale(
    flagged_rows: &[FlaggedRowRef<'_>],
    scanned: usize,
    regions: &[&str],
    fleet_burn: &BurnRate,
    flagged_burn: &BurnRate,
    cfg: &SlackRuntimeConfig,
    now: Timestamp,
) -> Value {
    let flagged = flagged_rows.len();
    let worst = flagged_rows
        .iter()
        .flat_map(|(_, _, v)| v.iter())
        .map(Verdict::severity)
        .max()
        .unwrap_or(Severity::Info);

    let fallback_text = if flagged == 0 {
        format!(
            "gasleak stale: 0 flagged / {scanned} scanned · {} fleet burn",
            usd_compact(fleet_burn.month),
        )
    } else {
        format!(
            "gasleak stale: {flagged} flagged / {scanned} scanned · worst {} · {} potential savings",
            worst.as_str(),
            usd_compact(flagged_burn.month),
        )
    };

    let mut top_blocks: Vec<Value> = Vec::new();
    top_blocks.push(header_block(&format!(
        "{} gasleak stale — {}",
        severity_emoji(worst),
        regions_label(regions),
    )));
    if let Some(url) = &cfg.report_url {
        top_blocks.push(actions_block_single(url, "Open full report"));
    }
    top_blocks.push(summary_section_stale(
        flagged_burn,
        fleet_burn,
        flagged == 0,
    ));
    top_blocks.push(scan_context_block(flagged, scanned, worst, regions));

    let mut attachments: Vec<Value> = Vec::new();

    if flagged_rows.is_empty() {
        // All-clear: no separate "all clear" section (it would duplicate the
        // summary and context blocks). The footer attachment alone carries
        // the green color bar as the visual signal.
        attachments.push(footer_attachment(
            regions,
            scanned,
            now,
            severity_color_all_clear(),
        ));
        return json!({
            "text": fallback_text,
            "blocks": top_blocks,
            "attachments": attachments,
        });
    }

    let (by_sev, overflow_banner) = bucket_by_severity(flagged_rows, cfg.max_flagged_rows);
    for (sev, rows, rollup_rest) in by_sev {
        if rows.is_empty() && rollup_rest.is_empty() {
            continue;
        }
        attachments.push(severity_attachment(sev, &rows, &rollup_rest, cfg));
    }
    if let Some(banner) = overflow_banner {
        attachments.push(json!({
            "color": severity_color(Severity::High),
            "blocks": [ context_block(&banner) ],
        }));
    }
    attachments.push(footer_attachment(regions, scanned, now, FOOTER_NEUTRAL_COLOR));

    json!({
        "text": fallback_text,
        "blocks": top_blocks,
        "attachments": attachments,
    })
}

pub fn render_list(
    records: &[InstanceRecord],
    regions: &[&str],
    burn: &BurnRate,
    cfg: &SlackRuntimeConfig,
    now: Timestamp,
) -> Value {
    let fallback_text = format!(
        "gasleak list: {} instances · {} fleet burn",
        records.len(),
        usd_compact(burn.month),
    );

    let mut top_blocks: Vec<Value> = Vec::new();
    top_blocks.push(header_block(&format!(
        ":clipboard: gasleak list — {}",
        regions_label(regions),
    )));
    if let Some(url) = &cfg.report_url {
        top_blocks.push(actions_block_single(url, "Open full report"));
    }
    top_blocks.push(json!({
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": format!(
                "*{}* fleet burn rate · {} instances · {}/yr",
                usd_compact(burn.month),
                records.len(),
                usd_compact(burn.year),
            )
        }
    }));

    // Top-5 by cost. `list` is informational (no verdicts), so the signal is
    // "where is the money."
    let mut by_cost: Vec<&InstanceRecord> = records.iter().collect();
    by_cost.sort_by(|a, b| {
        b.estimated_cost_usd
            .unwrap_or(-1.0)
            .total_cmp(&a.estimated_cost_usd.unwrap_or(-1.0))
    });
    let top_n: Vec<&InstanceRecord> = by_cost.into_iter().take(5).collect();

    let mut attachments: Vec<Value> = Vec::new();
    if !top_n.is_empty() {
        let mut blocks: Vec<Value> = vec![header_label_block("Top 5 by cost")];
        for r in &top_n {
            blocks.push(row_block_list(r));
        }
        attachments.push(json!({
            "color": "#3182ce",
            "blocks": blocks,
        }));
    }
    attachments.push(footer_attachment(regions, records.len(), now, FOOTER_NEUTRAL_COLOR));

    json!({
        "text": fallback_text,
        "blocks": top_blocks,
        "attachments": attachments,
    })
}

pub fn render_explain(
    record: &InstanceRecord,
    contract: &ContractView,
    rule_trace: &[RuleTrace],
    _cfg: &SlackRuntimeConfig,
    now: Timestamp,
) -> Value {
    let fired: Vec<&Verdict> = rule_trace
        .iter()
        .filter_map(|t| t.result.as_ref().ok())
        .collect();
    let worst = fired
        .iter()
        .copied()
        .map(Verdict::severity)
        .max()
        .unwrap_or(Severity::Info);
    let cost = record.estimated_cost_usd;
    let fallback_text = format!(
        "gasleak explain {} · {} · worst {}",
        record.instance_id,
        format_dollars_option(cost),
        worst.as_str(),
    );

    let top_blocks = vec![
        header_block(&format!(":mag: gasleak explain — {}", record.instance_id)),
        json!({
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!(
                    "`{}` · `{}` · {} · owner: {}\nregion: `{}`",
                    record.instance_id,
                    record.instance_type,
                    format_dollars_option(cost),
                    owner_label(record, contract, false),
                    record.region,
                )
            },
            "accessory": aws_console_button(&record.region, &record.instance_id),
        }),
    ];

    let mut attachments: Vec<Value> = Vec::new();

    // Rule trace, colored by worst fired verdict.
    let mut trace_blocks: Vec<Value> = vec![header_label_block("Rule evaluation")];
    for entry in rule_trace {
        trace_blocks.push(trace_entry_block(entry));
    }
    attachments.push(json!({
        "color": severity_color(worst),
        "blocks": trace_blocks,
    }));

    if let Some(bd) = &record.cost_breakdown
        && !bd.volumes.is_empty()
    {
        attachments.push(storage_attachment(bd));
    }

    attachments.push(footer_attachment(
        &[record.region.as_str()],
        1,
        now,
        FOOTER_NEUTRAL_COLOR,
    ));

    json!({
        "text": fallback_text,
        "blocks": top_blocks,
        "attachments": attachments,
    })
}

// ───────────────────────── Block primitives ─────────────────────────

fn header_block(text: &str) -> Value {
    // plain_text headers cap at 150 chars. Emoji shortcodes render.
    json!({
        "type": "header",
        "text": { "type": "plain_text", "text": truncate(text, 150), "emoji": true }
    })
}

fn header_label_block(text: &str) -> Value {
    // Smaller "sub-header" inside an attachment.
    json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": format!("*{text}*") }
    })
}

fn actions_block_single(url: &str, label: &str) -> Value {
    json!({
        "type": "actions",
        "elements": [
            {
                "type": "button",
                "text": { "type": "plain_text", "text": truncate(label, 75) },
                "url": url
            }
        ]
    })
}

fn summary_section_stale(flagged_burn: &BurnRate, fleet_burn: &BurnRate, all_clear: bool) -> Value {
    let text = if all_clear {
        format!(
            "*No stale instances.* {} fleet burn.",
            usd_compact(fleet_burn.month),
        )
    } else {
        format!(
            "*{}/mo* potential savings · *{}/mo* fleet burn rate",
            usd_compact(flagged_burn.month),
            usd_compact(fleet_burn.month),
        )
    };
    json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": text }
    })
}

fn scan_context_block(flagged: usize, scanned: usize, worst: Severity, regions: &[&str]) -> Value {
    let text = if flagged == 0 {
        format!(
            "✅ 0 flagged / {scanned} scanned · region {}",
            regions_label(regions),
        )
    } else {
        format!(
            "{} *{flagged} flagged* / {scanned} scanned · worst *{}* · region {}",
            severity_emoji(worst),
            worst.as_str(),
            regions_label(regions),
        )
    };
    context_block(&text)
}

fn context_block(text: &str) -> Value {
    json!({
        "type": "context",
        "elements": [
            { "type": "mrkdwn", "text": truncate(text, TEXT_FIELD_LIMIT) }
        ]
    })
}

const FOOTER_NEUTRAL_COLOR: &str = "#a0aec0";

/// Footer context carrying version, regions, relative timestamp, and scan
/// size. Color is passed in so the all-clear path can use green.
/// `<!date^…|fallback>` renders as relative time in Slack's client.
fn footer_attachment(regions: &[&str], scanned: usize, now: Timestamp, color: &str) -> Value {
    let ts = now.as_second();
    let text = format!(
        "v{} · {} · scanned <!date^{ts}^{{date_short_pretty}} at {{time}}|{ts}> · {scanned} instance{}",
        env!("CARGO_PKG_VERSION"),
        regions_label(regions),
        if scanned == 1 { "" } else { "s" },
    );
    json!({
        "color": color,
        "blocks": [ context_block(&text) ]
    })
}

// ───────────────────── Per-severity bucketing ─────────────────────

/// Partition flagged rows into (severity, full-row-bucket, rollup-bucket).
/// Invariants:
/// - High/Medium always go to the full-row bucket (never dropped).
/// - Low respects `max_low_rows`; overflow goes to the rollup bucket.
/// - Across severities, full-row blocks never exceed `MAX_ROW_BLOCKS`. If the
///   cap is reached, surplus urgent rows are counted in `overflow_banner`.
fn bucket_by_severity<'a>(
    rows: &'a [FlaggedRowRef<'a>],
    max_low_rows: usize,
) -> (Vec<SeverityBucket<'a>>, Option<String>) {
    let mut by_sev: std::collections::BTreeMap<Severity, Vec<FlaggedRowRef<'_>>> =
        std::collections::BTreeMap::new();
    for r in rows {
        let worst = r.2.iter().map(Verdict::severity).max().unwrap_or(Severity::Info);
        by_sev.entry(worst).or_default().push(*r);
    }

    // Within each severity, sort by estimated cost desc so the most
    // expensive rows surface first.
    for v in by_sev.values_mut() {
        v.sort_by(|a, b| {
            b.0.estimated_cost_usd
                .unwrap_or(-1.0)
                .total_cmp(&a.0.estimated_cost_usd.unwrap_or(-1.0))
        });
    }

    let mut remaining_block_budget: usize = MAX_ROW_BLOCKS;
    let mut out: Vec<SeverityBucket<'_>> = Vec::new();
    let mut suppressed_urgent = 0usize;

    for sev in [Severity::High, Severity::Medium, Severity::Low] {
        let Some(bucket) = by_sev.remove(&sev) else {
            continue;
        };
        let per_sev_cap = match sev {
            Severity::Low => max_low_rows.min(remaining_block_budget),
            _ => remaining_block_budget,
        };

        let mut full = Vec::new();
        let mut rollup = Vec::new();
        for (i, r) in bucket.iter().enumerate() {
            if i < per_sev_cap {
                full.push(*r);
            } else {
                rollup.push(*r);
            }
        }

        if sev != Severity::Low {
            suppressed_urgent += rollup.len();
        }
        remaining_block_budget = remaining_block_budget.saturating_sub(full.len());

        if sev == Severity::Low {
            out.push((sev, full, rollup));
        } else {
            out.push((sev, full, Vec::new()));
        }
    }

    let overflow_banner = if suppressed_urgent > 0 {
        Some(format!(
            "🚨 {suppressed_urgent} urgent finding{} beyond Slack's block budget. \
             Fleet is in distress — run `gasleak stale` locally for the full picture.",
            if suppressed_urgent == 1 { "" } else { "s" },
        ))
    } else {
        None
    };
    (out, overflow_banner)
}

fn severity_attachment(
    severity: Severity,
    full_rows: &[FlaggedRowRef<'_>],
    rollup_rows: &[FlaggedRowRef<'_>],
    cfg: &SlackRuntimeConfig,
) -> Value {
    let count_total = full_rows.len() + rollup_rows.len();
    let mut blocks: Vec<Value> = vec![header_label_block(&format!(
        "{} {} severity ({count_total})",
        severity_emoji(severity),
        severity.as_str(),
    ))];

    for (record, contract, verdicts) in full_rows {
        blocks.push(row_block_stale(record, contract, verdicts, severity, cfg));
    }

    if !rollup_rows.is_empty() {
        blocks.push(rollup_block(rollup_rows));
    }

    json!({
        "color": severity_color(severity),
        "blocks": blocks,
    })
}

// ───────────────────────── Row builders ─────────────────────────

fn row_block_stale(
    record: &InstanceRecord,
    contract: &ContractView,
    verdicts: &[Verdict],
    severity: Severity,
    cfg: &SlackRuntimeConfig,
) -> Value {
    let should_mention = cfg.mention_threshold.should_mention(severity);
    let owner = owner_label(record, contract, should_mention);
    let reasons: Vec<String> = verdicts
        .iter()
        .filter(|v| !matches!(v, Verdict::Managed { .. }))
        .map(format_reason_tag)
        .collect();

    let text = format!(
        "{} *`{}`* · `{}` · *{}* · owner: {}\n{}",
        severity_emoji(severity),
        record.instance_id,
        record.instance_type,
        format_dollars_option(record.estimated_cost_usd),
        owner,
        reasons.join("  ·  "),
    );

    json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": truncate(&text, TEXT_FIELD_LIMIT) },
        "accessory": aws_console_button(&record.region, &record.instance_id),
    })
}

fn row_block_list(record: &InstanceRecord) -> Value {
    let text = format!(
        "*`{}`* · `{}` · *{}* · region `{}`",
        record.instance_id,
        record.instance_type,
        format_dollars_option(record.estimated_cost_usd),
        record.region,
    );
    json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": text },
        "accessory": aws_console_button(&record.region, &record.instance_id),
    })
}

/// Compact rollup of Low-severity overflow. One section block, dot-separated
/// list sorted by cost desc, text capped at `TEXT_FIELD_LIMIT`. If the
/// concatenated entries would exceed the limit, truncate with `… and N more`
/// so the block stays valid.
fn rollup_block(rows: &[FlaggedRowRef<'_>]) -> Value {
    let total_cost: f64 = rows
        .iter()
        .map(|r| r.0.estimated_cost_usd.unwrap_or(0.0))
        .sum();
    let prefix = format!(
        "*{} more Low-severity findings* ({}/mo combined)\n",
        rows.len(),
        usd_compact(total_cost),
    );

    let mut body = String::new();
    let mut included = 0usize;
    for (i, (record, _, _)) in rows.iter().enumerate() {
        let entry = format!(
            "`{}` {} {}",
            record.instance_id,
            record.instance_type,
            format_dollars_option(record.estimated_cost_usd),
        );
        let separator = if i == 0 { "" } else { "  ·  " };
        if prefix.len() + body.len() + separator.len() + entry.len() >= TEXT_FIELD_LIMIT {
            break;
        }
        body.push_str(separator);
        body.push_str(&entry);
        included += 1;
    }

    let suffix = if included < rows.len() {
        format!("  ·  … and {} more", rows.len() - included)
    } else {
        String::new()
    };

    json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": format!("{prefix}{body}{suffix}") }
    })
}

// ─────────────── Explain trace + storage attachments ───────────────

fn trace_entry_block(entry: &RuleTrace) -> Value {
    // Emoji for fired verdicts carry their own severity/kind signal via
    // `format_reason_tag`. Use bold rule name for fired, italic for skipped —
    // the contrast is enough to distinguish without a misleading ✓/✗ prefix.
    let text = match &entry.result {
        Ok(v) => format!("*{}* — {}", entry.rule, format_reason_tag(v)),
        Err(reason) => format!("_{}_ — skipped: {}", entry.rule, reason.as_str()),
    };
    json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": truncate(&text, TEXT_FIELD_LIMIT) }
    })
}

fn storage_attachment(bd: &CostBreakdown) -> Value {
    let mut body = format!(
        "*Storage ${:.2} since volume create_time · run rate ${:.2}/mo*\n",
        bd.storage_usd, bd.storage_run_rate_usd_per_month,
    );
    for v in &bd.volumes {
        body.push_str(&render_volume_line(v));
        body.push('\n');
    }
    json!({
        "color": "#718096",
        "blocks": [
            {
                "type": "section",
                "text": { "type": "mrkdwn", "text": truncate(&body, TEXT_FIELD_LIMIT) }
            }
        ]
    })
}

fn render_volume_line(v: &VolumeCost) -> String {
    let iops = v.iops.map(|i| i.to_string()).unwrap_or_else(|| "-".into());
    let mibps = v
        .throughput_mibps
        .map(|t| t.to_string())
        .unwrap_or_else(|| "-".into());
    let note = v
        .excluded_reason
        .map(|r| format!(" _({r})_"))
        .unwrap_or_default();
    format!(
        "• `{}` {} {} GiB · iops {iops} · mibps {mibps} · ${:.2}{note}",
        v.volume_id, v.volume_type, v.size_gib, v.total_usd,
    )
}

// ───────────────── Formatting + styling helpers ─────────────────

fn severity_emoji(s: Severity) -> &'static str {
    match s {
        Severity::High => "🚨",
        Severity::Medium => "🔶",
        Severity::Low => "🟡",
        Severity::Info => "🟢",
    }
}

fn severity_color(s: Severity) -> &'static str {
    match s {
        Severity::High => "#c91517",
        Severity::Medium => "#d69e2e",
        Severity::Low => "#ecc94b",
        Severity::Info => "#3182ce",
    }
}

fn severity_color_all_clear() -> &'static str {
    "#38a169"
}

fn format_reason_tag(v: &Verdict) -> String {
    match v {
        Verdict::Managed { controller } => format!("🤖 managed({controller})"),
        Verdict::Expired { overdue_secs, .. } => {
            format!("🛑 expired {} ago", format_short_duration(*overdue_secs))
        }
        Verdict::ExpiringSoon { within_secs, .. } => {
            format!("⏰ expiring in {}", format_short_duration(*within_secs))
        }
        Verdict::Inactive { idle_for_secs, .. } => match idle_for_secs {
            Some(s) => format!("⏸ inactive {}", format_short_duration(*s)),
            None => "⏸ inactive (no activity in window)".to_string(),
        },
        Verdict::LongLived { age_secs } => {
            format!("🕰 long-lived ({})", format_short_duration(*age_secs))
        }
        Verdict::Underutilized { p95_pct, .. } => {
            format!("📉 underutilized (p95 {p95_pct:.1}%)")
        }
        Verdict::NonCompliant { tampered, .. } => {
            if *tampered {
                "🏷 non-compliant (tampered)".to_string()
            } else {
                "🏷 non-compliant".to_string()
            }
        }
    }
}

fn format_short_duration(secs: i64) -> String {
    let abs = secs.unsigned_abs();
    let d = abs / 86_400;
    let h = (abs % 86_400) / 3_600;
    if d > 0 {
        format!("{d}d")
    } else if h > 0 {
        format!("{h}h")
    } else {
        format!("{m}m", m = (abs % 3_600) / 60)
    }
}

fn format_dollars_option(amount: Option<f64>) -> String {
    match amount {
        Some(v) => usd_compact(v),
        None => "$—".to_string(),
    }
}

fn owner_label(record: &InstanceRecord, contract: &ContractView, should_mention: bool) -> String {
    if let Some(slack) = contract.owner_slack.as_deref() {
        return if should_mention {
            // Raw — Slack auto-links `@handle` / `#channel` in mrkdwn.
            slack.to_string()
        } else {
            format!("`{slack}`")
        };
    }
    match record.launched_by.as_deref() {
        Some(v) if !v.is_empty() => format!("`{v}`"),
        _ => "unknown".to_string(),
    }
}

fn aws_console_button(region: &str, instance_id: &str) -> Value {
    let url = format!(
        "https://console.aws.amazon.com/ec2/v2/home?region={region}#InstanceDetails:instanceId={instance_id}"
    );
    json!({
        "type": "button",
        "text": { "type": "plain_text", "text": "AWS Console" },
        "url": url
    })
}

fn regions_label(regions: &[&str]) -> String {
    match regions.len() {
        0 => "unknown".to_string(),
        1 => regions[0].to_string(),
        n if n <= 3 => regions.join(", "),
        n => format!("{n} regions"),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Convenience for callers that still have an un-filtered `evaluated` set
/// in hand. Does one `is_flagged` pass and collects refs.
pub fn collect_flagged<'a>(
    evaluated: &'a [(InstanceRecord, ContractView, Vec<Verdict>)],
) -> Vec<FlaggedRowRef<'a>> {
    evaluated
        .iter()
        .filter(|(_, _, v)| is_flagged(v))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InstanceState, LaunchedBySource};
    use std::collections::BTreeMap;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn cfg() -> SlackRuntimeConfig {
        SlackRuntimeConfig {
            webhook_url: "https://hooks.slack.com/services/AAA/BBB/CCC".into(),
            max_flagged_rows: 3,
            report_url: None,
            mention_threshold: MentionThreshold::High,
        }
    }

    fn record(id: &str, itype: &str, cost: f64, region: &str) -> InstanceRecord {
        InstanceRecord {
            instance_id: id.into(),
            launched_by: Some("alice".into()),
            launched_by_source: LaunchedBySource::Tag,
            launch_time: ts("2026-01-01T00:00:00Z"),
            created_at: ts("2026-01-01T00:00:00Z"),
            last_uptime_seconds: 100 * 86_400,
            total_age_seconds: 100 * 86_400,
            instance_type: itype.into(),
            state: InstanceState::Running,
            region: region.into(),
            az: None,
            iam_instance_profile: None,
            key_name: None,
            tags: BTreeMap::new(),
            estimated_cost_usd: Some(cost),
            cost_breakdown: None,
            cpu: None,
        }
    }

    fn empty_contract() -> ContractView {
        ContractView {
            managed_by_gasleak: false,
            owner: None,
            owner_slack: None,
            expires_at: None,
        }
    }

    fn high_severity_verdicts() -> Vec<Verdict> {
        vec![
            Verdict::Inactive {
                idle_for_secs: Some(40 * 86_400),
                samples: 500,
                window_secs: 30 * 86_400,
                severity: Severity::High,
            },
            Verdict::NonCompliant {
                missing: vec!["ManagedBy", "Owner"],
                tampered: false,
            },
        ]
    }

    fn low_severity_verdicts() -> Vec<Verdict> {
        vec![Verdict::LongLived {
            age_secs: 120 * 86_400,
        }]
    }

    #[test]
    fn empty_render_drops_all_clear_attachment_duplicate() {
        // Phase 12 #11: scan_context + summary block already convey "all
        // clear"; we don't re-emit a standalone attachment with the same
        // text. Only the footer attachment (now green) remains.
        let burn = BurnRate::from_hourly(42.0);
        let out = render_stale(
            &[],
            36,
            &["us-east-1"],
            &burn,
            &BurnRate::from_hourly(0.0),
            &cfg(),
            ts("2026-04-22T00:00:00Z"),
        );

        let attachments = out["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1, "only the footer should remain");
        assert_eq!(
            attachments[0]["color"], "#38a169",
            "footer uses the all-clear green when nothing flagged",
        );
        let fallback = out["text"].as_str().unwrap();
        assert!(fallback.contains("0 flagged"));
    }

    #[test]
    fn single_high_finding_renders_one_row_block() {
        let r = record("i-001", "c5.large", 1234.0, "us-east-1");
        let flagged = [(r, empty_contract(), high_severity_verdicts())];
        let refs: Vec<FlaggedRowRef<'_>> = flagged.iter().collect();

        let out = render_stale(
            &refs,
            1,
            &["us-east-1"],
            &BurnRate::from_hourly(10.0),
            &BurnRate::from_hourly(2.0),
            &cfg(),
            ts("2026-04-22T00:00:00Z"),
        );
        let attachments = out["attachments"].as_array().unwrap();
        assert_eq!(attachments[0]["color"], "#c91517");
        let blocks = attachments[0]["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 2); // header label + 1 row
        let row = &blocks[1];
        assert!(row["text"]["text"].as_str().unwrap().contains("i-001"));
        assert_eq!(row["accessory"]["type"], "button");
        assert!(row["accessory"]["url"]
            .as_str()
            .unwrap()
            .contains("InstanceDetails:instanceId=i-001"));
    }

    #[test]
    fn low_severity_overflow_compresses_into_rollup() {
        let evaluated: Vec<_> = (0..6)
            .map(|i| {
                (
                    record(
                        &format!("i-{i:03}"),
                        "t3.small",
                        (100.0 + i as f64) * 10.0,
                        "us-east-1",
                    ),
                    empty_contract(),
                    low_severity_verdicts(),
                )
            })
            .collect();
        let refs: Vec<FlaggedRowRef<'_>> = evaluated.iter().collect();

        let out = render_stale(
            &refs,
            6,
            &["us-east-1"],
            &BurnRate::from_hourly(10.0),
            &BurnRate::from_hourly(5.0),
            &cfg(),
            ts("2026-04-22T00:00:00Z"),
        );
        let attachments = out["attachments"].as_array().unwrap();
        let low = attachments
            .iter()
            .find(|a| a["color"] == "#ecc94b")
            .expect("low attachment present");
        let blocks = low["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 5); // header_label + 3 full + 1 rollup
        let rollup = blocks.last().unwrap();
        assert!(rollup["accessory"].is_null());
        let text = rollup["text"]["text"].as_str().unwrap();
        assert!(text.contains("3 more Low-severity findings"));
    }

    #[test]
    fn high_severity_never_compresses_even_with_many_findings() {
        let evaluated: Vec<_> = (0..20)
            .map(|i| {
                (
                    record(
                        &format!("i-{i:03}"),
                        "c5.large",
                        (1000.0 + i as f64) * 10.0,
                        "us-east-1",
                    ),
                    empty_contract(),
                    high_severity_verdicts(),
                )
            })
            .collect();
        let refs: Vec<FlaggedRowRef<'_>> = evaluated.iter().collect();

        let out = render_stale(
            &refs,
            20,
            &["us-east-1"],
            &BurnRate::from_hourly(10.0),
            &BurnRate::from_hourly(5.0),
            &cfg(),
            ts("2026-04-22T00:00:00Z"),
        );
        let attachments = out["attachments"].as_array().unwrap();
        let high = attachments
            .iter()
            .find(|a| a["color"] == "#c91517")
            .expect("high attachment present");
        let blocks = high["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 21); // header_label + 20 rows
    }

    #[test]
    fn row_text_includes_severity_emoji_and_cost() {
        let r = record("i-abc", "m5.large", 987.65, "us-east-1");
        let block = row_block_stale(
            &r,
            &empty_contract(),
            &high_severity_verdicts(),
            Severity::High,
            &cfg(),
        );
        let text = block["text"]["text"].as_str().unwrap();
        assert!(text.contains("🚨"));
        assert!(text.contains("i-abc"));
        assert!(text.contains("$988")); // rounded past $100
        assert!(text.contains("⏸ inactive"));
        assert!(text.contains("🏷 non-compliant"));
    }

    #[test]
    fn report_url_renders_as_actions_block_when_set() {
        let mut c = cfg();
        c.report_url = Some("https://runbook.example.com/gasleak".into());
        let out = render_stale(
            &[],
            0,
            &["us-east-1"],
            &BurnRate::from_hourly(10.0),
            &BurnRate::from_hourly(0.0),
            &c,
            ts("2026-04-22T00:00:00Z"),
        );
        let blocks = out["blocks"].as_array().unwrap();
        let actions = blocks
            .iter()
            .find(|b| b["type"] == "actions")
            .expect("actions block present when report_url is set");
        assert_eq!(
            actions["elements"][0]["url"],
            "https://runbook.example.com/gasleak"
        );
    }

    #[test]
    fn owner_handle_rendered_with_mention_threshold() {
        let mut r = record("i-001", "c5.large", 100.0, "us-east-1");
        r.launched_by = None;
        let mut contract = empty_contract();
        contract.owner_slack = Some("@alice".into());

        let high_block = row_block_stale(
            &r,
            &contract,
            &high_severity_verdicts(),
            Severity::High,
            &cfg(),
        );
        assert!(high_block["text"]["text"]
            .as_str()
            .unwrap()
            .contains("owner: @alice"));

        let low_block = row_block_stale(
            &r,
            &contract,
            &low_severity_verdicts(),
            Severity::Low,
            &cfg(),
        );
        assert!(low_block["text"]["text"]
            .as_str()
            .unwrap()
            .contains("owner: `@alice`"));
    }

    #[test]
    fn trace_emojis_are_outcome_driven_not_checkmark() {
        // Phase 12 #10: fired verdicts used to prefix with ✅ which misread as
        // "rule passed / good". Now we lean on bold-vs-italic rule names and
        // the verdict's own emoji carries the outcome.
        let fired = RuleTrace {
            rule: "inactive",
            result: Ok(Verdict::Inactive {
                idle_for_secs: Some(40 * 86_400),
                samples: 500,
                window_secs: 30 * 86_400,
                severity: Severity::High,
            }),
        };
        let skipped = RuleTrace {
            rule: "managed",
            result: Err(crate::staleness::SkipReason::NoControllerTags),
        };
        let fired_text = trace_entry_block(&fired)["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let skipped_text = trace_entry_block(&skipped)["text"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!fired_text.contains('✅'));
        assert!(fired_text.contains("*inactive*"));
        assert!(fired_text.contains("⏸"));
        assert!(skipped_text.contains("_managed_"));
        assert!(skipped_text.contains("skipped:"));
    }

    #[test]
    fn explain_renders_trace_attachment_with_worst_severity_color() {
        let r = record("i-001", "c5.large", 500.0, "us-east-1");
        let trace = vec![
            RuleTrace {
                rule: "managed",
                result: Err(crate::staleness::SkipReason::NoControllerTags),
            },
            RuleTrace {
                rule: "inactive",
                result: Ok(high_severity_verdicts().remove(0)),
            },
        ];
        let out = render_explain(
            &r,
            &empty_contract(),
            &trace,
            &cfg(),
            ts("2026-04-22T00:00:00Z"),
        );
        let attachments = out["attachments"].as_array().unwrap();
        assert_eq!(attachments[0]["color"], "#c91517");
        let blocks = attachments[0]["blocks"].as_array().unwrap();
        assert!(blocks.len() >= 3);
    }

    #[test]
    fn list_top_5_renders_accessory_button_per_row() {
        let records: Vec<_> = (0..8)
            .map(|i| record(&format!("i-{i:03}"), "t3.small", 100.0 + i as f64, "us-east-1"))
            .collect();
        let out = render_list(
            &records,
            &["us-east-1"],
            &BurnRate::from_hourly(12.0),
            &cfg(),
            ts("2026-04-22T00:00:00Z"),
        );
        let attachments = out["attachments"].as_array().unwrap();
        let rows_attachment = &attachments[0];
        let blocks = rows_attachment["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 6); // header_label + 5 rows
        for b in blocks.iter().skip(1) {
            assert_eq!(b["accessory"]["type"], "button");
        }
    }

    #[test]
    fn rollup_text_never_exceeds_slack_block_limit() {
        let rows: Vec<_> = (0..200)
            .map(|i| {
                (
                    record(
                        &format!("i-padpadpadpad{i:03}"),
                        "m5.24xlarge",
                        i as f64,
                        "us-east-1",
                    ),
                    empty_contract(),
                    low_severity_verdicts(),
                )
            })
            .collect();
        let row_refs: Vec<FlaggedRowRef<'_>> = rows.iter().collect();
        let block = rollup_block(&row_refs);
        let text = block["text"]["text"].as_str().unwrap();
        assert!(
            text.len() <= 3000,
            "rollup text exceeded Slack limit: {} chars",
            text.len()
        );
        assert!(text.contains("… and"));
    }

    #[test]
    fn mixed_severity_attachments_stay_under_ten() {
        let mut evaluated = Vec::new();
        for i in 0..5 {
            evaluated.push((
                record(&format!("i-h{i}"), "c5.large", 1000.0, "us-east-1"),
                empty_contract(),
                high_severity_verdicts(),
            ));
        }
        for i in 0..5 {
            evaluated.push((
                record(&format!("i-l{i}"), "t3.micro", 10.0, "us-east-1"),
                empty_contract(),
                low_severity_verdicts(),
            ));
        }
        let refs: Vec<FlaggedRowRef<'_>> = evaluated.iter().collect();

        let out = render_stale(
            &refs,
            evaluated.len(),
            &["us-east-1"],
            &BurnRate::from_hourly(20.0),
            &BurnRate::from_hourly(10.0),
            &cfg(),
            ts("2026-04-22T00:00:00Z"),
        );
        let attachments = out["attachments"].as_array().unwrap();
        assert!(attachments.len() <= 10);
    }

    /// Live-smoke test. Ignored in `cargo test`; run explicitly with:
    ///   GASLEAK_SMOKE_WEBHOOK=https://hooks.slack.com/... \
    ///     cargo test --lib -- --ignored --nocapture slack::render::tests::live_smoke
    /// Uses the real `render_stale` codepath so whatever lands in Slack is
    /// what the CLI would send.
    #[tokio::test]
    #[ignore = "requires GASLEAK_SMOKE_WEBHOOK; POSTs to a real Slack workspace"]
    async fn live_smoke_posts_realistic_stale() {
        let webhook = match std::env::var("GASLEAK_SMOKE_WEBHOOK") {
            Ok(v) => v,
            Err(_) => return,
        };

        let c = SlackRuntimeConfig {
            webhook_url: webhook.clone(),
            max_flagged_rows: 3,
            report_url: Some("https://github.com/powerslider/gasleak".into()),
            mention_threshold: MentionThreshold::High,
        };

        let mut evaluated = Vec::new();
        let mut high_expensive =
            record("i-0a297863a35b12ebc", "c5.4xlarge", 10_643.14, "us-east-1");
        high_expensive.cost_breakdown = Some(CostBreakdown {
            compute_usd: 6411.09,
            storage_usd: 4232.04,
            storage_run_rate_usd_per_month: 327.68,
            volumes: Vec::new(),
        });
        let mut contract_with_slack = empty_contract();
        contract_with_slack.owner_slack = Some("@austin".into());
        evaluated.push((high_expensive, contract_with_slack, high_severity_verdicts()));
        evaluated.push((
            record("i-0c5cb6fc3747b6233", "m5.4xlarge", 1_519.83, "us-east-1"),
            empty_contract(),
            high_severity_verdicts(),
        ));
        for i in 0..6 {
            evaluated.push((
                record(
                    &format!("i-00low{i:03}"),
                    "t3.small",
                    80.0 + i as f64 * 50.0,
                    "us-east-1",
                ),
                empty_contract(),
                low_severity_verdicts(),
            ));
        }
        let refs: Vec<FlaggedRowRef<'_>> = evaluated.iter().collect();

        let fleet_burn = BurnRate::from_hourly(42.50);
        let flagged_burn = BurnRate::from_hourly(10.68);
        let payload = render_stale(
            &refs,
            evaluated.len(),
            &["us-east-1"],
            &fleet_burn,
            &flagged_burn,
            &c,
            Timestamp::now(),
        );

        let client = super::super::SlackClient::new(webhook);
        client.post(&payload).await.expect("POST to Slack webhook");
    }
}
