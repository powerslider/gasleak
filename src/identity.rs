//! Best-effort attribution of an EC2 instance to a human or team.
//!
//! AWS gives us no direct "who launched this" signal. We combine four sources
//! in descending confidence order:
//!
//! 1. Owner-ish tags (`Owner`, `LaunchedBy`, and their case variants).
//!    Preferred when the value looks like an email or a useful identity.
//! 2. SSH `key_name`, de-fluffed (e.g. `austin-test` -> `austin`).
//! 3. IAM instance profile role name (last path segment).
//! 4. The `Name` tag, when it embeds a person-like prefix (`alice-dev` -> `alice`).
//!
//! The output is `(Option<String>, LaunchedBySource)` where the tag records
//! which bucket the value came from so the report can flag low-confidence
//! attributions.

use std::collections::BTreeMap;

use crate::model::LaunchedBySource;

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
const NAME_TAG_KEY: &str = "Name";

const GENERIC_IDENTITY_VALUES: &[&str] = &[
    "dev",
    "small",
    "firewood",
    "minion",
    "prod",
    "production",
    "staging",
    "stage",
    "qa",
    "test",
    "testing",
    "default",
    "unknown",
    "none",
    "n/a",
    "na",
];

pub fn resolve_launched_by(
    tags: &BTreeMap<String, String>,
    iam_profile_arn: Option<&str>,
    key_name: Option<&str>,
) -> (Option<String>, LaunchedBySource) {
    for key in PREFERRED_TAG_KEYS {
        if let Some(value) = tags.get(*key)
            && looks_like_email(value)
        {
            return (Some(value.clone()), LaunchedBySource::Tag);
        }
    }

    for key in PREFERRED_TAG_KEYS {
        if let Some(value) = tags.get(*key)
            && is_useful_identity(value)
        {
            return (Some(value.clone()), LaunchedBySource::Tag);
        }
    }

    if let Some(key) = key_name
        && let Some(identity) = identity_from_structured_label(key)
    {
        return (Some(identity), LaunchedBySource::KeyName);
    }

    if let Some(key) = key_name
        && is_useful_identity(key)
    {
        return (Some(key.to_string()), LaunchedBySource::KeyName);
    }
    if let Some(arn) = iam_profile_arn
        && let Some((_, role)) = arn.rsplit_once('/')
        && is_useful_identity(role)
    {
        return (Some(role.to_string()), LaunchedBySource::IamRole);
    }
    if let Some(name_tag) = tags.get(NAME_TAG_KEY)
        && let Some(identity) = identity_from_name_tag(name_tag)
    {
        return (Some(identity), LaunchedBySource::Tag);
    }

    // Last-resort fallback: return any available signal even if low confidence
    // (e.g. "dev") so operators still get a clue instead of pure unknown.
    for key in PREFERRED_TAG_KEYS {
        if let Some(value) = tags.get(*key)
            && !value.trim().is_empty()
        {
            return (Some(value.clone()), LaunchedBySource::Tag);
        }
    }
    if let Some(key) = key_name
        && !key.is_empty()
        && !contains_generic_token(key)
    {
        return (Some(key.to_string()), LaunchedBySource::KeyName);
    }
    if let Some(arn) = iam_profile_arn
        && let Some((_, role)) = arn.rsplit_once('/')
        && !role.is_empty()
    {
        return (Some(role.to_string()), LaunchedBySource::IamRole);
    }

    (None, LaunchedBySource::Unknown)
}

fn looks_like_email(value: &str) -> bool {
    let trimmed = value.trim();
    let mut parts = trimmed.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    parts.next().is_none() && !local.is_empty() && domain.contains('.')
}

