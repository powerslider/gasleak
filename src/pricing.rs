//! On-demand pricing lookups for EC2 compute and attached EBS volumes.
//!
//! A price table (`ec2_prices.json` at the repo root) maps:
//! - instance type → per-minute USD compute rate,
//! - EBS volume type → per-GB-month USD capacity rate,
//! - EBS volume type → per-IOPS-month USD rate (`gp3`, `io1`, `io2` base tier),
//! - EBS volume type → per-MiBps-month USD throughput rate (`gp3` only).
//!
//! Loaded once per process via [`OnceLock`]. A `--regenerate-pricing-table`
//! CLI flag rewrites the file from the public AWS `AmazonEC2` pricing offer,
//! which covers compute and EBS in the same JSON bundle.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::{info, warn};

const LOCATION: &str = "US East (N. Virginia)";
const PRICING_JSON_FILE: &str = "ec2_prices.json";
const DEFAULT_PUBLIC_OFFER_URL: &str =
    "https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonEC2/current/index.json";
const UPDATE_HINT: &str =
    "Update with: cargo run -- --regenerate-pricing-table [--pricing-offer-source file:///absolute/path/to/index.json]";

/// MiB per GiB. AWS bills gp3 provisioned throughput in GiBps-Mo at the wire
/// level; we store per-MiBps-Mo to match the EC2 SDK's `Throughput` field.
const MIB_PER_GIB: f64 = 1024.0;

#[derive(Debug, Default, Serialize, Deserialize)]
struct PriceTable {
    #[serde(rename = "_comment")]
    comment: String,
    prices_per_minute: HashMap<String, f64>,
    #[serde(default)]
    ebs_per_gb_month: HashMap<String, f64>,
    #[serde(default)]
    ebs_per_iops_month: HashMap<String, f64>,
    #[serde(default)]
    ebs_per_mibps_month: HashMap<String, f64>,
}

static PRICE_TABLE: OnceLock<PriceTable> = OnceLock::new();

fn price_table() -> &'static PriceTable {
    PRICE_TABLE.get_or_init(load_prices_from_json)
}

pub fn lookup_price_per_minute(instance_type: &str) -> Option<f64> {
    price_table().prices_per_minute.get(instance_type).copied()
}

/// Per-GB-month USD rate for the given volume type (e.g. "gp3"). Keys match
/// `Volume.VolumeType` in the EC2 SDK.
pub fn lookup_ebs_per_gb_month(volume_type: &str) -> Option<f64> {
    price_table().ebs_per_gb_month.get(volume_type).copied()
}

/// Per-IOPS-month USD rate. Populated for `gp3` / `io1` / `io2` base tier.
/// io2 volumes above 32,000 IOPS are actually billed at lower tiered rates;
/// using the base tier slightly overestimates cost for those, which is the
/// honest direction.
pub fn lookup_ebs_per_iops_month(volume_type: &str) -> Option<f64> {
    price_table().ebs_per_iops_month.get(volume_type).copied()
}

/// Per-MiBps-month USD rate for provisioned throughput. Only `gp3` today.
pub fn lookup_ebs_per_mibps_month(volume_type: &str) -> Option<f64> {
    price_table().ebs_per_mibps_month.get(volume_type).copied()
}

/// AWS bills storage in "730-hour months". Convert a wall-clock duration in
/// seconds to that same unit so capacity/IOPS/throughput rates apply cleanly.
pub fn seconds_to_months(secs: i64) -> f64 {
    (secs.max(0) as f64) / (730.0 * 3600.0)
}

/// gp3's first 3000 provisioned IOPS are included in the capacity rate.
/// io1/io2 bill every provisioned IOPS; gp2/st1/sc1/standard have no
/// provisioned-IOPS model.
pub const GP3_FREE_IOPS_BASELINE: i32 = 3000;

/// gp3's first 125 MiB/s of provisioned throughput is included in the
/// capacity rate.
pub const GP3_FREE_THROUGHPUT_MIBPS: i32 = 125;

