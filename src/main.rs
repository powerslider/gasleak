use aws_sdk_ec2::Client as Ec2Client;
use std::error::Error;

#[derive(Debug)]
struct InstanceRecord {
    instance_id: String,
    launched_by: String,
    started_at: String,
    instance_type: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let ec2 = Ec2Client::new(&config);

    let instances = list_instances(&ec2).await?;

    if instances.is_empty() {
        println!("No EC2 instances found.");
        return Ok(());
    }

    let mut records = Vec::with_capacity(instances.len());

    for instance in instances {
        let Some(instance_id) = instance.instance_id().map(ToString::to_string) else {
            continue;
        };

        let launched_by = launched_by_from_tags(&instance)
            .unwrap_or_else(|| "unknown".to_string());
        let started_at = instance
            .launch_time()
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let instance_type = instance
            .instance_type()
            .map(|it| it.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        records.push(InstanceRecord {
            instance_id,
            launched_by,
            started_at,
            instance_type,
        });
    }

    print_records(&records);
    Ok(())
}

async fn list_instances(
    ec2: &Ec2Client,
) -> Result<Vec<aws_sdk_ec2::types::Instance>, Box<dyn Error>> {
    let mut next_token: Option<String> = None;
    let mut instances = Vec::new();

    loop {
        let response = ec2
            .describe_instances()
            .set_next_token(next_token.clone())
            .send()
            .await?;

        for reservation in response.reservations() {
            for instance in reservation.instances() {
                instances.push(instance.clone());
            }
        }

        next_token = response.next_token().map(ToString::to_string);
        if next_token.is_none() {
            break;
        }
    }

    Ok(instances)
}

fn launched_by_from_tags(instance: &aws_sdk_ec2::types::Instance) -> Option<String> {
    let preferred_keys = [
        "launched_by",
        "LaunchedBy",
        "launched-by",
        "CreatedBy",
        "created_by",
        "created-by",
        "Owner",
        "owner",
    ];

    for key in preferred_keys {
        if let Some(value) = instance.tags().iter().find_map(|tag| {
            if tag.key() == Some(key) {
                tag.value().map(ToString::to_string)
            } else {
                None
            }
        }) {
            return Some(value);
        }
    }

    None
}

fn print_records(records: &[InstanceRecord]) {
    let id_w = records
        .iter()
        .map(|r| r.instance_id.len())
        .max()
        .unwrap_or(11)
        .max("instance_id".len());
    let by_w = records
        .iter()
        .map(|r| r.launched_by.len())
        .max()
        .unwrap_or(11)
        .max("launched_by".len());
    let at_w = records
        .iter()
        .map(|r| r.started_at.len())
        .max()
        .unwrap_or(10)
        .max("started_at".len());
    let type_w = records
        .iter()
        .map(|r| r.instance_type.len())
        .max()
        .unwrap_or(13)
        .max("instance_type".len());

    println!(
        "{:<id_w$}  {:<by_w$}  {:<at_w$}  {:<type_w$}",
        "instance_id", "launched_by", "started_at", "instance_type"
    );
    println!(
        "{:-<id_w$}  {:-<by_w$}  {:-<at_w$}  {:-<type_w$}",
        "", "", "", ""
    );

    for r in records {
        println!(
            "{:<id_w$}  {:<by_w$}  {:<at_w$}  {:<type_w$}",
            r.instance_id, r.launched_by, r.started_at, r.instance_type
        );
    }
}
