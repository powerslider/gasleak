//! Slack Block Kit reporting for `list`, `stale`, and `explain`.
//!
//! Webhook-only: no Slack app install, no bot token, no interactivity. URL
//! buttons are allowed (they navigate to AWS Console deep-links and the
//! optional `[slack] report_url`), but ack / snooze flows are out of scope —
//! they require a long-running endpoint to receive click callbacks.
//!
//! Design anchors:
//! - **Severity is the primary visual anchor.** Each severity bucket becomes
//!   its own `attachment` with a colored left bar. Block Kit alone has no
//!   color primitive; `attachments` is the only path.
//! - **Money above the fold.** The money-first section block sits right
//!   under the header so on-call mobile users grasp the stakes before
//!   scrolling.
//! - **No silent drops for urgent findings.** High- and Medium-severity rows
//!   always render as full section blocks. Only the Low-severity tail
//!   compresses into a rollup block past `max_flagged_rows`.
//! - **Progressive disclosure.** Top-N full rows per severity, overflow
//!   into a single rollup section. A configurable `report_url` button at
//!   the top points wherever the team wants the full data.
//!
//! This module hosts [`SlackClient`] (which POSTs payloads) and the typed
//! [`SlackRuntimeConfig`] / [`MentionThreshold`]. The render layer lives in
//! [`render`].

use anyhow::{Context, bail};
use serde_json::Value;
use std::time::Duration;

use crate::error::Error;
use crate::staleness::Severity;

pub mod render;

pub use render::{render_explain, render_list, render_stale};

/// Default POST timeout. Cron runs should never hang on a Slack hiccup; 15
/// seconds is enough for a round-trip with a slow TLS handshake but short
/// enough that a broken endpoint fails the run quickly.
const DEFAULT_POST_TIMEOUT: Duration = Duration::from_secs(15);

/// Resolved runtime config: the file-shape [`crate::config::SlackConfig`]
/// merged with the env-var webhook fallback, with values validated so the
/// render path can trust them.
#[derive(Debug, Clone)]
pub struct SlackRuntimeConfig {
    pub webhook_url: String,
    pub max_flagged_rows: usize,
    pub report_url: Option<String>,
    pub mention_threshold: MentionThreshold,
}

impl SlackRuntimeConfig {
    /// Merge the file config with `$GASLEAK_SLACK_WEBHOOK`. Returns a typed
    /// [`Error::SlackWebhookMissing`] if neither source surfaces a URL, or
    /// [`Error::SlackConfigInvalid`] if any field fails validation — so
    /// config typos are loud rather than silently defaulted.
    pub fn resolve(
        file_cfg: &crate::config::SlackConfig,
        env_webhook: Option<String>,
    ) -> Result<Self, Error> {
        let webhook_url = file_cfg
            .webhook_url
            .clone()
            .or(env_webhook)
            .ok_or(Error::SlackWebhookMissing)?;
        let mention_threshold =
            MentionThreshold::parse(file_cfg.mention_owner_at_severity.as_deref())?;
        Ok(Self {
            webhook_url,
            max_flagged_rows: file_cfg.max_flagged_rows.unwrap_or(10),
            report_url: file_cfg.report_url.clone(),
            mention_threshold,
        })
    }
}

/// At-or-above threshold for rendering `OwnerSlack` as a raw `@handle`
/// (Slack auto-pings) vs. code-formatted (no ping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionThreshold {
    Low,
    Medium,
    High,
    Never,
}

impl MentionThreshold {
    /// Parse the `mention_owner_at_severity` config value. `None` defaults
    /// to `High`. Unknown values are rejected with a message that lists the
    /// four valid options — typos don't silently fall through to default.
    pub fn parse(raw: Option<&str>) -> Result<Self, Error> {
        match raw.map(str::to_ascii_lowercase).as_deref() {
            None => Ok(Self::High),
            Some("low") => Ok(Self::Low),
            Some("medium") => Ok(Self::Medium),
            Some("high") => Ok(Self::High),
            Some("never") => Ok(Self::Never),
            Some(other) => Err(Error::SlackConfigInvalid(format!(
                "mention_owner_at_severity = {other:?}; expected one of: \
                 low, medium, high, never"
            ))),
        }
    }

    pub(crate) fn should_mention(self, actual: Severity) -> bool {
        let rank = |s: Severity| match s {
            Severity::Info => 0,
            Severity::Low => 1,
            Severity::Medium => 2,
            Severity::High => 3,
        };
        let threshold = match self {
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Never => i32::MAX,
        };
        rank(actual) >= threshold
    }
}