/// Capacity cost for the given time window. Zero when the volume type is not
/// in the rate table.
pub fn ebs_capacity_cost_usd(volume_type: &str, size_gib: i32, months: f64) -> f64 {
    match lookup_ebs_per_gb_month(volume_type) {
        Some(rate) => (size_gib.max(0) as f64) * rate * months,
        None => 0.0,
    }
}

/// Provisioned-IOPS cost for the given time window. Applies the gp3 free
/// baseline; io1/io2 bill from IOPS 0. Volumes with no provisioned IOPS
/// return 0.0.
pub fn ebs_iops_cost_usd(volume_type: &str, provisioned_iops: i32, months: f64) -> f64 {
    let Some(rate) = lookup_ebs_per_iops_month(volume_type) else {
        return 0.0;
    };
    let billable = match volume_type {
        "gp3" => (provisioned_iops - GP3_FREE_IOPS_BASELINE).max(0),
        _ => provisioned_iops.max(0),
    };
    (billable as f64) * rate * months
}

/// Provisioned-throughput cost for the given time window. Only `gp3` has a
/// provisioned-throughput tier, and its first 125 MiB/s is free.
pub fn ebs_throughput_cost_usd(volume_type: &str, provisioned_mibps: i32, months: f64) -> f64 {
    if volume_type != "gp3" {
        return 0.0;
    }
    let Some(rate) = lookup_ebs_per_mibps_month(volume_type) else {
        return 0.0;
    };
    let billable = (provisioned_mibps - GP3_FREE_THROUGHPUT_MIBPS).max(0);
    (billable as f64) * rate * months
}

pub async fn regenerate_price_table_json(
    pricing_offer_source: Option<&str>,
) -> anyhow::Result<usize> {
    let source = pricing_offer_source.unwrap_or(DEFAULT_PUBLIC_OFFER_URL);
    let raw = load_offer_source(source)
        .await
        .with_context(|| format!("failed to load pricing offer source: {source}"))?;
    let parsed = parse_public_offer_prices(&raw)
        .with_context(|| format!("failed to parse pricing offer source: {source}"))?;

    if parsed.compute_per_minute.is_empty() {
        anyhow::bail!(
            "no compute pricing entries matched filters (location={LOCATION}, os=Linux, preInstalledSw=NA, tenancy=Shared)"
        );
    }
    if parsed.ebs_per_gb_month.is_empty() {
        warn!(
            location = LOCATION,
            "no EBS capacity rates matched; cost_breakdown will fall back to compute-only figures"
        );
    }

    let compute_count = parsed.compute_per_minute.len();
    info!(
        compute = compute_count,
        ebs_gb = parsed.ebs_per_gb_month.len(),
        ebs_iops = parsed.ebs_per_iops_month.len(),
        ebs_mibps = parsed.ebs_per_mibps_month.len(),
        "regenerated pricing table"
    );

    let table = PriceTable {
        comment: UPDATE_HINT.to_string(),
        prices_per_minute: parsed.compute_per_minute,
        ebs_per_gb_month: parsed.ebs_per_gb_month,
        ebs_per_iops_month: parsed.ebs_per_iops_month,
        ebs_per_mibps_month: parsed.ebs_per_mibps_month,
    };
    let json = serde_json::to_string_pretty(&table)?;
    fs::write(repo_price_table_path(), format!("{json}\n"))
        .context("failed to write ec2_prices.json")?;

    Ok(compute_count)
}

async fn load_offer_source(source: &str) -> anyhow::Result<String> {
    if let Some(path) = source.strip_prefix("file://") {
        return fs::read_to_string(path)
            .with_context(|| format!("failed reading file URL path: {path}"));
    }

    if source.starts_with("http://") || source.starts_with("https://") {
        return reqwest::get(source)
            .await
            .with_context(|| format!("request failed for URL: {source}"))?
            .error_for_status()
            .with_context(|| format!("non-success response for URL: {source}"))?
            .text()
            .await
            .with_context(|| format!("failed reading response body from URL: {source}"));
    }

    fs::read_to_string(source).with_context(|| format!("failed reading local path: {source}"))
}

