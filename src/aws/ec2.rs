use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_ec2::types::Instance;
use jiff::Timestamp;
use std::collections::BTreeMap;

use crate::aws::aws_datetime_to_jiff;
use crate::error::{Error, Result};
use crate::model::{InstanceRecord, InstanceState, resolve_launched_by};

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
        let uptime_seconds = now.as_second().saturating_sub(launch_time.as_second());

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
            uptime_seconds,
            instance_type,
            state,
            region: region.to_string(),
            az,
            iam_instance_profile,
            key_name,
            tags,
            cpu: None,
        });
    }

    // Stable ordering: longest uptime first, then instance id.
    out.sort_by(|a, b| {
        b.uptime_seconds
            .cmp(&a.uptime_seconds)
            .then_with(|| a.instance_id.cmp(&b.instance_id))
    });

    Ok(out)
}
