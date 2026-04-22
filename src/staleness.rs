//! Rule engine: evaluate an instance against a declarative rule set and
//! produce [`Verdict`]s with a [`Severity`].
//!
//! The public API is three functions plus a config type:
//! - [`evaluate`] — run the rules, pre-empting on `managed`.
//! - [`trace`] — run every rule unconditionally and surface skip reasons,
//!   used by `gasleak explain`.
//! - [`worst_severity`] — fold verdicts into a single exit-code driver.
//! - [`Config`] — thresholds overridable via the TOML config file.
//!
//! Individual rule functions live in the [`rules`] submodule and share a
//! [`RuleResult`](rules::RuleResult) alias: `Ok(Verdict)` when they fire,
//! `Err(SkipReason)` when they deliberately do not.

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
    /// CPU has been quiet for long enough to flag. Severity is a step function
    /// of `idle_for_secs` against `Config::inactive_{low,medium,high}_secs`.
    /// `idle_for_secs = None` means no active hour was seen in the lookback
    /// window, which always maps to High severity.
    Inactive {
        idle_for_secs: Option<i64>,
        samples: usize,
        window_secs: i64,
        severity: Severity,
    },
    /// Warning-only verdict: instance has been around for a long time and is
    /// worth reviewing even if currently active.
    LongLived {
        age_secs: i64,
    },
    /// Warning-only verdict: sustained low load across the CPU window. Fires
    /// when p95 over the lookback is below the configured threshold. Signals
    /// "this box is oversized for what it does", not "this box is dead".
    /// Independent of `last_active_at` and always Low severity.
    Underutilized {
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
    ///   legacy). Always Low severity. Escalation comes from the `inactive`
    ///   rule once the instance goes quiet, not from a calendar deadline.
    NonCompliant {
        missing: Vec<&'static str>,
        tampered: bool,
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
            Self::Inactive { severity, .. } => *severity,
            Self::LongLived { .. } => Severity::Low,
            Self::Underutilized { .. } => Severity::Low,
            Self::NonCompliant { tampered, .. } => {
                if *tampered {
                    Severity::High
                } else {
                    Severity::Low
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub now: Timestamp,
    pub warn_window_secs: i64,
    pub min_cpu_samples: usize,
    /// Seconds since last activity at or above which the `inactive` rule starts
    /// to fire (Low severity).
    pub inactive_low_secs: i64,
    /// Seconds since last activity at or above which `inactive` is Medium.
    pub inactive_medium_secs: i64,
    /// Seconds since last activity at or above which `inactive` is High.
    pub inactive_high_secs: i64,
    /// Total age at or above which `long_lived` fires (Low warning).
    pub long_lived_age_secs: i64,
    /// p95 CPU % below which `underutilized` fires (Low warning).
    pub p95_underutilized_threshold: f64,
}

impl Config {
    pub fn defaults(now: Timestamp) -> Self {
        Self {
            now,
            warn_window_secs: 72 * SECS_PER_HOUR,
            min_cpu_samples: 168,
            inactive_low_secs: 7 * SECS_PER_DAY,
            inactive_medium_secs: 14 * SECS_PER_DAY,
            inactive_high_secs: 30 * SECS_PER_DAY,
            long_lived_age_secs: 90 * SECS_PER_DAY,
            p95_underutilized_threshold: 2.0,
        }
    }

    /// CloudWatch lookback window. Must cover the High threshold so `inactive`
    /// can distinguish "quiet for 20 days" from "quiet for 60 days".
    pub fn cpu_lookback_secs(&self) -> i64 {
        self.inactive_high_secs
    }
}

pub fn evaluate(r: &InstanceRecord, c: &ContractView, cfg: &Config) -> Vec<Verdict> {
    if let Ok(v) = rules::managed(r) {
        return vec![v];
    }
    [
        rules::expired,
        rules::expiring_soon,
        rules::inactive,
        rules::underutilized,
        rules::long_lived,
        rules::non_compliant,
    ]
    .iter()
    .filter_map(|f| f(r, c, cfg).ok())
    .collect()
}

/// Why a rule did not fire. One variant per documented "skipped" case across
/// the rules in [`rules`]. Typed rather than free-form strings so:
///
/// - `gasleak explain --json` emits stable `snake_case` identifiers that
///   downstream consumers (Slack formatters, `jq` pipelines) can match on,
/// - the human renderer produces the exact sentence a reader expects,
/// - `grep SkipReason::FooBar` finds every rule that emits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    NoControllerTags,
    ExpiresAtUnset,
    ExpiresAtInFuture,
    ExpiresAtAlreadyPast,
    ExpiresAtBeyondWindow,
    VetoedByFutureDeadline,
    NoCpuData,
    InsufficientSamples,
    RecentActivity,
    BelowLongLivedThreshold,
    P95NotComputable,
    P95AboveThreshold,
    AllContractTagsPresent,
}

impl SkipReason {
    /// Human-readable sentence used by the table renderer.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoControllerTags => "no controller tags present",
            Self::ExpiresAtUnset => "ExpiresAt tag not set",
            Self::ExpiresAtInFuture => "ExpiresAt is in the future",
            Self::ExpiresAtAlreadyPast => "ExpiresAt is in the past (see expired)",
            Self::ExpiresAtBeyondWindow => "ExpiresAt is beyond the warn window",
            Self::VetoedByFutureDeadline => "vetoed by a valid future ExpiresAt",
            Self::NoCpuData => "no CPU data (fetch failed or skipped)",
            Self::InsufficientSamples => "insufficient CPU samples",
            Self::RecentActivity => "last activity is recent",
            Self::BelowLongLivedThreshold => "total_age is below the long-lived threshold",
            Self::P95NotComputable => "p95 not computable",
            Self::P95AboveThreshold => "p95 CPU at or above underutilized threshold",
            Self::AllContractTagsPresent => "all required contract tags present",
        }
    }
}

