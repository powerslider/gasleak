pub mod cloudwatch;
pub mod ec2;

use crate::error::{Error, Result};
use jiff::Timestamp;

pub(crate) fn aws_datetime_to_jiff(dt: &aws_smithy_types::DateTime) -> Result<Timestamp> {
    let secs = dt.secs();
    let nanos = i32::try_from(dt.subsec_nanos())
        .map_err(|e| Error::InvalidTimestamp(format!("subsec_nanos out of range: {e}")))?;
    Timestamp::new(secs, nanos).map_err(|e| Error::InvalidTimestamp(e.to_string()))
}
