//! Human-readable renderers for `list`, `stale`, and `explain`.
//!
//! The table printer is hand-rolled (no extra dependency) and auto-sizes
//! columns based on row content. All three entry points (`print_table`,
//! `print_stale`, `print_explain`) consume the same core types from
//! [`crate::model`] and [`crate::staleness`]; see [`crate::json`] for the
//! machine-readable counterpart.

use jiff::Timestamp;

use crate::contract::ContractView;
use crate::model::{CostBreakdown, CpuSummary, InstanceRecord, VolumeCost, format_uptime};
use crate::staleness::{RuleTrace, Severity, Verdict, is_flagged, worst_severity};

fn format_date(ts: Timestamp) -> String {
    let s = ts.to_string();
    s.get(..10).unwrap_or(&s).to_string()
}

fn format_last_active(cpu: &CpuSummary) -> String {
    if cpu.samples == 0 {
        return "no data".to_string();
    }
    match cpu.last_active_at {
        Some(ts) => {
            let now = Timestamp::now();
            let delta = now.as_second().saturating_sub(ts.as_second());
            if delta < 0 {
                "-".to_string()
            } else {
                format!("{} ago", format_uptime(delta))
            }
        }
        // No hour in the actual lookback window crossed the active threshold.
        // "Not within the last N days", not "never ever". The N reflects the
        // configured CPU lookback, which tracks `inactive_high_days`.
        None => format!(">{}d ago", cpu.window_secs / 86_400),
    }
}

pub fn print_table(records: &[InstanceRecord]) {
    if records.is_empty() {
        println!("No EC2 instances matched.");
        return;
    }

    let rows: Vec<ListRow> = records.iter().map(ListRow::from_record).collect();

    let headers: Vec<&'static str> = vec![
        "instance_id",
        "state",
        "type",
        "cost_usd",
        "created",
        "total_age",
        "last_uptime",
        "launched_by",
        "region",
        "p95_cpu",
        "max_cpu",
        "last_active",
    ];

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (i, col) in row.columns().iter().enumerate() {
            if col.len() > widths[i] {
                widths[i] = col.len();
            }
        }
    }

    print_row(&headers, &widths);
    print_separator(&widths);
    for row in &rows {
        print_row(&row.columns(), &widths);
    }
}

pub fn print_stale(evaluated: &[(InstanceRecord, ContractView, Vec<Verdict>)]) {
    if evaluated.is_empty() {
        println!("No EC2 instances matched.");
        return;
    }

    let rows: Vec<StaleRow> = evaluated
        .iter()
        .filter(|(_, _, v)| is_flagged(v))
        .map(|(r, _c, verdicts)| StaleRow::from_parts(r, verdicts))
        .collect();

    if rows.is_empty() {
        println!("No instances with stale verdicts.");
        return;
    }

    let headers: Vec<&'static str> = vec![
        "sev",
        "instance_id",
        "type",
        "cost_usd",
        "created",
        "total_age",
        "last_uptime",
        "last_active",
        "p95_cpu",
        "launched_by",
        "verdicts",
    ];
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (i, col) in row.columns().iter().enumerate() {
            if col.len() > widths[i] {
                widths[i] = col.len();
            }
        }
    }

    print_row(&headers, &widths);
    print_separator(&widths);
    for row in &rows {
        print_row(&row.columns(), &widths);
    }

    let total = evaluated.len();
    let flagged = evaluated.iter().filter(|(_, _, v)| is_flagged(v)).count();
    let worst = evaluated
        .iter()
        .filter_map(|(_, _, v)| worst_severity(v))
        .max();
    let worst_label = worst.map(Severity::as_str).unwrap_or("none");
    println!();
    println!("{flagged} flagged / {total} scanned, worst severity: {worst_label}");
}

struct ListRow {
    instance_id: String,
    state: String,
    instance_type: String,
    estimated_cost_usd: String,
    created: String,
    total_age: String,
    last_uptime: String,
    launched_by: String,
    region: String,
    p95_cpu: String,
    max_cpu: String,
    last_active: String,
}

