use jiff::Timestamp;
use serde::Serialize;
use std::collections::BTreeMap;

const MANAGED_BY_PREFIX: &str = "gasleak/";

/// Parsed view of the contract tags on an EC2 instance.
///
/// The contract is minimal. We want to know *who* owns the instance, *how to
/// reach them*, and *when they've committed it should die*. Everything else
/// (environment classification, persistence tiers, do-not-disturb windows) has
/// been rolled into the single `ExpiresAt` lever. The owner's declared
/// deadline is the only policy the tool needs.
#[derive(Debug, Clone, Serialize)]
pub struct ContractView {
    /// `true` when the `ManagedBy` tag starts with `gasleak/`.
    pub managed_by_gasleak: bool,
    /// Free-form attribution (usually the caller's IAM principal ARN or team name).
    pub owner: Option<String>,
    /// Where to send confirmation nudges: `@handle` for a person, `#channel` for a team.
    pub owner_slack: Option<String>,
    /// Declared end-of-life timestamp. Treated as authoritative by the rules.
    pub expires_at: Option<Timestamp>,
}

impl ContractView {
    pub fn from_tags(tags: &BTreeMap<String, String>) -> Self {
        let managed_by_gasleak = tags
            .get("ManagedBy")
            .is_some_and(|v| v.starts_with(MANAGED_BY_PREFIX));

        let owner = tags.get("Owner").cloned();
        let owner_slack = tags.get("OwnerSlack").cloned();
        let expires_at = tags
            .get("ExpiresAt")
            .and_then(|v| v.parse::<Timestamp>().ok());

        ContractView {
            managed_by_gasleak,
            owner,
            owner_slack,
            expires_at,
        }
    }
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
    fn managed_by_gasleak_is_version_agnostic() {
        let c = ContractView::from_tags(&tags(&[("ManagedBy", "gasleak/0.1.0")]));
        assert!(c.managed_by_gasleak);
        let c2 = ContractView::from_tags(&tags(&[("ManagedBy", "gasleak/99.42.1")]));
        assert!(c2.managed_by_gasleak);
        let c3 = ContractView::from_tags(&tags(&[("ManagedBy", "terraform")]));
        assert!(!c3.managed_by_gasleak);
        let c4 = ContractView::from_tags(&tags(&[]));
        assert!(!c4.managed_by_gasleak);
    }

    #[test]
    fn parses_expires_at() {
        let c = ContractView::from_tags(&tags(&[("ExpiresAt", "2026-05-01T00:00:00Z")]));
        assert!(c.expires_at.is_some());

        let c_bad = ContractView::from_tags(&tags(&[("ExpiresAt", "tomorrow")]));
        assert!(c_bad.expires_at.is_none());
    }

    #[test]
    fn parses_owner_fields() {
        let c = ContractView::from_tags(&tags(&[
            ("Owner", "arn:aws:iam::123:user/tsvetan"),
            ("OwnerSlack", "@tsvetan"),
        ]));
        assert_eq!(c.owner.as_deref(), Some("arn:aws:iam::123:user/tsvetan"));
        assert_eq!(c.owner_slack.as_deref(), Some("@tsvetan"));
    }
}
