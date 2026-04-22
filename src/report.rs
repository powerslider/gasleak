use jiff::Timestamp;

use crate::contract::ContractView;
use crate::model::{CpuSummary, InstanceRecord, format_uptime};
use crate::staleness::{Severity, Verdict, is_flagged, worst_severity};

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
        None => ">14d ago".to_string(),
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
        "avg_cpu",
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
        "created",
        "total_age",
        "last_uptime",
        "last_active",
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
    avg_cpu: String,
    max_cpu: String,
    last_active: String,
}

impl ListRow {
    fn from_record(r: &InstanceRecord) -> Self {
        let (avg_cpu, max_cpu, last_active) = r
            .cpu
            .as_ref()
            .map(|c| (format_pct(c.avg_pct), format_pct(c.max_pct), format_last_active(c)))
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
            avg_cpu,
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
            self.avg_cpu.as_str(),
            self.max_cpu.as_str(),
            self.last_active.as_str(),
        ]
    }
}

struct StaleRow {
    severity: String,
    instance_id: String,
    instance_type: String,
    created: String,
    total_age: String,
    last_uptime: String,
    last_active: String,
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
        let last_active = r
            .cpu
            .as_ref()
            .map(format_last_active)
            .unwrap_or_else(|| "-".to_string());
        StaleRow {
            severity,
            instance_id: r.instance_id.clone(),
            instance_type: r.instance_type.clone(),
            created: format_date(r.created_at),
            total_age: format_uptime(r.total_age_seconds),
            last_uptime: format_uptime(r.last_uptime_seconds),
            last_active,
            launched_by: r.launched_by.clone().unwrap_or_else(|| unknown_guess(r)),
            verdicts: verdicts_str,
        }
    }

    fn columns(&self) -> Vec<&str> {
        vec![
            self.severity.as_str(),
            self.instance_id.as_str(),
            self.instance_type.as_str(),
            self.created.as_str(),
            self.total_age.as_str(),
            self.last_uptime.as_str(),
            self.last_active.as_str(),
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
        Verdict::Idle {
            p95_pct, samples, ..
        } => format!("idle(p95={p95_pct:.1}%, n={samples})"),
        Verdict::NonCompliant {
            missing,
            tampered,
            past_deadline,
        } => {
            let mut tail = String::new();
            if *tampered {
                tail.push_str(", tampered");
            } else if *past_deadline {
                tail.push_str(", past-deadline");
            }
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