fn repo_price_table_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(PRICING_JSON_FILE)
}

fn load_prices_from_json() -> PriceTable {
    let path = repo_price_table_path();
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to read pricing table; cost columns will show `-`. {UPDATE_HINT}"
            );
            return PriceTable::default();
        }
    };
    match serde_json::from_str::<PriceTable>(&raw) {
        Ok(table) => table,
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to parse pricing table; cost columns will show `-`. {UPDATE_HINT}"
            );
            PriceTable::default()
        }
    }
}

/// Output of parsing the AWS public pricing offer, keyed by product kind.
struct ParsedOfferPrices {
    compute_per_minute: HashMap<String, f64>,
    ebs_per_gb_month: HashMap<String, f64>,
    ebs_per_iops_month: HashMap<String, f64>,
    ebs_per_mibps_month: HashMap<String, f64>,
}

/// Which output bucket a SKU feeds into, plus the key used for that bucket.
enum SkuKind {
    /// Hourly compute rate; key is the EC2 instance type (e.g. `t3.micro`).
    Compute(String),
    /// Per-GB-month storage rate; key is `volumeApiName` (e.g. `gp3`).
    EbsCapacity(String),
    /// Per-IOPS-month provisioned-IOPS rate; key is `volumeApiName`.
    EbsIops(String),
    /// Per-GiBps-month provisioned-throughput rate; key is `volumeApiName`.
    EbsThroughput(String),
}

fn parse_public_offer_prices(raw_json: &str) -> anyhow::Result<ParsedOfferPrices> {
    let value: Value = serde_json::from_str(raw_json)?;

    let sku_kinds = classify_skus(&value);

    let mut out = ParsedOfferPrices {
        compute_per_minute: HashMap::new(),
        ebs_per_gb_month: HashMap::new(),
        ebs_per_iops_month: HashMap::new(),
        ebs_per_mibps_month: HashMap::new(),
    };

    let ondemand = value
        .get("terms")
        .and_then(|t| t.get("OnDemand"))
        .and_then(Value::as_object);

    let Some(ondemand) = ondemand else {
        return Ok(out);
    };

    for (sku, sku_terms) in ondemand {
        let Some(kind) = sku_kinds.get(sku) else {
            continue;
        };
        let Some(offers) = sku_terms.as_object() else {
            continue;
        };

        match kind {
            SkuKind::Compute(instance_type) => {
                // AWS lists Hrs; we store per-minute.
                if let Some(hourly) = best_rate(offers, &["Hrs"]) {
                    upsert_min(&mut out.compute_per_minute, instance_type, hourly / 60.0);
                }
            }
            SkuKind::EbsCapacity(volume_type) => {
                // io2 uses "GB-month"; others use "GB-Mo". Accept both.
                if let Some(rate) = best_rate(offers, &["GB-Mo", "GB-month"]) {
                    upsert_min(&mut out.ebs_per_gb_month, volume_type, rate);
                }
            }
            SkuKind::EbsIops(volume_type) => {
                if let Some(rate) = best_rate(offers, &["IOPS-Mo"]) {
                    upsert_min(&mut out.ebs_per_iops_month, volume_type, rate);
                }
            }
            SkuKind::EbsThroughput(volume_type) => {
                // Offer rate is per-GiBps-Mo; store per-MiBps-Mo so downstream
                // code can multiply by Volume.Throughput (MiBps from the SDK).
                if let Some(rate) = best_rate(offers, &["GiBps-mo"]) {
                    upsert_min(
                        &mut out.ebs_per_mibps_month,
                        volume_type,
                        rate / MIB_PER_GIB,
                    );
                }
            }
        }
    }

    Ok(out)
}

