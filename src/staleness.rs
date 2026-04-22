use jiff::Timestamp;
use serde::Serialize;

use crate::contract::ContractView;
use crate::model::InstanceRecord;

pub const SECS_PER_DAY: i64 = 86_400;
pub const SECS_PER_HOUR: i64 = 3_600;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    Managed {
        controller: &'static str,
    },
    Expired {
        at: Timestamp,
        overdue_secs: i64,
    },
    ExpiringSoon {
        at: Timestamp,
        within_secs: i64,
    },
    Idle {
        p95_pct: f64,
        samples: usize,
        window_secs: i64,
    },
    /// Instance tags don't meet the contract.
    ///
    /// - `tampered = true` means `ManagedBy=gasleak/*` is present but other
    ///   required tags are missing. Someone ran `ec2:DeleteTags` on a
    ///   compliant instance. Always High severity.
    /// - `tampered = false` means no valid `ManagedBy` tag (pre-contract /
    ///   legacy). Low severity until `--migration-deadline` passes.
    NonCompliant {
        missing: Vec<&'static str>,
        tampered: bool,
        past_deadline: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Info | Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Low => "LOW",
            Self::Medium => "MED",
            Self::High => "HIGH",
        }
    }
}

impl Verdict {
    pub fn severity(&self) -> Severity {
        match self {
            Self::Managed { .. } => Severity::Info,
            Self::Expired { .. } => Severity::High,
            Self::ExpiringSoon { .. } => Severity::Medium,
            Self::Idle { .. } => Severity::Low,
            Self::NonCompliant {
                tampered,
                past_deadline,
                ..
            } => {
                if *tampered || *past_deadline {
                    Severity::High
                } else {
                    Severity::Low
                }
            }
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Managed { .. } => "managed",
            Self::Expired { .. } => "expired",
            Self::ExpiringSoon { .. } => "expiring_soon",
            Self::Idle { .. } => "idle",
            Self::NonCompliant { .. } => "non_compliant",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub now: Timestamp,
    pub warn_window_secs: i64,
    pub min_cpu_samples: usize,
    pub p95_idle_threshold: f64,
    pub idle_lookback_secs: i64,
    pub migration_deadline: Option<Timestamp>,
}

impl Config {
    pub fn defaults(now: Timestamp) -> Self {
        Self {
            now,
            warn_window_secs: 72 * SECS_PER_HOUR,
            min_cpu_samples: 168, // 7 days of hourly data
            p95_idle_threshold: 10.0,
            idle_lookback_secs: 14 * SECS_PER_DAY,
            migration_deadline: None,
        }
    }
}

pub fn evaluate(r: &InstanceRecord, c: &ContractView, cfg: &Config) -> Vec<Verdict> {
    if let Some(v) = rules::managed(r) {
        return vec![v];
    }
    [
        rules::expired,
        rules::expiring_soon,
        rules::idle,
        rules::non_compliant,
    ]
    .iter()
    .filter_map(|f| f(r, c, cfg))
    .collect()
}

pub fn worst_severity(verdicts: &[Verdict]) -> Option<Severity> {
    verdicts.iter().map(Verdict::severity).max()
}

/// An instance is "flagged" when it carries at least one verdict that isn't `Managed`.
/// `Managed` means the controller owns lifecycle and is not a user-actionable concern.
pub fn is_flagged(verdicts: &[Verdict]) -> bool {
    verdicts
        .iter()
        .any(|v| !matches!(v, Verdict::Managed { .. }))
}

pub mod rules {
    use super::{Config, Verdict};
    use crate::contract::ContractView;
    use crate::model::InstanceRecord;

    pub fn managed(r: &InstanceRecord) -> Option<Verdict> {
        // EKS managed node groups create an ASG under the hood, so both tags
        // are usually present. Check EKS first to surface the more useful label.
        const EKS_TAG: &str = "eks:cluster-name";
        const ASG_TAG: &str = "aws:autoscaling:groupName";
        const SPOT_TAG: &str = "aws:ec2spot:fleet-request-id";

        if r.tags.contains_key(EKS_TAG) {
            return Some(Verdict::Managed { controller: "eks" });
        }
        if r.tags.contains_key(ASG_TAG) {
            return Some(Verdict::Managed { controller: "asg" });
        }
        if r.tags.contains_key(SPOT_TAG) {
            return Some(Verdict::Managed {
                controller: "spot-fleet",
            });
        }
        None
    }

    pub fn expired(_r: &InstanceRecord, c: &ContractView, cfg: &Config) -> Option<Verdict> {
        let at = c.expires_at?;
        let now_s = cfg.now.as_second();
        let at_s = at.as_second();
        if at_s < now_s {
            Some(Verdict::Expired {
                at,
                overdue_secs: now_s - at_s,
            })
        } else {
            None
        }
    }

    pub fn expiring_soon(
        _r: &InstanceRecord,
        c: &ContractView,
        cfg: &Config,
    ) -> Option<Verdict> {
        let at = c.expires_at?;
        let now_s = cfg.now.as_second();
        let at_s = at.as_second();
        let delta = at_s - now_s;
        if delta > 0 && delta <= cfg.warn_window_secs {
            Some(Verdict::ExpiringSoon {
                at,
                within_secs: delta,
            })
        } else {
            None
        }
    }

    pub fn idle(r: &InstanceRecord, c: &ContractView, cfg: &Config) -> Option<Verdict> {
        // If the owner has declared when this instance should die, trust them.
        // `expired` and `expiring_soon` handle the confirmation nudge. Idle
        // would just nag owners who have already committed to a deadline.
        if let Some(exp) = c.expires_at
            && exp.as_second() > cfg.now.as_second()
        {
            return None;
        }
        let cpu = r.cpu.as_ref()?;
        if cpu.samples < cfg.min_cpu_samples {
            return None;
        }
        let p95 = cpu.p95_pct?;
        if p95 >= cfg.p95_idle_threshold {
            return None;
        }
        Some(Verdict::Idle {
            p95_pct: p95,
            samples: cpu.samples,
            window_secs: cfg.idle_lookback_secs,
        })
    }

    pub fn non_compliant(
        r: &InstanceRecord,
        c: &ContractView,
        cfg: &Config,
    ) -> Option<Verdict> {
        let mut missing: Vec<&'static str> = Vec::new();

        if !c.managed_by_gasleak {
            missing.push(if r.tags.contains_key("ManagedBy") {
                "ManagedBy(wrong-prefix)"
            } else {
                "ManagedBy"
            });
        }
        if c.owner.is_none() {
            missing.push("Owner");
        }
        if c.owner_slack.is_none() {
            missing.push("OwnerSlack");
        }
        if c.expires_at.is_none() {
            missing.push("ExpiresAt");
        }

        if missing.is_empty() {
            return None;
        }

        // "tampered" = ManagedBy is correct but something else is missing.
        // Either someone stripped tags after launch, or gasleak itself has a
        // bug. Either way, High severity.
        let tampered = c.managed_by_gasleak;

        Some(Verdict::NonCompliant {
            missing,
            tampered,
            past_deadline: past_deadline(cfg),
        })
    }

    fn past_deadline(cfg: &Config) -> bool {
        cfg.migration_deadline
            .map(|d| cfg.now.as_second() > d.as_second())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CpuSummary, InstanceRecord, InstanceState, LaunchedBySource};

    fn ts(rfc: &str) -> Timestamp {
        rfc.parse().expect("valid RFC 3339")
    }

    fn record(tags: &[(&str, &str)], age_days: i64) -> InstanceRecord {
        let launch = ts("2024-01-01T00:00:00Z");
        InstanceRecord {
            instance_id: "i-test".into(),
            launched_by: None,
            launched_by_source: LaunchedBySource::Unknown,
            launch_time: launch,
            created_at: launch,
            last_uptime_seconds: age_days * SECS_PER_DAY,
            total_age_seconds: age_days * SECS_PER_DAY,
            instance_type: "t3.micro".into(),
            state: InstanceState::Running,
            region: "us-east-1".into(),
            az: None,
            iam_instance_profile: None,
            key_name: None,
            tags: tags
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            cpu: None,
        }
    }

    fn contract_of(r: &InstanceRecord) -> ContractView {
        ContractView::from_tags(&r.tags)
    }

    fn cfg_at(now: &str) -> Config {
        Config::defaults(ts(now))
    }

    #[test]
    fn managed_preempts_for_eks_nodes() {
        let r = record(&[("eks:cluster-name", "prod-cluster")], 400);
        let c = contract_of(&r);
        let v = evaluate(&r, &c, &cfg_at("2026-04-21T00:00:00Z"));
        assert_eq!(v.len(), 1);
        assert!(matches!(v[0], Verdict::Managed { controller: "eks" }));
    }

    #[test]
    fn managed_preempts_for_asg() {
        let r = record(&[("aws:autoscaling:groupName", "fleet-a")], 100);
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(matches!(v[0], Verdict::Managed { controller: "asg" }));
    }

    #[test]
    fn expired_fires_when_past_expiry() {
        let r = record(
            &[
                ("ManagedBy", "gasleak/0.1.0"),
                ("Owner", "alice"),
                ("OwnerSlack", "@alice"),
                ("ExpiresAt", "2026-04-18T00:00:00Z"),
            ],
            5,
        );
        let now = "2026-04-21T00:00:00Z";
        let v = evaluate(&r, &contract_of(&r), &cfg_at(now));
        assert!(v.iter().any(|v| matches!(v, Verdict::Expired { .. })));
    }

    #[test]
    fn expiring_soon_fires_within_window() {
        let r = record(
            &[
                ("ManagedBy", "gasleak/0.1.0"),
                ("Owner", "alice"),
                ("OwnerSlack", "@alice"),
                ("ExpiresAt", "2026-04-22T00:00:00Z"),
            ],
            2,
        );
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(v.iter().any(|v| matches!(v, Verdict::ExpiringSoon { .. })));
    }

    #[test]
    fn idle_requires_min_samples_and_threshold() {
        // Missing ExpiresAt means the instance is non-compliant or legacy,
        // so idle evaluation is active.
        let mut r = record(&[], 30);

        // Too few samples, no idle verdict.
        r.cpu = Some(CpuSummary {
            avg_pct: Some(1.0),
            p95_pct: Some(1.0),
            max_pct: Some(1.0),
            samples: 5,
            last_active_at: None,
        });
        let v1 = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(!v1.iter().any(|v| matches!(v, Verdict::Idle { .. })));

        // Enough samples, below threshold, fires.
        r.cpu = Some(CpuSummary {
            avg_pct: Some(1.0),
            p95_pct: Some(1.0),
            max_pct: Some(1.0),
            samples: 300,
            last_active_at: None,
        });
        let v2 = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(v2.iter().any(|v| matches!(v, Verdict::Idle { .. })));

        // p95 above threshold, no idle.
        r.cpu = Some(CpuSummary {
            avg_pct: Some(1.0),
            p95_pct: Some(50.0),
            max_pct: Some(99.0),
            samples: 300,
            last_active_at: None,
        });
        let v3 = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(!v3.iter().any(|v| matches!(v, Verdict::Idle { .. })));
    }

    #[test]
    fn idle_is_vetoed_by_future_expires_at() {
        // Instance has contract + a future ExpiresAt. Even with low CPU over
        // many samples, idle must not fire. The owner has committed to a date.
        let mut r = record(
            &[
                ("ManagedBy", "gasleak/0.1.0"),
                ("Owner", "alice"),
                ("OwnerSlack", "@alice"),
                ("ExpiresAt", "2026-05-15T00:00:00Z"), // > now (2026-04-21)
            ],
            30,
        );
        r.cpu = Some(CpuSummary {
            avg_pct: Some(1.0),
            p95_pct: Some(1.0),
            max_pct: Some(1.0),
            samples: 300,
            last_active_at: None,
        });
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(!v.iter().any(|v| matches!(v, Verdict::Idle { .. })));
    }

    #[test]
    fn idle_still_fires_when_expires_at_is_past() {
        // ExpiresAt in the past means the deadline was missed. Idle should
        // still surface as an additional signal alongside `expired`.
        let mut r = record(
            &[
                ("ManagedBy", "gasleak/0.1.0"),
                ("Owner", "alice"),
                ("OwnerSlack", "@alice"),
                ("ExpiresAt", "2026-04-18T00:00:00Z"), // before now
            ],
            30,
        );
        r.cpu = Some(CpuSummary {
            avg_pct: Some(1.0),
            p95_pct: Some(1.0),
            max_pct: Some(1.0),
            samples: 300,
            last_active_at: None,
        });
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(v.iter().any(|v| matches!(v, Verdict::Expired { .. })));
        assert!(v.iter().any(|v| matches!(v, Verdict::Idle { .. })));
    }

    #[test]
    fn non_compliant_fires_when_managed_by_missing() {
        let r = record(&[("Owner", "alice")], 100);
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        let Some(Verdict::NonCompliant { missing, .. }) =
            v.iter().find(|v| matches!(v, Verdict::NonCompliant { .. }))
        else {
            panic!("expected NonCompliant");
        };
        assert!(missing.contains(&"ManagedBy"));
    }

    #[test]
    fn non_compliant_flags_wrong_prefix_managed_by() {
        let r = record(
            &[
                ("ManagedBy", "terraform"),
                ("Owner", "alice"),
                ("OwnerSlack", "@alice"),
                ("ExpiresAt", "2026-05-01T00:00:00Z"),
            ],
            5,
        );
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        let Some(Verdict::NonCompliant { missing, .. }) =
            v.iter().find(|v| matches!(v, Verdict::NonCompliant { .. }))
        else {
            panic!("expected NonCompliant");
        };
        assert!(missing.contains(&"ManagedBy(wrong-prefix)"));
        assert!(!missing.contains(&"Owner"));
    }

    #[test]
    fn managed_prefers_eks_label_when_both_tags_present() {
        let r = record(
            &[
                ("aws:autoscaling:groupName", "fleet-a"),
                ("eks:cluster-name", "prod-cluster"),
            ],
            100,
        );
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(matches!(v[0], Verdict::Managed { controller: "eks" }));
    }

    #[test]
    fn is_flagged_excludes_managed_only_rows() {
        let managed = vec![Verdict::Managed { controller: "eks" }];
        assert!(!is_flagged(&managed));
        let empty: Vec<Verdict> = vec![];
        assert!(!is_flagged(&empty));
        let with_idle = vec![Verdict::Idle {
            p95_pct: 1.0,
            samples: 100,
            window_secs: 14 * SECS_PER_DAY,
        }];
        assert!(is_flagged(&with_idle));
    }

    #[test]
    fn non_compliant_severity_upgrades_past_deadline() {
        let r = record(&[], 5);
        let mut cfg = cfg_at("2026-04-21T00:00:00Z");
        cfg.migration_deadline = Some(ts("2026-04-01T00:00:00Z"));
        let verdicts = evaluate(&r, &contract_of(&r), &cfg);
        let nc = verdicts
            .iter()
            .find(|v| matches!(v, Verdict::NonCompliant { .. }))
            .expect("non_compliant fired");
        assert_eq!(nc.severity(), Severity::High);
    }

    #[test]
    fn non_compliant_tampered_fires_on_missing_owner_tag() {
        let r = record(
            &[
                ("ManagedBy", "gasleak/0.1.0"),
                // Owner missing
                ("OwnerSlack", "@alice"),
                ("ExpiresAt", "2026-05-01T00:00:00Z"),
            ],
            5,
        );
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        let Some(Verdict::NonCompliant {
            missing, tampered, ..
        }) = v.iter().find(|v| matches!(v, Verdict::NonCompliant { .. }))
        else {
            panic!("expected NonCompliant");
        };
        assert!(missing.contains(&"Owner"));
        assert!(*tampered, "ManagedBy is correct → tampered should be true");
    }

    #[test]
    fn non_compliant_tampered_severity_is_high() {
        let r = record(
            &[
                ("ManagedBy", "gasleak/0.1.0"),
                ("Owner", "alice"),
                ("OwnerSlack", "@alice"),
                // ExpiresAt missing
            ],
            5,
        );
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        let nc = v
            .iter()
            .find(|v| matches!(v, Verdict::NonCompliant { .. }))
            .expect("non_compliant fired");
        let Verdict::NonCompliant {
            missing, tampered, ..
        } = nc
        else {
            unreachable!()
        };
        assert!(missing.contains(&"ExpiresAt"));
        assert!(*tampered);
        assert_eq!(nc.severity(), Severity::High);
    }

    #[test]
    fn worst_severity_is_max() {
        let verdicts = vec![
            Verdict::Idle {
                p95_pct: 1.0,
                samples: 100,
                window_secs: 14 * SECS_PER_DAY,
            },
            Verdict::Expired {
                at: ts("2026-04-18T00:00:00Z"),
                overdue_secs: 3 * SECS_PER_DAY,
            },
        ];
        assert_eq!(worst_severity(&verdicts), Some(Severity::High));
    }

    #[test]
    fn severity_exit_codes() {
        assert_eq!(Severity::High.exit_code(), 2);
        assert_eq!(Severity::Medium.exit_code(), 1);
        assert_eq!(Severity::Low.exit_code(), 0);
        assert_eq!(Severity::Info.exit_code(), 0);
    }
}
