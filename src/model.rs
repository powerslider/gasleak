//! Core domain types: `InstanceRecord`, `CpuSummary`, `InstanceState`,
//! `LaunchedBySource`, plus a small uptime formatter.
//!
//! Identity resolution heuristics live in [`crate::identity`]. Pricing, rules,
//! and output all consume these types; the types themselves depend only on
//! `jiff` and serde.

use jiff::Timestamp;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct InstanceRecord {
    pub instance_id: String,
    pub launched_by: Option<String>,
    pub launched_by_source: LaunchedBySource,
    /// Time of the most recent start. Resets on stop/start.
    pub launch_time: Timestamp,
    /// Time the instance was originally created (root EBS volume `AttachTime`).
    /// Falls back to `launch_time` for instance-store / missing-data cases.
    pub created_at: Timestamp,
    /// `now - launch_time`. Time since the most recent start.
    pub last_uptime_seconds: i64,
    /// `now - created_at`. Time since original creation, which survives stop/start.
    pub total_age_seconds: i64,
    pub instance_type: String,
    pub state: InstanceState,
    pub region: String,
    pub az: Option<String>,
    pub iam_instance_profile: Option<String>,
    pub key_name: Option<String>,
    pub tags: BTreeMap<String, String>,
    /// Total estimated cost since the most recent start, including attached
    /// EBS volumes when `cost_breakdown` is populated. Stays compute-only when
    /// `DescribeVolumes` failed or no pricing data was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_breakdown: Option<CostBreakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuSummary>,
}

/// Drill-down of `InstanceRecord.estimated_cost_usd` into compute + storage.
/// `Some` when the cost pipeline succeeded; `None` when `DescribeVolumes`
/// failed (in which case `estimated_cost_usd` remains compute-only).
#[derive(Debug, Clone, Serialize)]
pub struct CostBreakdown {
    /// On-demand EC2 compute cost since the most recent start.
    pub compute_usd: f64,
    /// Sum of `volumes[].total_usd`.
    pub storage_usd: f64,
    /// Projected forward: storage cost per month at current provisioning.
    pub storage_run_rate_usd_per_month: f64,
    pub volumes: Vec<VolumeCost>,
}

/// Per-volume cost accounting. `age_secs = now - Volume.CreateTime` — the
/// billing-accurate anchor, which survives attach/detach cycles.
#[derive(Debug, Clone, Serialize)]
pub struct VolumeCost {
    pub volume_id: String,
    pub volume_type: String,
    pub size_gib: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iops: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throughput_mibps: Option<i32>,
    pub age_secs: i64,
    pub capacity_usd: f64,
    pub iops_usd: f64,
    pub throughput_usd: f64,
    pub total_usd: f64,
    /// `Some` when part of this volume's real cost is intentionally not
    /// modeled (e.g. `standard` per-I/O charges, or a volume type absent
    /// from the rate table). `None` means `total_usd` is fully attributed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_reason: Option<&'static str>,
}

/// Domain-level view of an EBS volume used by the cost pipeline. Decoupled
/// from the SDK's `Volume` type so we don't leak AWS SDK concerns.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub volume_id: String,
    /// AWS volume-type wire string: `gp3`, `gp2`, `io1`, `io2`, `st1`, `sc1`,
    /// `standard`. Matches the pricing table keys.
    pub volume_type: String,
    pub size_gib: i32,
    pub iops: Option<i32>,
    pub throughput_mibps: Option<i32>,
    /// EBS bills from `CreateTime` regardless of attachment status. This is
    /// the anchor for age-based cost computation.
    pub create_time: Timestamp,
    /// Instance IDs the volume is currently attached to. Usually one entry;
    /// multi-attach io1/io2 can have more.
    pub attached_instance_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchedBySource {
    Tag,
    IamRole,
    KeyName,
    Unknown,
}

impl LaunchedBySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::IamRole => "iam-role",
            Self::KeyName => "key-name",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InstanceState {
    Pending,
    Running,
    ShuttingDown,
    Terminated,
    Stopping,
    Stopped,
    Other,
}

impl InstanceState {
    pub fn from_sdk(name: Option<&aws_sdk_ec2::types::InstanceStateName>) -> Self {
        use aws_sdk_ec2::types::InstanceStateName;
        match name {
            Some(InstanceStateName::Pending) => Self::Pending,
            Some(InstanceStateName::Running) => Self::Running,
            Some(InstanceStateName::ShuttingDown) => Self::ShuttingDown,
            Some(InstanceStateName::Terminated) => Self::Terminated,
            Some(InstanceStateName::Stopping) => Self::Stopping,
            Some(InstanceStateName::Stopped) => Self::Stopped,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::ShuttingDown => "shutting-down",
            Self::Terminated => "terminated",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CpuSummary {
    pub avg_pct: Option<f64>,
    pub p95_pct: Option<f64>,
    pub max_pct: Option<f64>,
    pub samples: usize,
    /// Most recent hour (within the lookback window) whose *maximum* CPU was
    /// at or above `ACTIVE_THRESHOLD_PCT`. `None` if the instance was never
    /// active in the window, or if CloudWatch returned no data.
    pub last_active_at: Option<Timestamp>,
    /// The lookback window used to compute this summary, in seconds. Lets the
    /// report label "no active hour in window" rows accurately as e.g. ">30d ago".
    pub window_secs: i64,
}

pub fn format_uptime(seconds: i64) -> String {
    if seconds < 0 {
        return "-".to_string();
    }
    let d = seconds / 86_400;
    let h = (seconds % 86_400) / 3_600;
    let m = (seconds % 3_600) / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uptime_variants() {
        assert_eq!(format_uptime(45), "0m");
        assert_eq!(format_uptime(60), "1m");
        assert_eq!(format_uptime(3_600), "1h 0m");
        assert_eq!(format_uptime(86_400 + 3_600 * 2 + 60 * 3), "1d 2h 3m");
        assert_eq!(format_uptime(-1), "-");
    }
}