/// First pass: walk `products` and classify each SKU by `productFamily`.
/// Unknown / filtered-out SKUs don't end up in the map.
fn classify_skus(offer: &Value) -> HashMap<String, SkuKind> {
    let mut kinds: HashMap<String, SkuKind> = HashMap::new();

    let Some(products) = offer.get("products").and_then(Value::as_object) else {
        return kinds;
    };

    for (sku, product) in products {
        let Some(family) = product.get("productFamily").and_then(Value::as_str) else {
            continue;
        };
        let Some(attrs) = product.get("attributes").and_then(Value::as_object) else {
            continue;
        };

        let location = attrs
            .get("location")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if location != LOCATION {
            continue;
        }

        match family {
            // "Compute Instance (bare metal)" covers *.metal-* SKUs and ships
            // as a distinct product family but with the same attribute shape.
            "Compute Instance" | "Compute Instance (bare metal)" => {
                let Some(instance_type) = attrs.get("instanceType").and_then(Value::as_str)
                else {
                    continue;
                };
                let os = attrs
                    .get("operatingSystem")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let pre = attrs
                    .get("preInstalledSw")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tenancy = attrs
                    .get("tenancy")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let cap = attrs
                    .get("capacitystatus")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if os == "Linux"
                    && pre == "NA"
                    && tenancy == "Shared"
                    && (cap.is_empty() || cap == "Used")
                {
                    kinds.insert(sku.clone(), SkuKind::Compute(instance_type.to_string()));
                }
            }
            "Storage" => {
                if let Some(vt) = attrs.get("volumeApiName").and_then(Value::as_str) {
                    kinds.insert(sku.clone(), SkuKind::EbsCapacity(vt.to_string()));
                }
            }
            "System Operation" => {
                // Base tier only. Higher io2 tiers (`EBS IOPS Tier 2`, `Tier 3`)
                // and snapshot copy tiers (`EBS Time Based Copy Tier N`) are
                // ignored — using the base rate slightly overestimates io2 cost
                // at very high provisioned IOPS, which is fine for an upper
                // bound.
                let group = attrs
                    .get("group")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if group == "EBS IOPS"
                    && let Some(vt) = attrs.get("volumeApiName").and_then(Value::as_str)
                {
                    kinds.insert(sku.clone(), SkuKind::EbsIops(vt.to_string()));
                }
            }
            "Provisioned Throughput" => {
                if let Some(vt) = attrs.get("volumeApiName").and_then(Value::as_str) {
                    kinds.insert(sku.clone(), SkuKind::EbsThroughput(vt.to_string()));
                }
            }
            _ => {}
        }
    }

    kinds
}

