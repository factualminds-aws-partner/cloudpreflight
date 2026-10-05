//! FinOps findings. Each rule states what the configuration shows and what to review;
//! none claims waste, because that needs runtime evidence the IaC does not contain.

use rust_decimal::Decimal;
use serde_json::Value;

use crate::model::{
    Assumption, ChangeEstimate, Confidence, Finding, Lookup, Resource, ResourceChange, ResourceEstimate,
    ResourceStatus, Severity,
};

pub fn findings(changes: &[ResourceChange], estimates: &[ChangeEstimate], assumptions: &[Assumption]) -> Vec<Finding> {
    let planned: Vec<&Resource> = changes.iter().filter_map(|change| change.after.as_ref()).collect();
    let mut findings = Vec::new();

    for resource in &planned {
        let estimate = estimates
            .iter()
            .find(|estimate| estimate.address == resource.address)
            .and_then(|estimate| estimate.after.as_ref());

        match resource.resource_type.as_str() {
            "aws_db_instance" => findings.extend(rds_multi_az(resource, estimate)),
            "aws_nat_gateway" => findings.extend(nat_traffic(resource, assumptions)),
            "aws_s3_bucket" => findings.extend(s3_lifecycle(resource, &planned)),
            "aws_ebs_volume" => findings.extend(ebs_gp2(resource, "type", true)),
            "aws_instance" => findings.extend(ebs_gp2(resource, "root_block_device.0.volume_type", false)),
            _ => {}
        }
    }

    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.rule_id.cmp(&b.rule_id))
            .then_with(|| a.resource.cmp(&b.resource))
    });
    findings
}

fn rds_multi_az(resource: &Resource, estimate: Option<&ResourceEstimate>) -> Option<Finding> {
    if resource.get("multi_az") != Lookup::Known(&Value::Bool(true)) {
        return None;
    }

    // Multi-AZ list rates are twice the Single-AZ rates, so half of a fully priced
    // instance is the increment. A partly priced one gives no trustworthy figure.
    let impact = estimate
        .filter(|estimate| estimate.status == ResourceStatus::Priced)
        .and_then(|estimate| estimate.monthly)
        .map(|monthly| monthly / Decimal::TWO);
    Some(Finding {
        severity: Severity::High,
        rule_id: "RDS-MULTIAZ-COST".into(),
        title: "RDS Multi-AZ adds significant fixed monthly cost".into(),
        resource: resource.address.clone(),
        reason: "The instance is configured with multi_az = true, which runs a standby in a second zone at roughly double the instance and storage rate.".into(),
        estimated_impact: impact,
        recommendation: "Confirm the availability requirement for this environment. Non-production environments often do not need a standby.".into(),
        confidence: Confidence::High,
    })
}

fn nat_traffic(resource: &Resource, assumptions: &[Assumption]) -> Option<Finding> {
    let traffic = assumptions
        .iter()
        .find(|assumption| assumption.scope == resource.address && assumption.key == "data_processed_gb");
    if traffic.is_some_and(|assumption| assumption.source.starts_with("config")) {
        return None;
    }

    Some(Finding {
        severity: Severity::Medium,
        rule_id: "NAT-TRAFFIC-UNDEFINED".into(),
        title: "NAT Gateway traffic assumptions are undefined".into(),
        resource: resource.address.clone(),
        reason: "NAT Gateways charge per GB processed on top of the hourly rate, and no traffic volume is set for this gateway in the config file.".into(),
        estimated_impact: None,
        recommendation: "Set usage.aws.nat_gateway.data_processed_gb from measured traffic, and review whether VPC endpoints for S3, DynamoDB or ECR would take traffic off the gateway.".into(),
        confidence: Confidence::Medium,
    })
}

fn s3_lifecycle(bucket: &Resource, planned: &[&Resource]) -> Option<Finding> {
    let inline_rule = !matches!(bucket.get("lifecycle_rule.0"), Lookup::Missing);
    let separate_rule = planned.iter().any(|resource| {
        resource.resource_type == "aws_s3_bucket_lifecycle_configuration"
            && (resource
                .refs
                .get("bucket")
                .is_some_and(|targets| targets.iter().any(|target| target == bucket.base_address()))
                || (resource.get_str("bucket").is_some() && resource.get_str("bucket") == bucket.get_str("bucket")))
    });
    if inline_rule || separate_rule {
        return None;
    }

    Some(Finding {
        severity: Severity::Low,
        rule_id: "S3-NO-LIFECYCLE".into(),
        title: "S3 lifecycle policy is not configured".into(),
        resource: bucket.address.clone(),
        reason: "No lifecycle configuration for this bucket was found in the scanned infrastructure, so objects and old versions stay in their original storage class indefinitely.".into(),
        estimated_impact: None,
        recommendation: "Review whether objects can transition to a colder storage class or expire, and whether incomplete multipart uploads should be aborted.".into(),
        confidence: Confidence::Medium,
    })
}

