use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

const LOCATION: &str = "US East (N. Virginia)";
const PRICING_JSON_FILE: &str = "instance_prices.json";
const DEFAULT_PUBLIC_OFFER_URL: &str =
    "https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonEC2/current/index.json";
const UPDATE_HINT: &str =
    "Update with: cargo run -- --regenerate-pricing-table [--pricing-offer-source file:///absolute/path/to/index.json]";

#[derive(Debug, Serialize, Deserialize)]
struct PriceTable {
    #[serde(rename = "_comment")]
    comment: String,
    prices_per_minute: HashMap<String, f64>,
}

pub fn lookup_price_per_minute(instance_type: &str) -> Option<f64> {
    static PRICE_MAP: OnceLock<HashMap<String, f64>> = OnceLock::new();
    PRICE_MAP
        .get_or_init(load_prices_from_json)
        .get(instance_type)
        .copied()
}

pub fn known_instance_type_count() -> usize {
    static PRICE_MAP: OnceLock<HashMap<String, f64>> = OnceLock::new();
    PRICE_MAP.get_or_init(load_prices_from_json).len()
}

pub async fn regenerate_price_table_json(pricing_offer_source: Option<&str>) -> anyhow::Result<usize> {
    let source = pricing_offer_source.unwrap_or(DEFAULT_PUBLIC_OFFER_URL);
    let raw = load_offer_source(source)
        .await
        .with_context(|| format!("failed to load pricing offer source: {source}"))?;
    let out = parse_public_offer_prices(&raw)
        .with_context(|| format!("failed to parse pricing offer source: {source}"))?;

    if out.is_empty() {
        anyhow::bail!(
            "no pricing entries matched filters (location={LOCATION}, os=Linux, preInstalledSw=NA, tenancy=Shared)"
        );
    }

    let count = out.len();
    let table = PriceTable {
        comment: UPDATE_HINT.to_string(),
        prices_per_minute: out,
    };
    let json = serde_json::to_string_pretty(&table)?;
    fs::write(repo_price_table_path(), format!("{json}\n"))
        .context("failed to write instance_prices.json")?;

    Ok(count)
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

fn load_prices_from_json() -> HashMap<String, f64> {
    let path = repo_price_table_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let Ok(table) = serde_json::from_str::<PriceTable>(&raw) else {
        return HashMap::new();
    };
    table.prices_per_minute
}

fn parse_public_offer_prices(raw_json: &str) -> anyhow::Result<HashMap<String, f64>> {
    let value: Value = serde_json::from_str(raw_json)?;

    let mut sku_to_instance_type: HashMap<String, String> = HashMap::new();
    if let Some(products) = value.get("products").and_then(Value::as_object) {
        for (sku, product) in products {
            let attrs = product.get("attributes").and_then(Value::as_object);
            let Some(attrs) = attrs else {
                continue;
            };

            let Some(instance_type) = attrs.get("instanceType").and_then(Value::as_str) else {
                continue;
            };

            let location = attrs
                .get("location")
                .and_then(Value::as_str)
                .unwrap_or_default();
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

            if location == LOCATION
                && os == "Linux"
                && pre == "NA"
                && tenancy == "Shared"
                && (cap.is_empty() || cap == "Used")
            {
                sku_to_instance_type.insert(sku.clone(), instance_type.to_string());
            }
        }
    }

    let mut out: HashMap<String, f64> = HashMap::new();
    let ondemand = value
        .get("terms")
        .and_then(|t| t.get("OnDemand"))
        .and_then(Value::as_object);

    if let Some(ondemand) = ondemand {
        for (sku, sku_terms) in ondemand {
            let Some(instance_type) = sku_to_instance_type.get(sku) else {
                continue;
            };
            let Some(offers) = sku_terms.as_object() else {
                continue;
            };

            let mut best_hourly: Option<f64> = None;
            for offer in offers.values() {
                let Some(price_dims) = offer.get("priceDimensions").and_then(Value::as_object)
                else {
                    continue;
                };

                for dim in price_dims.values() {
                    let unit = dim.get("unit").and_then(Value::as_str).unwrap_or_default();
                    if unit != "Hrs" {
                        continue;
                    }

                    let usd = dim
                        .get("pricePerUnit")
                        .and_then(|ppu| ppu.get("USD"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();

                    let Ok(hourly) = usd.parse::<f64>() else {
                        continue;
                    };
                    if hourly <= 0.0 {
                        continue;
                    }

                    best_hourly = Some(match best_hourly {
                        Some(existing) => existing.min(hourly),
                        None => hourly,
                    });
                }
            }

            if let Some(hourly) = best_hourly {
                let per_minute = hourly / 60.0;
                out.entry(instance_type.clone())
                    .and_modify(|existing| {
                        if per_minute < *existing {
                            *existing = per_minute;
                        }
                    })
                    .or_insert(per_minute);
            }
        }
    }

    Ok(out)
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
                            "attributes": {
                                "instanceType": "t3.micro",
                                "location": "US East (N. Virginia)",
                                "operatingSystem": "Linux",
                                "preInstalledSw": "NA",
                                "tenancy": "Shared",
                                "capacitystatus": "Used"
                            }
                        }
                    },
                    "terms": {
                        "OnDemand": {
                            "SKU1": {
                                "TERM1": {
                                    "priceDimensions": {
                                        "DIM1": {
                                            "unit": "Hrs",
                                            "pricePerUnit": { "USD": "0.0104" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }"#;

                let prices = parse_public_offer_prices(sample).expect("parse should succeed");
                let ppm = prices.get("t3.micro").copied().expect("entry should exist");
                assert!((ppm - (0.0104 / 60.0)).abs() < 1e-12);
        }

        #[test]
        fn parses_file_url_to_path() {
                let source = "file:///tmp/index.json";
                let path = source.strip_prefix("file://").expect("has prefix");
                assert_eq!(path, "/tmp/index.json");
        }
}