/// Walk all offers / price dimensions under an OnDemand SKU entry and return
/// the smallest USD rate whose unit matches any of `accepted_units`.
fn best_rate(offers: &serde_json::Map<String, Value>, accepted_units: &[&str]) -> Option<f64> {
    let mut best: Option<f64> = None;
    for offer in offers.values() {
        let Some(price_dims) = offer.get("priceDimensions").and_then(Value::as_object) else {
            continue;
        };
        for dim in price_dims.values() {
            let unit = dim.get("unit").and_then(Value::as_str).unwrap_or_default();
            if !accepted_units.contains(&unit) {
                continue;
            }
            let usd = dim
                .get("pricePerUnit")
                .and_then(|ppu| ppu.get("USD"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let Ok(raw) = usd.parse::<f64>() else {
                continue;
            };
            if raw <= 0.0 {
                continue;
            }
            best = Some(match best {
                Some(cur) => cur.min(raw),
                None => raw,
            });
        }
    }
    best
}

/// Insert `value` at `key` if absent, or lower the existing entry if `value`
/// is smaller. Used so multiple product SKUs for the same volume type collapse
/// to the cheapest rate.
fn upsert_min(map: &mut HashMap<String, f64>, key: &str, value: f64) {
    map.entry(key.to_string())
        .and_modify(|existing| {
            if value < *existing {
                *existing = value;
            }
        })
        .or_insert(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_instance_type_returns_none() {
        assert!(lookup_price_per_minute("definitely-not-a-real-instance-type").is_none());
    }

    #[test]
    fn parses_public_offer_file_shape() {
        let sample = r#"{
            "products": {
                "SKU1": {
                    "productFamily": "Compute Instance",
                    "attributes": {
                        "instanceType": "t3.micro",
                        "location": "US East (N. Virginia)",
                        "operatingSystem": "Linux",
                        "preInstalledSw": "NA",
                        "tenancy": "Shared",
                        "capacitystatus": "Used"
                    }
                },
                "SKU_GP3_GB": {
                    "productFamily": "Storage",
                    "attributes": {
                        "location": "US East (N. Virginia)",
                        "volumeApiName": "gp3"
                    }
                },
                "SKU_IO2_GB": {
                    "productFamily": "Storage",
                    "attributes": {
                        "location": "US East (N. Virginia)",
                        "volumeApiName": "io2"
                    }
                },
                "SKU_GP3_IOPS": {
                    "productFamily": "System Operation",
                    "attributes": {
                        "location": "US East (N. Virginia)",
                        "group": "EBS IOPS",
                        "volumeApiName": "gp3"
                    }
                },
                "SKU_IO2_TIER2": {
                    "productFamily": "System Operation",
                    "attributes": {
                        "location": "US East (N. Virginia)",
                        "group": "EBS IOPS Tier 2",
                        "volumeApiName": "io2"
                    }
                },
                "SKU_GP3_MBPS": {
                    "productFamily": "Provisioned Throughput",
                    "attributes": {
                        "location": "US East (N. Virginia)",
                        "volumeApiName": "gp3"
                    }
                },
                "SKU_SNAPSHOT": {
                    "productFamily": "Storage Snapshot",
                    "attributes": {
                        "location": "US East (N. Virginia)",
                        "volumeApiName": "gp3"
                    }
                }
            },
            "terms": {
                "OnDemand": {
                    "SKU1": {
                        "TERM1": {
                            "priceDimensions": {
                                "DIM1": { "unit": "Hrs", "pricePerUnit": { "USD": "0.0104" } }
                            }
                        }
                    },
                    "SKU_GP3_GB": {
                        "T": {
                            "priceDimensions": {
                                "D": { "unit": "GB-Mo", "pricePerUnit": { "USD": "0.08" } }
                            }
                        }
                    },
                    "SKU_IO2_GB": {
                        "T": {
                            "priceDimensions": {
                                "D": { "unit": "GB-month", "pricePerUnit": { "USD": "0.125" } }
                            }
                        }
                    },
                    "SKU_GP3_IOPS": {
                        "T": {
                            "priceDimensions": {
                                "D": { "unit": "IOPS-Mo", "pricePerUnit": { "USD": "0.005" } }
                            }
                        }
                    },
                    "SKU_IO2_TIER2": {
                        "T": {
                            "priceDimensions": {
                                "D": { "unit": "IOPS-Mo", "pricePerUnit": { "USD": "0.0455" } }
                            }
                        }
                    },
                    "SKU_GP3_MBPS": {
                        "T": {
                            "priceDimensions": {
                                "D": { "unit": "GiBps-mo", "pricePerUnit": { "USD": "40.96" } }
                            }
                        }
                    },
                    "SKU_SNAPSHOT": {
                        "T": {
                            "priceDimensions": {
                                "D": { "unit": "GB-Mo", "pricePerUnit": { "USD": "0.05" } }
                            }
                        }
                    }
                }
            }
        }"#;

        let parsed = parse_public_offer_prices(sample).expect("parse should succeed");

        // Compute rate: $0.0104/hr → per-minute.
        let compute = parsed
            .compute_per_minute
            .get("t3.micro")
            .copied()
            .expect("compute rate present");
        assert!((compute - (0.0104 / 60.0)).abs() < 1e-12);

        // Capacity rates: both "GB-Mo" and "GB-month" unit strings accepted.
        assert!((parsed.ebs_per_gb_month["gp3"] - 0.08).abs() < 1e-12);
        assert!((parsed.ebs_per_gb_month["io2"] - 0.125).abs() < 1e-12);

        // Base-tier IOPS only; tiered SKUs with group != "EBS IOPS" are ignored.
        assert!((parsed.ebs_per_iops_month["gp3"] - 0.005).abs() < 1e-12);
        assert!(!parsed.ebs_per_iops_month.contains_key("io2"));

        // Throughput stored per-MiBps: $40.96/GiBps-Mo ÷ 1024 = $0.04/MiBps-Mo.
        assert!((parsed.ebs_per_mibps_month["gp3"] - 0.04).abs() < 1e-12);

        // Snapshots must be filtered out.
        assert!(!parsed.ebs_per_gb_month.contains_key("snapshot"));
    }

    #[test]
    fn parses_file_url_to_path() {
        let source = "file:///tmp/index.json";
        let path = source.strip_prefix("file://").expect("has prefix");
        assert_eq!(path, "/tmp/index.json");
    }

    // The following tests exercise the formulas against the real shipped rate
    // table. They intentionally re-read the shipped rates so that if AWS
    // pricing updates flow in via `--regenerate-pricing-table`, the tests
    // keep verifying the formulas rather than hard-coded dollar amounts.

    #[test]
    fn capacity_cost_applies_rate_times_size_times_months() {
        let rate = lookup_ebs_per_gb_month("gp3").expect("gp3 rate present in shipped table");
        let cost = ebs_capacity_cost_usd("gp3", 100, 1.0);
        assert!((cost - 100.0 * rate).abs() < 1e-9);
    }

    #[test]
    fn capacity_cost_zero_for_unknown_volume_type() {
        assert_eq!(ebs_capacity_cost_usd("future_ebs_type", 1000, 1.0), 0.0);
    }

    #[test]
    fn iops_cost_gp3_respects_3000_baseline() {
        let rate = lookup_ebs_per_iops_month("gp3").expect("gp3 iops rate present");
        // Under baseline → no billable IOPS.
        assert_eq!(ebs_iops_cost_usd("gp3", 2500, 1.0), 0.0);
        // 5000 provisioned → 2000 billable.
        let cost = ebs_iops_cost_usd("gp3", 5000, 1.0);
        assert!((cost - 2000.0 * rate).abs() < 1e-9);
    }

    #[test]
    fn iops_cost_io2_bills_all_provisioned_iops() {
        let rate = lookup_ebs_per_iops_month("io2").expect("io2 iops rate present");
        let cost = ebs_iops_cost_usd("io2", 5000, 1.0);
        assert!((cost - 5000.0 * rate).abs() < 1e-9);
    }

    #[test]
    fn iops_cost_zero_for_types_without_iops_rate() {
        assert_eq!(ebs_iops_cost_usd("gp2", 1000, 1.0), 0.0);
        assert_eq!(ebs_iops_cost_usd("st1", 500, 1.0), 0.0);
        assert_eq!(ebs_iops_cost_usd("standard", 100, 1.0), 0.0);
    }

    #[test]
    fn throughput_cost_gp3_respects_125_baseline() {
        let rate = lookup_ebs_per_mibps_month("gp3").expect("gp3 throughput rate present");
        assert_eq!(ebs_throughput_cost_usd("gp3", 125, 1.0), 0.0);
        let cost = ebs_throughput_cost_usd("gp3", 200, 1.0);
        assert!((cost - 75.0 * rate).abs() < 1e-9);
    }

    #[test]
    fn throughput_cost_zero_for_non_gp3() {
        assert_eq!(ebs_throughput_cost_usd("io2", 1000, 1.0), 0.0);
        assert_eq!(ebs_throughput_cost_usd("gp2", 500, 1.0), 0.0);
    }

    #[test]
    fn seconds_to_months_uses_730_hour_denominator() {
        // AWS convention: 730 hours = 1 "month".
        let one_month_secs = 730 * 3600;
        assert!((seconds_to_months(one_month_secs) - 1.0).abs() < 1e-12);
        assert_eq!(seconds_to_months(0), 0.0);
        // Negative input is clamped to 0 (defensive against clock skew).
        assert_eq!(seconds_to_months(-100), 0.0);
    }
}