/// Minimal wrapper around a Slack incoming-webhook POST. Uses
/// [`DEFAULT_POST_TIMEOUT`] when constructed via [`SlackClient::new`]; callers
/// that want to share a pooled [`reqwest::Client`] (for multi-post runs, or
/// to inherit a caller-provided proxy config) should use
/// [`SlackClient::with_client`].
pub struct SlackClient {
    http: reqwest::Client,
    webhook: String,
}

impl SlackClient {
    /// Uses an internal [`reqwest::Client`] with a 15-second POST timeout.
    /// If building the client fails (extremely unlikely — only happens when
    /// the TLS backend can't initialize), falls back to a default client so
    /// the caller still gets a usable instance.
    pub fn new(webhook: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_POST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http, webhook }
    }

    /// Share an existing `reqwest::Client`. Useful when the caller already
    /// maintains a pool (e.g. future multi-post rendering paths).
    pub fn with_client(http: reqwest::Client, webhook: String) -> Self {
        Self { http, webhook }
    }

    /// POST a Block Kit payload. On non-2xx or any response body other than
    /// `ok`, surfaces Slack's response text so schema violations
    /// (`invalid_blocks`, `invalid_payload`, …) reach the operator intact.
    pub async fn post(&self, payload: &Value) -> anyhow::Result<()> {
        let payload_bytes = serde_json::to_vec(payload)
            .context("failed to serialize Slack payload before POST")?;
        tracing::debug!(
            bytes = payload_bytes.len(),
            blocks = payload
                .get("blocks")
                .and_then(|v| v.as_array())
                .map(Vec::len)
                .unwrap_or(0),
            attachments = payload
                .get("attachments")
                .and_then(|v| v.as_array())
                .map(Vec::len)
                .unwrap_or(0),
            "posting to Slack webhook"
        );
        let resp = self
            .http
            .post(&self.webhook)
            .header("Content-Type", "application/json")
            .body(payload_bytes)
            .send()
            .await
            .context("Slack webhook POST failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::debug!(status = %status, body = %body, "Slack webhook response");
        if !status.is_success() || body.trim() != "ok" {
            bail!("Slack webhook rejected payload: {status} — {body}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_threshold_default_is_high_when_unset() {
        assert_eq!(MentionThreshold::parse(None).unwrap(), MentionThreshold::High);
    }

    #[test]
    fn mention_threshold_parses_each_valid_variant_case_insensitive() {
        assert_eq!(MentionThreshold::parse(Some("low")).unwrap(), MentionThreshold::Low);
        assert_eq!(MentionThreshold::parse(Some("MEDIUM")).unwrap(), MentionThreshold::Medium);
        assert_eq!(MentionThreshold::parse(Some("High")).unwrap(), MentionThreshold::High);
        assert_eq!(MentionThreshold::parse(Some("Never")).unwrap(), MentionThreshold::Never);
    }

    #[test]
    fn mention_threshold_rejects_typos_with_listed_options() {
        let err = MentionThreshold::parse(Some("hihg")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mention_owner_at_severity"));
        assert!(msg.contains("low, medium, high, never"));
    }

    #[test]
    fn should_mention_obeys_threshold_rank() {
        use MentionThreshold::*;
        assert!(High.should_mention(Severity::High));
        assert!(!High.should_mention(Severity::Medium));
        assert!(Medium.should_mention(Severity::High));
        assert!(Medium.should_mention(Severity::Medium));
        assert!(!Medium.should_mention(Severity::Low));
        assert!(Low.should_mention(Severity::Low));
        assert!(!Never.should_mention(Severity::High));
    }

    #[test]
    fn resolve_missing_webhook_returns_typed_error() {
        let file_cfg = crate::config::SlackConfig::default();
        let err = SlackRuntimeConfig::resolve(&file_cfg, None).unwrap_err();
        assert!(matches!(err, Error::SlackWebhookMissing));
    }

    #[test]
    fn resolve_prefers_file_over_env() {
        let file_cfg = crate::config::SlackConfig {
            webhook_url: Some("from-file".into()),
            ..Default::default()
        };
        let c = SlackRuntimeConfig::resolve(&file_cfg, Some("from-env".into())).unwrap();
        assert_eq!(c.webhook_url, "from-file");
    }

    #[test]
    fn resolve_falls_back_to_env_when_file_empty() {
        let file_cfg = crate::config::SlackConfig::default();
        let c = SlackRuntimeConfig::resolve(&file_cfg, Some("from-env".into())).unwrap();
        assert_eq!(c.webhook_url, "from-env");
    }
}