impl ListRow {
    fn from_record(r: &InstanceRecord) -> Self {
        let (p95_cpu, max_cpu, last_active) = r
            .cpu
            .as_ref()
            .map(|c| {
                (
                    format_pct(c.p95_pct),
                    format_pct(c.max_pct),
                    format_last_active(c),
                )
            })
            .unwrap_or_else(|| ("-".to_string(), "-".to_string(), "-".to_string()));

        ListRow {
            instance_id: r.instance_id.clone(),
            state: r.state.as_str().to_string(),
            instance_type: r.instance_type.clone(),
            estimated_cost_usd: format_usd(r.estimated_cost_usd),
            created: format_date(r.created_at),
            total_age: format_uptime(r.total_age_seconds),
            last_uptime: format_uptime(r.last_uptime_seconds),
            launched_by: r.launched_by.clone().unwrap_or_else(|| unknown_guess(r)),
            region: r.region.clone(),
            p95_cpu,
            max_cpu,
            last_active,
        }
    }

    fn columns(&self) -> Vec<&str> {
        vec![
            self.instance_id.as_str(),
            self.state.as_str(),
            self.instance_type.as_str(),
            self.estimated_cost_usd.as_str(),
            self.created.as_str(),
            self.total_age.as_str(),
            self.last_uptime.as_str(),
            self.launched_by.as_str(),
            self.region.as_str(),
            self.p95_cpu.as_str(),
            self.max_cpu.as_str(),
            self.last_active.as_str(),
        ]
    }
}

struct StaleRow {
    severity: String,
    instance_id: String,
    instance_type: String,
    estimated_cost_usd: String,
    created: String,
    total_age: String,
    last_uptime: String,
    last_active: String,
    p95_cpu: String,
    launched_by: String,
    verdicts: String,
}

impl StaleRow {
    fn from_parts(r: &InstanceRecord, verdicts: &[Verdict]) -> Self {
        let severity = worst_severity(verdicts)
            .map(Severity::as_str)
            .unwrap_or("-")
            .to_string();
        let verdicts_str = if verdicts.is_empty() {
            "—".to_string()
        } else {
            verdicts
                .iter()
                .map(format_verdict)
                .collect::<Vec<_>>()
                .join("; ")
        };
        let (last_active, p95_cpu) = r
            .cpu
            .as_ref()
            .map(|c| (format_last_active(c), format_pct(c.p95_pct)))
            .unwrap_or_else(|| ("-".to_string(), "-".to_string()));
        StaleRow {
            severity,
            instance_id: r.instance_id.clone(),
            instance_type: r.instance_type.clone(),
            estimated_cost_usd: format_usd(r.estimated_cost_usd),
            created: format_date(r.created_at),
            total_age: format_uptime(r.total_age_seconds),
            last_uptime: format_uptime(r.last_uptime_seconds),
            last_active,
            p95_cpu,
            launched_by: r.launched_by.clone().unwrap_or_else(|| unknown_guess(r)),
            verdicts: verdicts_str,
        }
    }

    fn columns(&self) -> Vec<&str> {
        vec![
            self.severity.as_str(),
            self.instance_id.as_str(),
            self.instance_type.as_str(),
            self.estimated_cost_usd.as_str(),
            self.created.as_str(),
            self.total_age.as_str(),
            self.last_uptime.as_str(),
            self.last_active.as_str(),
            self.p95_cpu.as_str(),
            self.launched_by.as_str(),
            self.verdicts.as_str(),
        ]
    }
}

fn format_verdict(v: &Verdict) -> String {
    match v {
        Verdict::Managed { controller } => format!("managed({controller})"),
        Verdict::Expired { overdue_secs, .. } => {
            format!("expired({} ago)", format_duration(*overdue_secs))
        }
        Verdict::ExpiringSoon { within_secs, .. } => {
            format!("expiring_soon(in {})", format_duration(*within_secs))
        }
        Verdict::Inactive {
            idle_for_secs,
            samples,
            ..
        } => {
            let idle_for = match idle_for_secs {
                Some(s) => format!("{} ago", format_duration(*s)),
                None => "no active hour in window".to_string(),
            };
            format!("inactive(last={idle_for}, n={samples})")
        }
        Verdict::LongLived { age_secs } => {
            format!("long_lived(age={})", format_duration(*age_secs))
        }
        Verdict::Underutilized {
            p95_pct, samples, ..
        } => format!("underutilized(p95={p95_pct:.1}%, n={samples})"),
        Verdict::NonCompliant { missing, tampered } => {
            let tail = if *tampered { ", tampered" } else { "" };
            format!("non_compliant(missing={}{})", missing.join(","), tail)
        }
    }
}