/// One evaluation trace entry. Used by `gasleak explain`.
///
/// Every rule is evaluated (no short-circuit on `managed`) so the explain
/// output shows the full decision table rather than whichever rules happened
/// to run before a pre-empt.
#[derive(Debug)]
pub struct RuleTrace {
    pub rule: &'static str,
    pub result: core::result::Result<Verdict, SkipReason>,
}

pub fn trace(r: &InstanceRecord, c: &ContractView, cfg: &Config) -> Vec<RuleTrace> {
    vec![
        RuleTrace {
            rule: "managed",
            result: rules::managed(r),
        },
        RuleTrace {
            rule: "expired",
            result: rules::expired(r, c, cfg),
        },
        RuleTrace {
            rule: "expiring_soon",
            result: rules::expiring_soon(r, c, cfg),
        },
        RuleTrace {
            rule: "inactive",
            result: rules::inactive(r, c, cfg),
        },
        RuleTrace {
            rule: "underutilized",
            result: rules::underutilized(r, c, cfg),
        },
        RuleTrace {
            rule: "long_lived",
            result: rules::long_lived(r, c, cfg),
        },
        RuleTrace {
            rule: "non_compliant",
            result: rules::non_compliant(r, c, cfg),
        },
    ]
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
    use super::{Config, Severity, SkipReason, Verdict};
    use crate::contract::ContractView;
    use crate::model::InstanceRecord;

    /// Rules return `Ok(Verdict)` when they fire, and `Err(SkipReason)` with a
    /// typed reason when they deliberately do not. `evaluate` throws the
    /// reasons away, `trace` surfaces them to the user.
    pub type RuleResult = core::result::Result<Verdict, SkipReason>;

    pub fn managed(r: &InstanceRecord) -> RuleResult {
        // EKS managed node groups create an ASG under the hood, so both tags
        // are usually present. Check EKS first to surface the more useful label.
        const EKS_TAG: &str = "eks:cluster-name";
        const ASG_TAG: &str = "aws:autoscaling:groupName";
        const SPOT_TAG: &str = "aws:ec2spot:fleet-request-id";

        if r.tags.contains_key(EKS_TAG) {
            return Ok(Verdict::Managed { controller: "eks" });
        }
        if r.tags.contains_key(ASG_TAG) {
            return Ok(Verdict::Managed { controller: "asg" });
        }
        if r.tags.contains_key(SPOT_TAG) {
            return Ok(Verdict::Managed {
                controller: "spot-fleet",
            });
        }
        Err(SkipReason::NoControllerTags)
    }

    pub fn expired(_r: &InstanceRecord, c: &ContractView, cfg: &Config) -> RuleResult {
        let Some(at) = c.expires_at else {
            return Err(SkipReason::ExpiresAtUnset);
        };
        let now_s = cfg.now.as_second();
        let at_s = at.as_second();
        if at_s < now_s {
            Ok(Verdict::Expired {
                at,
                overdue_secs: now_s - at_s,
            })
        } else {
            Err(SkipReason::ExpiresAtInFuture)
        }
    }

    pub fn expiring_soon(_r: &InstanceRecord, c: &ContractView, cfg: &Config) -> RuleResult {
        let Some(at) = c.expires_at else {
            return Err(SkipReason::ExpiresAtUnset);
        };
        let now_s = cfg.now.as_second();
        let at_s = at.as_second();
        let delta = at_s - now_s;
        if delta <= 0 {
            Err(SkipReason::ExpiresAtAlreadyPast)
        } else if delta > cfg.warn_window_secs {
            Err(SkipReason::ExpiresAtBeyondWindow)
        } else {
            Ok(Verdict::ExpiringSoon {
                at,
                within_secs: delta,
            })
        }
    }

    pub fn inactive(r: &InstanceRecord, c: &ContractView, cfg: &Config) -> RuleResult {
        // If the owner has declared when this instance should die, trust them.
        // `expired` and `expiring_soon` handle the confirmation nudge. Inactive
        // would just nag owners who have already committed to a deadline.
        if let Some(exp) = c.expires_at
            && exp.as_second() > cfg.now.as_second()
        {
            return Err(SkipReason::VetoedByFutureDeadline);
        }
        let Some(cpu) = r.cpu.as_ref() else {
            return Err(SkipReason::NoCpuData);
        };
        if cpu.samples < cfg.min_cpu_samples {
            return Err(SkipReason::InsufficientSamples);
        }

        let now_s = cfg.now.as_second();
        let idle_for_secs = cpu
            .last_active_at
            .map(|ts| now_s.saturating_sub(ts.as_second()));

        // None (no active hour in lookback) or idle beyond the High threshold -> High.
        // Below the Low threshold -> don't fire.
        // Otherwise step: Low / Medium / High.
        let severity = match idle_for_secs {
            None => Severity::High,
            Some(secs) if secs < cfg.inactive_low_secs => {
                return Err(SkipReason::RecentActivity);
            }
            Some(secs) if secs >= cfg.inactive_high_secs => Severity::High,
            Some(secs) if secs >= cfg.inactive_medium_secs => Severity::Medium,
            Some(_) => Severity::Low,
        };

        Ok(Verdict::Inactive {
            idle_for_secs,
            samples: cpu.samples,
            window_secs: cfg.cpu_lookback_secs(),
            severity,
        })
    }

    pub fn long_lived(r: &InstanceRecord, _c: &ContractView, cfg: &Config) -> RuleResult {
        if r.total_age_seconds < cfg.long_lived_age_secs {
            return Err(SkipReason::BelowLongLivedThreshold);
        }
        Ok(Verdict::LongLived {
            age_secs: r.total_age_seconds,
        })
    }

    pub fn underutilized(r: &InstanceRecord, c: &ContractView, cfg: &Config) -> RuleResult {
        // Same veto as `inactive`: a live future deadline means the owner has
        // committed. Do not surface right-sizing noise on top.
        if let Some(exp) = c.expires_at
            && exp.as_second() > cfg.now.as_second()
        {
            return Err(SkipReason::VetoedByFutureDeadline);
        }
        let Some(cpu) = r.cpu.as_ref() else {
            return Err(SkipReason::NoCpuData);
        };
        if cpu.samples < cfg.min_cpu_samples {
            return Err(SkipReason::InsufficientSamples);
        }
        let Some(p95) = cpu.p95_pct else {
            return Err(SkipReason::P95NotComputable);
        };
        if p95 >= cfg.p95_underutilized_threshold {
            return Err(SkipReason::P95AboveThreshold);
        }
        Ok(Verdict::Underutilized {
            p95_pct: p95,
            samples: cpu.samples,
            window_secs: cfg.cpu_lookback_secs(),
        })
    }

    pub fn non_compliant(r: &InstanceRecord, c: &ContractView, _cfg: &Config) -> RuleResult {
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
            return Err(SkipReason::AllContractTagsPresent);
        }

        // "tampered" = ManagedBy is correct but something else is missing.
        // Either someone stripped tags after launch, or gasleak itself has a
        // bug. Either way, High severity. Legacy untagged boxes stay at Low.
        let tampered = c.managed_by_gasleak;

        Ok(Verdict::NonCompliant { missing, tampered })
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
            estimated_cost_usd: None,
            cost_breakdown: None,
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

    /// Build a `CpuSummary` with a chosen `last_active_at` and enough samples
    /// to pass the data-quality gate. All CPU stat fields are placeholders.
    fn cpu_with_last_active(last_active: Option<Timestamp>, samples: usize) -> CpuSummary {
        CpuSummary {
            avg_pct: Some(1.0),
            p95_pct: Some(1.0),
            max_pct: Some(1.0),
            samples,
            last_active_at: last_active,
            window_secs: 14 * SECS_PER_DAY,
        }
    }

    #[test]
    fn inactive_does_not_fire_when_activity_is_recent() {
        let now = "2026-04-21T00:00:00Z";
        let mut r = record(&[], 30);
        // 1 day ago is well under the 7d Low threshold.
        r.cpu = Some(cpu_with_last_active(
            Some(ts("2026-04-20T00:00:00Z")),
            300,
        ));
        let v = evaluate(&r, &contract_of(&r), &cfg_at(now));
        assert!(!v.iter().any(|v| matches!(v, Verdict::Inactive { .. })));
    }

    #[test]
    fn inactive_fires_low_medium_high_by_idle_days() {
        let now = "2026-04-21T00:00:00Z";
        // Low: 10 days ago (>= 7d low threshold, < 14d medium).
        let mut r = record(&[], 30);
        r.cpu = Some(cpu_with_last_active(Some(ts("2026-04-11T00:00:00Z")), 300));
        let v = evaluate(&r, &contract_of(&r), &cfg_at(now));
        let sev = v
            .iter()
            .find(|v| matches!(v, Verdict::Inactive { .. }))
            .expect("inactive fired")
            .severity();
        assert_eq!(sev, Severity::Low);

        // Medium: 20 days ago (>= 14d medium threshold, < 30d high).
        r.cpu = Some(cpu_with_last_active(Some(ts("2026-04-01T00:00:00Z")), 300));
        let v = evaluate(&r, &contract_of(&r), &cfg_at(now));
        assert_eq!(
            v.iter()
                .find(|v| matches!(v, Verdict::Inactive { .. }))
                .unwrap()
                .severity(),
            Severity::Medium
        );

        // High: 40 days ago (>= 30d high threshold).
        r.cpu = Some(cpu_with_last_active(Some(ts("2026-03-12T00:00:00Z")), 300));
        let v = evaluate(&r, &contract_of(&r), &cfg_at(now));
        assert_eq!(
            v.iter()
                .find(|v| matches!(v, Verdict::Inactive { .. }))
                .unwrap()
                .severity(),
            Severity::High
        );
    }

    #[test]
    fn inactive_without_any_active_hour_is_high() {
        // last_active_at = None means no hour in the lookback window crossed
        // the activity threshold. Treat as at least `inactive_high_secs`.
        let mut r = record(&[], 30);
        r.cpu = Some(cpu_with_last_active(None, 300));
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        let sev = v
            .iter()
            .find(|v| matches!(v, Verdict::Inactive { .. }))
            .expect("inactive fired")
            .severity();
        assert_eq!(sev, Severity::High);
    }

    #[test]
    fn inactive_skipped_when_samples_insufficient() {
        let mut r = record(&[], 30);
        r.cpu = Some(cpu_with_last_active(None, 5));
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(!v.iter().any(|v| matches!(v, Verdict::Inactive { .. })));
    }

    #[test]
    fn inactive_vetoed_by_future_expires_at() {
        let mut r = record(
            &[
                ("ManagedBy", "gasleak/0.1.0"),
                ("Owner", "alice"),
                ("OwnerSlack", "@alice"),
                ("ExpiresAt", "2026-05-15T00:00:00Z"),
            ],
            30,
        );
        r.cpu = Some(cpu_with_last_active(None, 300));
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(!v.iter().any(|v| matches!(v, Verdict::Inactive { .. })));
    }

    #[test]
    fn inactive_fires_when_expires_at_is_past() {
        // Missed deadline still fires `expired`; inactive should add its own
        // signal.
        let mut r = record(
            &[
                ("ManagedBy", "gasleak/0.1.0"),
                ("Owner", "alice"),
                ("OwnerSlack", "@alice"),
                ("ExpiresAt", "2026-04-18T00:00:00Z"),
            ],
            30,
        );
        r.cpu = Some(cpu_with_last_active(None, 300));
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(v.iter().any(|v| matches!(v, Verdict::Expired { .. })));
        assert!(v.iter().any(|v| matches!(v, Verdict::Inactive { .. })));
    }

    #[test]
    fn long_lived_fires_past_default_age() {
        // Default long_lived threshold is 90 days. A 100-day-old box fires it
        // even when currently active (or with no CPU data).
        let r = record(&[], 100);
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        let ll = v.iter().find(|v| matches!(v, Verdict::LongLived { .. }));
        assert!(ll.is_some());
        assert_eq!(ll.unwrap().severity(), Severity::Low);
    }

    #[test]
    fn long_lived_does_not_fire_below_threshold() {
        let r = record(&[], 30);
        let v = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        assert!(!v.iter().any(|v| matches!(v, Verdict::LongLived { .. })));
    }

    /// Build a CpuSummary at a specific p95 with enough samples to pass the gate.
    /// `last_active_at` is set to "now" so `inactive` never fires and the tests
    /// isolate `underutilized` behaviour.
    fn cpu_with_p95(p95: f64, samples: usize, now: &str) -> CpuSummary {
        CpuSummary {
            avg_pct: Some(p95),
            p95_pct: Some(p95),
            max_pct: Some(p95 * 2.0),
            samples,
            last_active_at: Some(ts(now)),
            window_secs: 30 * SECS_PER_DAY,
        }
    }

    #[test]
    fn underutilized_fires_when_p95_below_threshold() {
        let now = "2026-04-21T00:00:00Z";
        let mut r = record(&[], 30);
        r.cpu = Some(cpu_with_p95(0.5, 300, now));
        let v = evaluate(&r, &contract_of(&r), &cfg_at(now));
        let verdict = v
            .iter()
            .find(|v| matches!(v, Verdict::Underutilized { .. }))
            .expect("underutilized fired");
        assert_eq!(verdict.severity(), Severity::Low);
    }

    #[test]
    fn underutilized_skipped_when_p95_at_or_above_threshold() {
        let now = "2026-04-21T00:00:00Z";
        let mut r = record(&[], 30);
        // Default threshold is 2.0%; 2.5% should not fire.
        r.cpu = Some(cpu_with_p95(2.5, 300, now));
        let v = evaluate(&r, &contract_of(&r), &cfg_at(now));
        assert!(!v.iter().any(|v| matches!(v, Verdict::Underutilized { .. })));
    }

    #[test]
    fn underutilized_vetoed_by_future_expires_at() {
        let now = "2026-04-21T00:00:00Z";
        let mut r = record(
            &[
                ("ManagedBy", "gasleak/0.1.0"),
                ("Owner", "alice"),
                ("OwnerSlack", "@alice"),
                ("ExpiresAt", "2026-05-15T00:00:00Z"),
            ],
            30,
        );
        r.cpu = Some(cpu_with_p95(0.5, 300, now));
        let v = evaluate(&r, &contract_of(&r), &cfg_at(now));
        assert!(!v.iter().any(|v| matches!(v, Verdict::Underutilized { .. })));
    }

    #[test]
    fn underutilized_skipped_when_samples_insufficient() {
        let now = "2026-04-21T00:00:00Z";
        let mut r = record(&[], 30);
        r.cpu = Some(cpu_with_p95(0.5, 5, now));
        let v = evaluate(&r, &contract_of(&r), &cfg_at(now));
        assert!(!v.iter().any(|v| matches!(v, Verdict::Underutilized { .. })));
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
        let with_inactive = vec![Verdict::Inactive {
            idle_for_secs: Some(20 * SECS_PER_DAY),
            samples: 300,
            window_secs: 30 * SECS_PER_DAY,
            severity: Severity::High,
        }];
        assert!(is_flagged(&with_inactive));
    }

    #[test]
    fn legacy_non_compliant_stays_at_low() {
        let r = record(&[], 5);
        let verdicts = evaluate(&r, &contract_of(&r), &cfg_at("2026-04-21T00:00:00Z"));
        let nc = verdicts
            .iter()
            .find(|v| matches!(v, Verdict::NonCompliant { .. }))
            .expect("non_compliant fired");
        assert_eq!(nc.severity(), Severity::Low);
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
            Verdict::LongLived {
                age_secs: 100 * SECS_PER_DAY,
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