fn is_useful_identity(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    if looks_like_email(trimmed) {
        return true;
    }

    let normalized = trimmed.to_ascii_lowercase();
    if GENERIC_IDENTITY_VALUES.contains(&normalized.as_str()) {
        return false;
    }
    if contains_generic_token(trimmed) {
        return false;
    }

    // Prefer values that look person/team-specific over generic environment labels.
    trimmed.len() >= 4
        && (trimmed.contains('-')
            || trimmed.contains('_')
            || trimmed.contains('.')
            || trimmed.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn contains_generic_token(value: &str) -> bool {
    value
        .split(['-', '_', '.'])
        .filter(|p| !p.trim().is_empty())
        .map(|p| p.trim().to_ascii_lowercase())
        .any(|p| GENERIC_IDENTITY_VALUES.contains(&p.as_str()))
}

fn identity_from_name_tag(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }

    if looks_like_email(trimmed) {
        return Some(trimmed.to_string());
    }

    if let Some(identity) = identity_from_structured_label(trimmed) {
        return Some(identity);
    }

    let mut parts = trimmed
        .split(['-', '_', '.'])
        .filter(|p| !p.trim().is_empty());
    let first = parts.next()?.trim();
    let second = parts.next().map(str::trim);

    if first.len() >= 3
        && let Some(second) = second
    {
        let second_norm = second.to_ascii_lowercase();
        if GENERIC_IDENTITY_VALUES.contains(&second_norm.as_str()) && !is_generic_value(first) {
            return Some(first.to_string());
        }
    }

    None
}

fn identity_from_structured_label(value: &str) -> Option<String> {
    let tokens: Vec<&str> = value
        .split(['-', '_', '.'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.len() < 2 {
        return None;
    }

    let first = tokens[0];
    if first.len() < 3 || is_generic_value(first) || !first.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }

    let has_generic_suffix = tokens
        .iter()
        .skip(1)
        .any(|t| is_generic_value(t) || t.chars().all(|c| c.is_ascii_digit()));

    if has_generic_suffix {
        return Some(first.to_string());
    }

    None
}

fn is_generic_value(value: &str) -> bool {
    GENERIC_IDENTITY_VALUES.contains(&value.trim().to_ascii_lowercase().as_str())
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
    fn resolve_prefers_email_tag_value() {
        let (v, s) = resolve_launched_by(
            &tags(&[("Owner", "alice@example.com")]),
            Some("arn:aws:iam::1:instance-profile/my-role"),
            Some("mykey"),
        );
        assert_eq!(v.as_deref(), Some("alice@example.com"));
        assert_eq!(s, LaunchedBySource::Tag);
    }

    #[test]
    fn resolve_ignores_generic_dev_value() {
        let (v, s) = resolve_launched_by(&tags(&[("Owner", "dev")]), None, None);
        assert_eq!(v.as_deref(), Some("dev"));
        assert_eq!(s, LaunchedBySource::Tag);
    }

    #[test]
    fn resolve_uses_name_tag_for_person_hint() {
        let (v, s) = resolve_launched_by(&tags(&[("Name", "aaron-dev")]), None, Some("dev"));
        assert_eq!(v.as_deref(), Some("aaron"));
        assert_eq!(s, LaunchedBySource::Tag);
    }

    #[test]
    fn resolve_name_tag_not_used_for_generic_prefix() {
        let (v, s) = resolve_launched_by(&tags(&[("Name", "dev-staging")]), None, Some("dev"));
        assert!(v.is_none());
        assert_eq!(s, LaunchedBySource::Unknown);
    }

    #[test]
    fn resolve_prefers_keyname_over_project_name() {
        let (v, s) = resolve_launched_by(&tags(&[("Name", "firewood-opt2")]), None, Some("elvis"));
        assert_eq!(v.as_deref(), Some("elvis"));
        assert_eq!(s, LaunchedBySource::KeyName);
    }

    #[test]
    fn resolve_does_not_extract_generic_small_from_name() {
        let (v, s) = resolve_launched_by(&tags(&[("Name", "small-dev")]), None, Some("dev"));
        assert!(v.is_none());
        assert_eq!(s, LaunchedBySource::Unknown);
    }

    #[test]
    fn resolve_ignores_small_minion_alias() {
        let (v, s) =
            resolve_launched_by(&tags(&[("Name", "small-minion")]), None, Some("small-minion"));
        assert!(v.is_none());
        assert_eq!(s, LaunchedBySource::Unknown);
    }

    #[test]
    fn resolve_extracts_austin_from_keyname_test_suffix() {
        let (v, s) = resolve_launched_by(&tags(&[]), None, Some("austin-test"));
        assert_eq!(v.as_deref(), Some("austin"));
        assert_eq!(s, LaunchedBySource::KeyName);
    }

    #[test]
    fn resolve_extracts_austin_from_name_tag_testing_suffix() {
        let (v, s) = resolve_launched_by(
            &tags(&[("Name", "austin-state-sync-testing-1")]),
            None,
            Some("dev"),
        );
        assert_eq!(v.as_deref(), Some("austin"));
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