fn format_duration(seconds: i64) -> String {
    let abs = seconds.unsigned_abs();
    let d = abs / 86_400;
    let h = (abs % 86_400) / 3_600;
    let m = (abs % 3_600) / 60;
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

fn unknown_guess(r: &InstanceRecord) -> String {
    if let Some(key) = r.key_name.as_deref()
        && !key.trim().is_empty()
    {
        return format!("unknown({key})");
    }
    if let Some(name) = r.tags.get("Name")
        && !name.trim().is_empty()
    {
        return format!("unknown({name})");
    }
    "unknown".to_string()
}

fn format_pct(pct: Option<f64>) -> String {
    match pct {
        Some(v) => format!("{v:.1}%"),
        None => "-".to_string(),
    }
}

fn format_usd(v: Option<f64>) -> String {
    match v {
        Some(cost) if cost.is_finite() => format!("${cost:.2}"),
        _ => "-".to_string(),
    }
}

fn print_row<S: AsRef<str>>(cols: &[S], widths: &[usize]) {
    let mut line = String::new();
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        let s = col.as_ref();
        line.push_str(s);
        if i + 1 < cols.len() {
            for _ in s.len()..widths[i] {
                line.push(' ');
            }
        }
    }
    println!("{line}");
}

fn print_separator(widths: &[usize]) {
    let mut line = String::new();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        for _ in 0..*w {
            line.push('-');
        }
    }
    println!("{line}");
}

pub fn print_explain(
    record: &InstanceRecord,
    contract: &ContractView,
    rule_trace: &[RuleTrace],
) {
    // Header
    println!("Instance {} ({})", record.instance_id, record.region);
    println!("  state        : {}", record.state.as_str());
    println!("  type         : {}", record.instance_type);
    println!("  created      : {}", format_date(record.created_at));
    println!("  total_age    : {}", format_uptime(record.total_age_seconds));
    println!("  last_uptime  : {}", format_uptime(record.last_uptime_seconds));
    let launched_by = record.launched_by.as_deref().unwrap_or("unknown");
    println!(
        "  launched_by  : {} (src: {})",
        launched_by,
        record.launched_by_source.as_str()
    );
    if let Some(az) = &record.az {
        println!("  az           : {az}");
    }
    if let Some(arn) = &record.iam_instance_profile {
        println!("  iam_profile  : {arn}");
    }
    if let Some(key) = &record.key_name {
        println!("  key_name     : {key}");
    }

    // Tags
    println!();
    println!("Tags ({}):", record.tags.len());
    if record.tags.is_empty() {
        println!("  (none)");
    } else {
        let key_w = record
            .tags
            .keys()
            .map(String::len)
            .max()
            .unwrap_or(0)
            .min(40);
        for (k, v) in &record.tags {
            println!("  {k:<key_w$}  {v}");
        }
    }

    // Contract view
    println!();
    println!("Contract view:");
    println!(
        "  managed_by_gasleak : {}",
        if contract.managed_by_gasleak {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "  owner              : {}",
        contract.owner.as_deref().unwrap_or("(unset)")
    );
    println!(
        "  owner_slack        : {}",
        contract.owner_slack.as_deref().unwrap_or("(unset)")
    );
    println!(
        "  expires_at         : {}",
        contract
            .expires_at
            .map(|ts| ts.to_string())
            .unwrap_or_else(|| "(unset)".to_string())
    );

    // CPU
    println!();
    let window_days = record
        .cpu
        .as_ref()
        .map(|cpu| cpu.window_secs / 86_400)
        .unwrap_or(0);
    println!("CPU ({window_days}-day window):");
    match &record.cpu {
        Some(cpu) if cpu.samples > 0 => {
            println!("  avg       : {}", format_pct(cpu.avg_pct));
            println!("  p95       : {}", format_pct(cpu.p95_pct));
            println!("  max       : {}", format_pct(cpu.max_pct));
            println!("  samples   : {}", cpu.samples);
            println!("  last_active : {}", format_last_active(cpu));
        }
        _ => println!("  (no CloudWatch data)"),
    }

    // Storage cost breakdown
    println!();
    print_storage_section(record.cost_breakdown.as_ref());

    // Rule trace
    println!();
    println!("Rule evaluation:");
    let rule_w = rule_trace
        .iter()
        .map(|t| t.rule.len())
        .max()
        .unwrap_or(0);
    for entry in rule_trace {
        match &entry.result {
            Ok(v) => println!(
                "  {rule:<rule_w$}  fired   {desc}",
                rule = entry.rule,
                desc = format_verdict(v),
            ),
            Err(reason) => println!(
                "  {rule:<rule_w$}  skipped {desc}",
                rule = entry.rule,
                desc = reason.as_str(),
            ),
        }
    }

    // Verdict summary
    let verdicts: Vec<Verdict> = rule_trace
        .iter()
        .filter_map(|t| t.result.clone().ok())
        .collect();
    println!();
    let worst = worst_severity(&verdicts)
        .map(Severity::as_str)
        .unwrap_or("none");
    let flagged = is_flagged(&verdicts);
    println!(
        "Summary: {} verdict(s) fired, worst severity: {}, flagged: {}",
        verdicts.len(),
        worst,
        if flagged { "yes" } else { "no" }
    );
}

fn print_storage_section(breakdown: Option<&CostBreakdown>) {
    let Some(bd) = breakdown else {
        println!("Storage: (DescribeVolumes unavailable; cost_usd shows compute only)");
        return;
    };
    if bd.volumes.is_empty() {
        println!(
            "Storage: no attached volumes. Compute cost ${:.2}.",
            bd.compute_usd
        );
        return;
    }

    println!(
        "Storage (${:.2} since volume CreateTime; run rate ${:.2}/mo):",
        bd.storage_usd, bd.storage_run_rate_usd_per_month
    );
    let headers: Vec<&'static str> = vec![
        "volume_id",
        "type",
        "size_gib",
        "iops",
        "mibps",
        "age",
        "capacity_usd",
        "iops_usd",
        "throughput_usd",
        "total_usd",
    ];
    let rows: Vec<VolumeRow> = bd.volumes.iter().map(VolumeRow::from).collect();

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (i, col) in row.columns().iter().enumerate() {
            if col.len() > widths[i] {
                widths[i] = col.len();
            }
        }
    }

    print_indented_row(&headers, &widths);
    print_indented_separator(&widths);
    for row in &rows {
        print_indented_row(&row.columns(), &widths);
        if let Some(reason) = row.excluded_reason {
            println!("    note: {reason}");
        }
    }

    println!(
        "  compute ${:.2} + storage ${:.2} = cost_usd ${:.2}",
        bd.compute_usd,
        bd.storage_usd,
        bd.compute_usd + bd.storage_usd
    );
}

