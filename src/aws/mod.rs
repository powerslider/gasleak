//! Thin wrappers around the AWS SDK.
//!
//! - [`ec2`]: `DescribeInstances` pagination + transform to `InstanceRecord`.
//! - [`cloudwatch`]: `GetMetricData` fan-out and CPU summarization.
//! - [`errors`]: user-facing classification of SDK errors (expired creds,
//!   missing creds, etc.).

pub mod cloudwatch;
pub mod ec2;
pub mod errors;

use crate::error::{Error, Result};
use jiff::Timestamp;

pub(crate) fn aws_datetime_to_jiff(dt: &aws_smithy_types::DateTime) -> Result<Timestamp> {
    let secs = dt.secs();
    let nanos = i32::try_from(dt.subsec_nanos())
        .map_err(|e| Error::InvalidTimestamp(format!("subsec_nanos out of range: {e}")))?;
    Timestamp::new(secs, nanos).map_err(|e| Error::InvalidTimestamp(e.to_string()))
}
