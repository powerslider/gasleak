//! EC2 API access.
//!
//! [`list_instances`] wraps `DescribeInstances` pagination. [`to_records`]
//! transforms the raw SDK `Instance` shape into the domain's
//! [`InstanceRecord`], filtering by state, resolving the launcher identity,
//! and deriving `created_at` from the root-volume attach time so stop/start
//! cycles don't reset the "how old is this box?" signal.

use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_ec2::types::Instance;
use jiff::Timestamp;
use std::collections::BTreeMap;

use crate::aws::aws_datetime_to_jiff;
use crate::error::{Error, Result};
use crate::identity::resolve_launched_by;
use crate::model::{InstanceRecord, InstanceState};

pub async fn list_instances(ec2: &Ec2Client) -> Result<Vec<Instance>> {
    let mut pages = ec2.describe_instances().into_paginator().send();
    let mut instances = Vec::new();

    while let Some(page) = pages.next().await {
        let page = page.map_err(|e| Error::aws("ec2:DescribeInstances", e))?;
        for reservation in page.reservations() {
            for instance in reservation.instances() {
                instances.push(instance.clone());
            }
        }
    }

    Ok(instances)
}

pub fn to_records(
    instances: Vec<Instance>,
    region: &str,
    now: Timestamp,
    keep_states: &[InstanceState],
) -> Result<Vec<InstanceRecord>> {
    let mut out = Vec::with_capacity(instances.len());

    for instance in instances {
        let Some(instance_id) = instance.instance_id().map(str::to_string) else {
            continue;
        };

        let state = InstanceState::from_sdk(instance.state().and_then(|s| s.name()));
        if !keep_states.contains(&state) {
            continue;
        }

        let Some(launch_time_sdk) = instance.launch_time() else {
            continue;
        };
        let launch_time = aws_datetime_to_jiff(launch_time_sdk)?;
        let last_uptime_seconds = now.as_second().saturating_sub(launch_time.as_second());

        // Original creation time: root EBS volume's AttachTime survives stop/start.
        // Fall back to launch_time for instance-store or missing-data cases.
        let created_at = root_volume_attach_time(&instance)?.unwrap_or(launch_time);
        let total_age_seconds = now.as_second().saturating_sub(created_at.as_second());

        let instance_type = instance
            .instance_type()
            .map(|it| it.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let az = instance
            .placement()
            .and_then(|p| p.availability_zone())
            .map(str::to_string);

        let iam_instance_profile = instance
            .iam_instance_profile()
            .and_then(|p| p.arn())
            .map(str::to_string);

        let key_name = instance.key_name().map(str::to_string);

        let tags: BTreeMap<String, String> = instance
            .tags()
            .iter()
            .filter_map(|t| match (t.key(), t.value()) {
                (Some(k), Some(v)) => Some((k.to_string(), v.to_string())),
                _ => None,
            })
            .collect();

        let (launched_by, launched_by_source) = resolve_launched_by(
            &tags,
            iam_instance_profile.as_deref(),
            key_name.as_deref(),
        );

        out.push(InstanceRecord {
            instance_id,
            launched_by,
            launched_by_source,
            launch_time,
            created_at,
            last_uptime_seconds,
            total_age_seconds,
            instance_type,
            state,
            region: region.to_string(),
            az,
            iam_instance_profile,
            key_name,
            tags,
            estimated_cost_usd: None,
            cpu: None,
        });
    }

    // Stable ordering: oldest first (by creation), then instance id.
    out.sort_by(|a, b| {
        b.total_age_seconds
            .cmp(&a.total_age_seconds)
            .then_with(|| a.instance_id.cmp(&b.instance_id))
    });

    Ok(out)
}

fn root_volume_attach_time(instance: &Instance) -> Result<Option<Timestamp>> {
    let Some(root_device) = instance.root_device_name() else {
        return Ok(None);
    };
    for mapping in instance.block_device_mappings() {
        if mapping.device_name() != Some(root_device) {
            continue;
        }
        let Some(ebs) = mapping.ebs() else {
            continue;
        };
        let Some(attach_time) = ebs.attach_time() else {
            continue;
        };
        return Ok(Some(aws_datetime_to_jiff(attach_time)?));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ec2::types::InstanceStateName as SdkInstanceStateName;
    use aws_sdk_ec2::types::{Instance as SdkInstance, InstanceState as SdkInstanceState};
    use aws_smithy_types::DateTime as SdkDateTime;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::new(secs, 0).expect("valid timestamp")
    }

    fn instance_with_state_and_launch(
        id: &str,
        state: SdkInstanceStateName,
        launch_secs: i64,
    ) -> SdkInstance {
        SdkInstance::builder()
            .instance_id(id)
            .state(SdkInstanceState::builder().name(state).build())
            .launch_time(SdkDateTime::from_secs(launch_secs))
            .build()
    }

    #[test]
    fn to_records_filters_states_and_sorts_stably() {
        let instances = vec![
            instance_with_state_and_launch("i-2", SdkInstanceStateName::Running, 900),
            instance_with_state_and_launch("i-1", SdkInstanceStateName::Running, 900),
            instance_with_state_and_launch("i-3", SdkInstanceStateName::Running, 950),
            instance_with_state_and_launch("i-stop", SdkInstanceStateName::Stopped, 900),
        ];

        let keep_states = vec![InstanceState::Running];
        let records =
            to_records(instances, "us-east-1", ts(1000), &keep_states).expect("to_records should succeed");

        assert_eq!(records.len(), 3);
        assert_eq!(records[0].instance_id, "i-1");
        assert_eq!(records[1].instance_id, "i-2");
        assert_eq!(records[2].instance_id, "i-3");
        assert_eq!(records[0].last_uptime_seconds, 100);
        assert_eq!(records[2].last_uptime_seconds, 50);
        assert_eq!(records[0].region, "us-east-1");
    }
}
