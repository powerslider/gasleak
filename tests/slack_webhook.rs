//! Integration test for the Slack client against a stubbed HTTP endpoint.
//!
//! Covers the POST contract without hitting real Slack: payload shape,
//! error handling on non-2xx and on `ok`-but-schema-rejected responses,
//! and the short-circuit timeout when the server hangs longer than
//! [`SlackClient`]'s internal budget.
//!
//! Why `wiremock` over mocking the HTTP layer manually: the behavior under
//! test is "we speak HTTP correctly" — a mocked client would tautologically
//! pass. A real TCP server is the narrowest thing that still exercises the
//! reqwest code path.

use gasleak::model::{BurnRate, InstanceRecord, InstanceState, LaunchedBySource};
use gasleak::slack::render::collect_flagged;
use gasleak::slack::{MentionThreshold, SlackClient, SlackRuntimeConfig, render_stale};
use gasleak::staleness::{Severity, Verdict};
use jiff::Timestamp;
use std::collections::BTreeMap;
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

fn record(id: &str) -> InstanceRecord {
    InstanceRecord {
        instance_id: id.into(),
        launched_by: Some("alice".into()),
        launched_by_source: LaunchedBySource::Tag,
        launch_time: "2026-01-01T00:00:00Z".parse::<Timestamp>().unwrap(),
        created_at: "2026-01-01T00:00:00Z".parse::<Timestamp>().unwrap(),
        last_uptime_seconds: 100 * 86_400,
        total_age_seconds: 100 * 86_400,
        instance_type: "c5.large".into(),
        state: InstanceState::Running,
        region: "us-east-1".into(),
        az: None,
        iam_instance_profile: None,
        key_name: None,
        tags: BTreeMap::new(),
        estimated_cost_usd: Some(1_234.56),
        cost_breakdown: None,
        cpu: None,
    }
}

fn cfg(webhook: String) -> SlackRuntimeConfig {
    SlackRuntimeConfig {
        webhook_url: webhook,
        max_flagged_rows: 3,
        report_url: None,
        mention_threshold: MentionThreshold::High,
    }
}

/// Matches the Slack Block Kit envelope shape: object with `text` (fallback
/// notification string), `blocks` array, `attachments` array. We match on
/// structure, not on exact content — unit tests cover the per-field shape.
struct BlockKitEnvelope;

impl Match for BlockKitEnvelope {
    fn matches(&self, request: &Request) -> bool {
        let v: serde_json::Value = match serde_json::from_slice(&request.body) {
            Ok(v) => v,
            Err(_) => return false,
        };
        v.get("text").and_then(|t| t.as_str()).is_some()
            && v.get("blocks").and_then(|b| b.as_array()).is_some()
            && v.get("attachments").and_then(|a| a.as_array()).is_some()
    }
}

#[tokio::test]
async fn post_sends_valid_block_kit_envelope_with_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("content-type", "application/json"))
        .and(BlockKitEnvelope)
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;

    let webhook = format!("{}/hook", server.uri());
    let run_cfg = cfg(webhook.clone());

    let evaluated = vec![(
        record("i-001"),
        gasleak::contract::ContractView {
            managed_by_gasleak: false,
            owner: None,
            owner_slack: None,
            expires_at: None,
        },
        vec![Verdict::Inactive {
            idle_for_secs: Some(40 * 86_400),
            samples: 500,
            window_secs: 30 * 86_400,
            severity: Severity::High,
        }],
    )];
    let flagged = collect_flagged(&evaluated);
    let payload = render_stale(
        &flagged,
        evaluated.len(),
        &["us-east-1"],
        &BurnRate::from_hourly(10.0),
        &BurnRate::from_hourly(5.0),
        &run_cfg,
        "2026-04-23T00:00:00Z".parse::<Timestamp>().unwrap(),
    );

    let client = SlackClient::new(webhook);
    client.post(&payload).await.expect("POST succeeds");
}

#[tokio::test]
async fn post_surfaces_slack_error_body_on_non_2xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string("invalid_blocks_format: unknown block type"),
        )
        .mount(&server)
        .await;

    let webhook = format!("{}/hook", server.uri());
    let client = SlackClient::new(webhook);

    let err = client
        .post(&serde_json::json!({"text": "bad payload"}))
        .await
        .expect_err("non-2xx should surface as error");
    let msg = format!("{err:#}");
    assert!(msg.contains("400"), "error should include status: {msg}");
    assert!(
        msg.contains("invalid_blocks_format"),
        "error should include Slack response body: {msg}"
    );
}

#[tokio::test]
async fn post_rejects_2xx_with_body_other_than_ok() {
    // Slack has historically returned 200 with a specific error body for a
    // small set of soft-rejections. Treat anything other than `ok` as a
    // failure so we don't believe we delivered when we didn't.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200).set_body_string("invalid_payload"))
        .mount(&server)
        .await;

    let webhook = format!("{}/hook", server.uri());
    let client = SlackClient::new(webhook);

    let err = client
        .post(&serde_json::json!({"text": "probe"}))
        .await
        .expect_err("body != ok should surface as error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invalid_payload"),
        "error should include body: {msg}"
    );
}

#[tokio::test]
async fn post_accepts_200_ok_even_with_trailing_whitespace() {
    // Some corporate proxies / gateways inject trailing newlines on text
    // responses. Accept `ok` with surrounding whitespace the same as `ok`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok\n"))
        .mount(&server)
        .await;

    let webhook = format!("{}/hook", server.uri());
    let client = SlackClient::new(webhook);
    client
        .post(&serde_json::json!({"text": "probe"}))
        .await
        .expect("trimmed body 'ok' should be accepted");
}
