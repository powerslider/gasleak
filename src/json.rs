//! JSON output for `list`, `stale`, and `explain`.
//!
//! Each command has its own envelope type optimized for downstream consumers
//! (Slack reporters, cron `jq` pipelines) rather than mirroring the internal
//! Rust types one-for-one. `RuleTrace` is adapted into a tagged `TraceEntry`
//! enum so the `Err(&'static str)` branch has a clean wire shape.
//!
//! The single shared primitive is [`emit`], which writes pretty JSON plus a
//! trailing newline to any [`Write`]. Everything else is just envelope shape.

use serde::Serialize;
use std::io::Write;

use crate::contract::ContractView;
use crate::model::InstanceRecord;
use crate::staleness::{RuleTrace, Severity, SkipReason, Verdict, is_flagged, worst_severity};

/// Write `value` as pretty JSON plus a trailing newline. Sole I/O primitive
/// shared by all JSON emitters.
pub fn emit<W: Write, T: Serialize + ?Sized>(mut writer: W, value: &T) -> anyhow::Result<()> {
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    Ok(())
}

#[derive(Serialize)]
pub struct ListOutput<'a> {
    /// Regions that were scanned. Always a list so consumers can treat
    /// single-region and `--all-regions` runs uniformly.
    pub regions: &'a [&'a str],
    pub rows: &'a [InstanceRecord],
}

impl<'a> ListOutput<'a> {
    pub fn new(regions: &'a [&'a str], rows: &'a [InstanceRecord]) -> Self {
        Self { regions, rows }
    }
}

#[derive(Serialize)]
pub struct StaleOutput<'a> {
    pub regions: &'a [&'a str],
    pub summary: StaleSummary,
    pub rows: Vec<StaleRow<'a>>,
}

#[derive(Serialize)]
pub struct StaleSummary {
    pub scanned: usize,
    pub flagged: usize,
    pub worst_severity: Option<Severity>,
}

#[derive(Serialize)]
pub struct StaleRow<'a> {
    pub instance: &'a InstanceRecord,
    pub contract: &'a ContractView,
    pub verdicts: &'a [Verdict],
}

impl<'a> StaleOutput<'a> {
    /// Build from the same tuple the table renderer consumes. Applies the same
    /// `is_flagged` filter so JSON rows match what `print_stale` would show.
    pub fn from_evaluated(
        regions: &'a [&'a str],
        evaluated: &'a [(InstanceRecord, ContractView, Vec<Verdict>)],
    ) -> Self {
        let scanned = evaluated.len();
        let rows: Vec<StaleRow<'a>> = evaluated
            .iter()
            .filter(|(_, _, v)| is_flagged(v))
            .map(|(instance, contract, verdicts)| StaleRow {
                instance,
                contract,
                verdicts: verdicts.as_slice(),
            })
            .collect();
        let worst = evaluated
            .iter()
            .filter_map(|(_, _, v)| worst_severity(v))
            .max();
        Self {
            regions,
            summary: StaleSummary {
                scanned,
                flagged: rows.len(),
                worst_severity: worst,
            },
            rows,
        }
    }
}

#[derive(Serialize)]
pub struct ExplainOutput<'a> {
    pub instance: &'a InstanceRecord,
    pub contract: &'a ContractView,
    pub trace: Vec<TraceEntry<'a>>,
}

impl<'a> ExplainOutput<'a> {
    pub fn from_parts(
        instance: &'a InstanceRecord,
        contract: &'a ContractView,
        trace: &'a [RuleTrace],
    ) -> Self {
        Self {
            instance,
            contract,
            trace: trace.iter().map(TraceEntry::from_rule_trace).collect(),
        }
    }
}

/// Public-facing shape of one rule evaluation. Adapts `RuleTrace` into a
/// tagged union so the two outcomes (a verdict fired, or a typed skip reason)
/// both land on the wire as `{ "status": "...", ... }`. Downstream code
/// pattern-matches on `status`.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TraceEntry<'a> {
    Fired {
        rule: &'a str,
        verdict: &'a Verdict,
    },
    Skipped {
        rule: &'a str,
        reason: SkipReason,
    },
}

impl<'a> TraceEntry<'a> {
    pub fn from_rule_trace(t: &'a RuleTrace) -> Self {
        match &t.result {
            Ok(v) => Self::Fired {
                rule: t.rule,
                verdict: v,
            },
            Err(reason) => Self::Skipped {
                rule: t.rule,
                reason: *reason,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::staleness::SECS_PER_DAY;
    use jiff::Timestamp;

    #[test]
    fn emit_writes_pretty_json_with_trailing_newline() {
        #[derive(Serialize)]
        struct X {
            a: i32,
        }
        let mut buf = Vec::new();
        emit(&mut buf, &X { a: 1 }).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with("{\n"));
        assert!(out.ends_with("}\n"));
        assert!(out.contains("\"a\": 1"));
    }

    #[test]
    fn trace_entry_tags_fired_and_skipped() {
        let fired = RuleTrace {
            rule: "long_lived",
            result: Ok(Verdict::LongLived {
                age_secs: 100 * SECS_PER_DAY,
            }),
        };
        let skipped = RuleTrace {
            rule: "expired",
            result: Err(SkipReason::ExpiresAtUnset),
        };
        let a = serde_json::to_value(TraceEntry::from_rule_trace(&fired)).unwrap();
        let b = serde_json::to_value(TraceEntry::from_rule_trace(&skipped)).unwrap();
        assert_eq!(a["status"], "fired");
        assert_eq!(a["rule"], "long_lived");
        assert_eq!(a["verdict"]["kind"], "long_lived");
        assert_eq!(b["status"], "skipped");
        assert_eq!(b["rule"], "expired");
        assert_eq!(b["reason"], "expires_at_unset");
    }

    #[test]
    fn stale_summary_captures_worst_severity() {
        fn record() -> InstanceRecord {
            InstanceRecord {
                instance_id: "i-1".into(),
                launched_by: None,
                launched_by_source: crate::model::LaunchedBySource::Unknown,
                launch_time: "2024-01-01T00:00:00Z".parse::<Timestamp>().unwrap(),
                created_at: "2024-01-01T00:00:00Z".parse::<Timestamp>().unwrap(),
                last_uptime_seconds: 0,
                total_age_seconds: 0,
                instance_type: "t3.micro".into(),
                state: crate::model::InstanceState::Running,
                region: "us-east-1".into(),
                az: None,
                iam_instance_profile: None,
                key_name: None,
                tags: Default::default(),
                estimated_cost_usd: None,
                cost_breakdown: None,
                cpu: None,
            }
        }
        let evaluated = vec![
            (
                record(),
                ContractView::from_tags(&Default::default()),
                vec![Verdict::LongLived {
                    age_secs: 100 * SECS_PER_DAY,
                }],
            ),
            (
                record(),
                ContractView::from_tags(&Default::default()),
                vec![Verdict::Managed { controller: "eks" }],
            ),
        ];
        let region_refs: [&str; 1] = ["us-east-1"];
        let out = StaleOutput::from_evaluated(&region_refs, &evaluated);
        assert_eq!(out.summary.scanned, 2);
        assert_eq!(out.summary.flagged, 1);
        assert_eq!(out.summary.worst_severity, Some(Severity::Low));
    }
}
