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
    /// `now - created_at`. Time since original creation; survives stop/start.
    pub total_age_seconds: i64,
    pub instance_type: String,
    pub state: InstanceState,
    pub region: String,
    pub az: Option<String>,
    pub iam_instance_profile: Option<String>,
    pub key_name: Option<String>,
    pub tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuSummary>,
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

    pub fn is_billable(self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Stopping)
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

const PREFERRED_TAG_KEYS: &[&str] = &[
    "launched_by",
    "LaunchedBy",
    "launched-by",
    "CreatedBy",
    "created_by",
    "created-by",
    "Owner",
    "owner",
];

pub fn resolve_launched_by(
    tags: &BTreeMap<String, String>,
    iam_profile_arn: Option<&str>,
    key_name: Option<&str>,
) -> (Option<String>, LaunchedBySource) {
    for key in PREFERRED_TAG_KEYS {
        if let Some(value) = tags.get(*key) {
            return (Some(value.clone()), LaunchedBySource::Tag);
        }
    }
    if let Some(arn) = iam_profile_arn
        && let Some((_, role)) = arn.rsplit_once('/')
        && !role.is_empty()
    {
        return (Some(role.to_string()), LaunchedBySource::IamRole);
    }
    if let Some(key) = key_name
        && !key.is_empty()
    {
        return (Some(key.to_string()), LaunchedBySource::KeyName);
    }
    (None, LaunchedBySource::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn format_uptime_variants() {
        assert_eq!(format_uptime(45), "0m");
        assert_eq!(format_uptime(60), "1m");
        assert_eq!(format_uptime(3_600), "1h 0m");
        assert_eq!(format_uptime(86_400 + 3_600 * 2 + 60 * 3), "1d 2h 3m");
        assert_eq!(format_uptime(-1), "-");
    }

    #[test]
    fn resolve_prefers_tag_over_iam() {
        let (v, s) = resolve_launched_by(
            &tags(&[("Owner", "alice")]),
            Some("arn:aws:iam::1:instance-profile/my-role"),
            Some("mykey"),
        );
        assert_eq!(v.as_deref(), Some("alice"));
        assert_eq!(s, LaunchedBySource::Tag);
    }

    #[test]
    fn resolve_falls_back_to_iam_role_name() {
        let (v, s) = resolve_launched_by(
            &tags(&[]),
            Some("arn:aws:iam::1:instance-profile/ci-runner"),
            None,
        );
        assert_eq!(v.as_deref(), Some("ci-runner"));
        assert_eq!(s, LaunchedBySource::IamRole);
    }

    #[test]
    fn resolve_falls_back_to_key_name() {
        let (v, s) = resolve_launched_by(&tags(&[]), None, Some("tsvetan-laptop"));
        assert_eq!(v.as_deref(), Some("tsvetan-laptop"));
        assert_eq!(s, LaunchedBySource::KeyName);
    }

    #[test]
    fn resolve_unknown_when_nothing_matches() {
        let (v, s) = resolve_launched_by(&tags(&[]), None, None);
        assert!(v.is_none());
        assert_eq!(s, LaunchedBySource::Unknown);
    }
}
