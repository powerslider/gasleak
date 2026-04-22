use aws_sdk_cloudwatch::Client as CwClient;
use aws_sdk_cloudwatch::types::{Dimension, Metric, MetricDataQuery, MetricStat};
use aws_smithy_types::DateTime as SdkDateTime;
use futures::stream::{self, StreamExt, TryStreamExt};
use jiff::Timestamp;
use std::collections::HashMap;

use crate::aws::aws_datetime_to_jiff;
use crate::error::{Error, Result};
use crate::model::CpuSummary;

const SECS_PER_DAY: i64 = 86_400;

/// GetMetricData accepts up to 500 MetricDataQuery entries per call.
/// We issue two queries (avg, max) per instance, so pack at most 250 instances per batch.
const INSTANCES_PER_BATCH: usize = 250;
const PERIOD_SECS: i32 = 3_600;
const CONCURRENT_BATCHES: usize = 4;

/// Threshold (hourly Maximum CPU %) above which an hour is considered "active".
/// Tuned for "something real happened" rather than the stricter idle threshold
/// used for the `idle` rule.
const ACTIVE_THRESHOLD_PCT: f64 = 5.0;

pub struct CpuFetcher {
    client: CwClient,
}

impl CpuFetcher {
    pub fn new(client: CwClient) -> Self {
        Self { client }
    }

    pub async fn fetch(
        &self,
        instance_ids: &[String],
        lookback_days: i64,
    ) -> Result<HashMap<String, CpuSummary>> {
        if instance_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let now_secs = Timestamp::now().as_second();
        let lookback_secs = lookback_days.saturating_mul(SECS_PER_DAY);
        let start_secs = now_secs.saturating_sub(lookback_secs);

        let start_sdk = SdkDateTime::from_secs(start_secs);
        let end_sdk = SdkDateTime::from_secs(now_secs);

        #[expect(
            clippy::disallowed_methods,
            reason = "INSTANCES_PER_BATCH is a compile-time non-zero constant"
        )]
        let batches: Vec<Vec<String>> = instance_ids
            .chunks(INSTANCES_PER_BATCH)
            .map(|c| c.to_vec())
            .collect();

        let results = stream::iter(batches)
            .map(|batch| self.fetch_batch(batch, start_sdk, end_sdk))
            .buffer_unordered(CONCURRENT_BATCHES)
            .try_collect::<Vec<HashMap<String, CpuSummary>>>()
            .await?;

        Ok(results.into_iter().flatten().collect())
    }

    async fn fetch_batch(
        &self,
        batch: Vec<String>,
        start: SdkDateTime,
        end: SdkDateTime,
    ) -> Result<HashMap<String, CpuSummary>> {
        let queries = build_queries(&batch);

        let mut avg_values: Vec<Vec<f64>> = vec![Vec::new(); batch.len()];
        let mut max_values: Vec<Vec<f64>> = vec![Vec::new(); batch.len()];
        // Most recent hour (per instance) whose Max CPU hit ACTIVE_THRESHOLD_PCT.
        // GetMetricData returns samples newest-first, so we take the first match
        // we encounter and keep it.
        let mut last_active: Vec<Option<Timestamp>> = vec![None; batch.len()];
        let mut next_token: Option<String> = None;

        loop {
            let resp = self
                .client
                .get_metric_data()
                .set_metric_data_queries(Some(queries.clone()))
                .start_time(start)
                .end_time(end)
                .set_next_token(next_token.clone())
                .send()
                .await
                .map_err(|e| Error::aws("cloudwatch:GetMetricData", e))?;

            for result in resp.metric_data_results() {
                let Some(id) = result.id() else { continue };
                let Some((kind, idx)) = parse_query_id(id) else {
                    continue;
                };
                if idx >= batch.len() {
                    continue;
                }
                match kind {
                    QueryKind::Avg => {
                        avg_values[idx].extend(result.values().iter().copied());
                    }
                    QueryKind::Max => {
                        let values = result.values();
                        let timestamps = result.timestamps();
                        if last_active[idx].is_none() {
                            for (v, t) in values.iter().zip(timestamps.iter()) {
                                if *v >= ACTIVE_THRESHOLD_PCT {
                                    if let Ok(ts) = aws_datetime_to_jiff(t) {
                                        last_active[idx] = Some(ts);
                                    }
                                    break;
                                }
                            }
                        }
                        max_values[idx].extend(values.iter().copied());
                    }
                }
            }

            match resp.next_token().map(str::to_string) {
                Some(t) => next_token = Some(t),
                None => break,
            }
        }

        let mut out = HashMap::with_capacity(batch.len());
        for (idx, id) in batch.into_iter().enumerate() {
            out.insert(
                id,
                summarize(&avg_values[idx], &max_values[idx], last_active[idx]),
            );
        }
        Ok(out)
    }
}