struct VolumeRow {
    volume_id: String,
    volume_type: String,
    size_gib: String,
    iops: String,
    mibps: String,
    age: String,
    capacity_usd: String,
    iops_usd: String,
    throughput_usd: String,
    total_usd: String,
    excluded_reason: Option<&'static str>,
}

impl From<&VolumeCost> for VolumeRow {
    fn from(v: &VolumeCost) -> Self {
        Self {
            volume_id: v.volume_id.clone(),
            volume_type: v.volume_type.clone(),
            size_gib: v.size_gib.to_string(),
            iops: v.iops.map(|i| i.to_string()).unwrap_or_else(|| "-".into()),
            mibps: v
                .throughput_mibps
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".into()),
            age: format_uptime(v.age_secs),
            capacity_usd: format_usd(Some(v.capacity_usd)),
            iops_usd: format_usd(Some(v.iops_usd)),
            throughput_usd: format_usd(Some(v.throughput_usd)),
            total_usd: format_usd(Some(v.total_usd)),
            excluded_reason: v.excluded_reason,
        }
    }
}

impl VolumeRow {
    fn columns(&self) -> Vec<&str> {
        vec![
            &self.volume_id,
            &self.volume_type,
            &self.size_gib,
            &self.iops,
            &self.mibps,
            &self.age,
            &self.capacity_usd,
            &self.iops_usd,
            &self.throughput_usd,
            &self.total_usd,
        ]
    }
}

fn print_indented_row<S: AsRef<str>>(cols: &[S], widths: &[usize]) {
    let mut line = String::from("  ");
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        let s = col.as_ref();
        line.push_str(s);
        if i + 1 < cols.len() {
            for _ in s.len()..widths[i] {
                line.push(' ');
            }
        }
    }
    println!("{line}");
}

fn print_indented_separator(widths: &[usize]) {
    let mut line = String::from("  ");
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        for _ in 0..*w {
            line.push('-');
        }
    }
    println!("{line}");
}