fn ebs_gp2(resource: &Resource, type_path: &str, gp2_when_unset: bool) -> Option<Finding> {
    let is_gp2 = match resource.get(type_path) {
        Lookup::Known(Value::String(volume_type)) => volume_type == "gp2",
        Lookup::Missing => gp2_when_unset,
        _ => false,
    };
    if !is_gp2 {
        return None;
    }

    Some(Finding {
        severity: Severity::Low,
        rule_id: "EBS-GP2-VOLUME".into(),
        title: "EBS volume uses gp2".into(),
        resource: resource.address.clone(),
        reason: "Based on the configured volume type, this volume uses gp2. gp3 has a lower per-GB list price in most regions and a baseline of 3,000 IOPS independent of size.".into(),
        estimated_impact: None,
        recommendation: "Review whether gp3 meets the performance requirement; volumes can be migrated in place.".into(),
        confidence: Confidence::Medium,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Action;
    use serde_json::json;

    fn create(resource: Resource) -> ResourceChange {
        ResourceChange {
            address: resource.address.clone(),
            resource_type: resource.resource_type.clone(),
            action: Action::Create,
            before: None,
            after: Some(resource),
        }
    }

    fn rule_ids(changes: &[ResourceChange], assumptions: &[Assumption]) -> Vec<String> {
        findings(changes, &[], assumptions)
            .into_iter()
            .map(|finding| finding.rule_id)
            .collect()
    }

    #[test]
    fn multi_az_is_flagged_only_when_enabled_and_sorted_first() {
        let on = Resource::new("aws_db_instance.a", "aws_db_instance", json!({"multi_az": true}));
        let off = Resource::new("aws_db_instance.b", "aws_db_instance", json!({"multi_az": false}));
        let volume = Resource::new("aws_ebs_volume.v", "aws_ebs_volume", json!({}));

        let found = findings(&[create(volume), create(off), create(on)], &[], &[]);

        let summary: Vec<_> = found
            .iter()
            .map(|f| (f.rule_id.as_str(), f.resource.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("RDS-MULTIAZ-COST", "aws_db_instance.a"),
                ("EBS-GP2-VOLUME", "aws_ebs_volume.v")
            ]
        );
    }

    #[test]
    fn nat_finding_disappears_once_traffic_is_configured() {
        let gateway = || create(Resource::new("aws_nat_gateway.n", "aws_nat_gateway", json!({})));
        let assumption = |source: &str| Assumption {
            scope: "aws_nat_gateway.n".into(),
            key: "data_processed_gb".into(),
            value: "500".into(),
            source: source.into(),
        };

        assert_eq!(
            rule_ids(&[gateway()], &[assumption("profile: standard (default)")]),
            vec!["NAT-TRAFFIC-UNDEFINED"]
        );
        assert!(rule_ids(&[gateway()], &[assumption("config: usage")]).is_empty());
    }

    #[test]
    fn s3_lifecycle_is_recognised_inline_by_reference_and_by_name() {
        let bucket = |name: &str| {
            Resource::new(
                &format!("aws_s3_bucket.{name}"),
                "aws_s3_bucket",
                json!({"bucket": name}),
            )
        };
        let mut by_reference = Resource::new(
            "aws_s3_bucket_lifecycle_configuration.a",
            "aws_s3_bucket_lifecycle_configuration",
            json!({}),
        );
        by_reference
            .refs
            .insert("bucket".into(), vec!["aws_s3_bucket.a".into()]);
        let by_name = Resource::new(
            "aws_s3_bucket_lifecycle_configuration.b",
            "aws_s3_bucket_lifecycle_configuration",
            json!({"bucket": "b"}),
        );
        let inline = Resource::new(
            "aws_s3_bucket.c",
            "aws_s3_bucket",
            json!({"lifecycle_rule": [{"enabled": true}]}),
        );

        let changes = [
            create(bucket("a")),
            create(bucket("b")),
            create(inline),
            create(bucket("d")),
            create(by_reference),
            create(by_name),
        ];
        let found = findings(&changes, &[], &[]);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].resource, "aws_s3_bucket.d");
    }

    #[test]
    fn gp3_and_unknown_volume_types_are_not_flagged() {
        let gp3 = Resource::new("aws_ebs_volume.a", "aws_ebs_volume", json!({"type": "gp3"}));
        let instance = Resource::new("aws_instance.i", "aws_instance", json!({}));
        assert!(rule_ids(&[create(gp3), create(instance)], &[]).is_empty());
    }
}
