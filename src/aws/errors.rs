//! User-facing classification of AWS SDK errors.
//!
//! The AWS Rust SDK surfaces transport errors, signing errors, service
//! protocol errors, and credential-resolution errors through the same
//! `SdkError` tree. Pulling a clean "credentials are expired" signal out of
//! that tree via typed matching is fragile: the typed error kinds vary by
//! operation, wrapper layers change between SDK versions, and STS rejections
//! for expired tokens surface as opaque service errors.
//!
//! In exchange for robustness we walk the error-source chain as a single
//! lowercased string and match against a short whitelist of well-known
//! substrings. The patterns come straight from the AWS SDK and STS error
//! codes (`RequestExpired`, `ExpiredToken`, `AuthFailure`, etc.) and are
//! pinned as module consts so the heuristic is auditable.
//!
//! When a known pattern matches, we rewrite the error into an actionable
//! remediation message (how to re-auth). Everything else is passed through
//! with a `.context()` hint naming the failed operation.
//!
//! `map_aws_operation_error` is the only entry point. `operation` is the
//! human-readable name of what we were trying to do (e.g. `"list EC2
//! instances"`).

use std::error::Error as StdError;

/// Substrings that indicate the caller's credentials have expired. Lowercased
/// for case-insensitive matching against the full error source chain.
const EXPIRED_CREDENTIALS_MARKERS: &[&str] = &[
    "requestexpired",
    "request has expired",
    "expiredtoken",
    "token is expired",
];

/// Substrings that indicate the caller has no usable credentials (missing,
/// malformed, or rejected by AWS).
const MISSING_OR_INVALID_CREDENTIALS_MARKERS: &[&str] = &[
    "authfailure",
    "invalidclienttokenid",
    "unrecognizedclient",
    "unable to locate credentials",
    "no valid credential sources",
    "could not load credentials",
    "aws was not able to validate the provided access credentials",
];

/// Rewrite an AWS SDK error into a user-facing remediation message when we
/// recognize a credential-shaped failure. Otherwise wrap it with operation
/// context and pass it through unchanged.
pub fn map_aws_operation_error(err: crate::error::Error, operation: &str) -> anyhow::Error {
    let chain = error_chain_text(&err).to_ascii_lowercase();

    if is_expired_credentials_error(&chain) {
        return anyhow::anyhow!(
            "AWS credentials appear to be expired while trying to {operation}.\n\
\n\
How to fix:\n\
1. Re-authenticate your profile (SSO): `aws sso login --profile <profile>`\n\
2. Or refresh static credentials: `aws configure --profile <profile>`\n\
3. Verify identity: `aws sts get-caller-identity --profile <profile>`\n\
4. If this persists, confirm your system clock is correct (clock skew can trigger RequestExpired)."
        );
    }

    if is_missing_or_invalid_credentials_error(&chain) {
        return anyhow::anyhow!(
            "AWS credentials are missing or invalid while trying to {operation}.\n\
\n\
How to authenticate correctly:\n\
1. Choose a profile and export it: `export AWS_PROFILE=<profile>`\n\
2. Authenticate with SSO: `aws sso login --profile <profile>`\n\
3. Or configure access keys: `aws configure --profile <profile>`\n\
4. Verify access: `aws sts get-caller-identity --profile <profile>`"
        );
    }

    anyhow::Error::new(err).context(format!("failed to {operation}"))
}

fn error_chain_text(err: &(dyn StdError + 'static)) -> String {
    let mut out = String::new();
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&e.to_string());
        cur = e.source();
    }
    out
}

fn is_expired_credentials_error(chain: &str) -> bool {
    EXPIRED_CREDENTIALS_MARKERS
        .iter()
        .any(|m| chain.contains(m))
}

fn is_missing_or_invalid_credentials_error(chain: &str) -> bool {
    MISSING_OR_INVALID_CREDENTIALS_MARKERS
        .iter()
        .any(|m| chain.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_expired_credentials_messages() {
        let chain = "service error | unhandled error (RequestExpired) | Request has expired";
        assert!(is_expired_credentials_error(&chain.to_ascii_lowercase()));
    }

    #[test]
    fn classifies_invalid_credentials_messages() {
        let chain = "service error | unhandled error (AuthFailure) | validate access credentials";
        assert!(is_missing_or_invalid_credentials_error(&chain.to_ascii_lowercase()));
    }

    #[test]
    fn does_not_misclassify_unrelated_messages() {
        let chain = "throttling: rate exceeded";
        assert!(!is_expired_credentials_error(chain));
        assert!(!is_missing_or_invalid_credentials_error(chain));
    }
}