#[derive(Clone, Copy)]
enum QueryKind {
    Avg,
    Max,
}

fn build_queries(batch: &[String]) -> Vec<MetricDataQuery> {
    let mut queries = Vec::with_capacity(batch.len() * 2);
    for (idx, instance_id) in batch.iter().enumerate() {
        queries.push(metric_query(QueryKind::Avg, idx, instance_id));
        queries.push(metric_query(QueryKind::Max, idx, instance_id));
    }
    queries
}

fn metric_query(kind: QueryKind, idx: usize, instance_id: &str) -> MetricDataQuery {
    let stat = match kind {
        QueryKind::Avg => "Average",
        QueryKind::Max => "Maximum",
    };
    let prefix = match kind {
        QueryKind::Avg => "avg",
        QueryKind::Max => "max",
    };

    let metric = Metric::builder()
        .namespace("AWS/EC2")
        .metric_name("CPUUtilization")
        .dimensions(
            Dimension::builder()
                .name("InstanceId")
                .value(instance_id)
                .build(),
        )
        .build();

    let metric_stat = MetricStat::builder()
        .metric(metric)
        .period(PERIOD_SECS)
        .stat(stat)
        .build();

    MetricDataQuery::builder()
        .id(format!("{prefix}_{idx}"))
        .metric_stat(metric_stat)
        .return_data(true)
        .build()
}

fn parse_query_id(id: &str) -> Option<(QueryKind, usize)> {
    let (prefix, rest) = id.split_once('_')?;
    let idx = rest.parse::<usize>().ok()?;
    let kind = match prefix {
        "avg" => QueryKind::Avg,
        "max" => QueryKind::Max,
        _ => return None,
    };
    Some((kind, idx))
}

fn summarize(
    averages: &[f64],
    maximums: &[f64],
    last_active_at: Option<Timestamp>,
) -> CpuSummary {
    if averages.is_empty() {
        return CpuSummary {
            avg_pct: None,
            p95_pct: None,
            max_pct: None,
            samples: 0,
            last_active_at,
        };
    }

    let mean = averages.iter().sum::<f64>() / averages.len() as f64;

    let mut sorted = averages.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "percentile index computation; sorted.len() >= 1 here"
    )]
    let p95_idx = (((sorted.len() as f64) * 0.95) as usize).min(sorted.len() - 1);
    let p95 = sorted[p95_idx];

    let max = maximums
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let max = if max.is_finite() { Some(max) } else { None };

    CpuSummary {
        avg_pct: Some(mean),
        p95_pct: Some(p95),
        max_pct: max,
        samples: averages.len(),
        last_active_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_query_ids() {
        assert!(matches!(parse_query_id("avg_0"), Some((QueryKind::Avg, 0))));
        assert!(matches!(parse_query_id("max_42"), Some((QueryKind::Max, 42))));
        assert!(parse_query_id("bogus_1").is_none());
        assert!(parse_query_id("avg_").is_none());
        assert!(parse_query_id("avg").is_none());
    }

    #[test]
    fn summarize_empty_yields_no_samples() {
        let s = summarize(&[], &[], None);
        assert_eq!(s.samples, 0);
        assert!(s.avg_pct.is_none());
        assert!(s.last_active_at.is_none());
    }

    #[test]
    fn summarize_computes_mean_max_p95() {
        let avgs: Vec<f64> = (1..=100).map(f64::from).collect();
        let maxes: Vec<f64> = vec![10.0, 99.0, 42.0];
        let s = summarize(&avgs, &maxes, None);
        assert_eq!(s.samples, 100);
        assert!((s.avg_pct.unwrap() - 50.5).abs() < 1e-9);
        // 95th percentile of 1..=100 lands at index 95 → value 96.
        assert_eq!(s.p95_pct, Some(96.0));
        assert_eq!(s.max_pct, Some(99.0));
    }

    #[test]
    fn summarize_passes_through_last_active_at() {
        let now: Timestamp = "2026-04-21T00:00:00Z".parse().unwrap();
        let s = summarize(&[1.0, 2.0], &[3.0, 4.0], Some(now));
        assert_eq!(s.last_active_at, Some(now));
    }
}
