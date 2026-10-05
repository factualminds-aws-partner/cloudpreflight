//! AWS resource normaliser. Computes the derived attributes (prefixed `_`) that the
//! mapping files reference but that need more than a plain attribute lookup.

use std::str::FromStr;

use rust_decimal::Decimal;
use serde_json::Value;

use crate::model::{Input, Lookup, Resource};

pub const FALLBACK_REGION: &str = "us-east-1";

/// Fills in regions and derived attributes. Returns true when any resource had to fall
/// back to `default_region` because neither it nor its provider block names one.
pub fn normalise(input: &mut Input, default_region: &str) -> bool {
    let provider_region = input.provider_regions.get("aws").cloned();
    let mut region_assumed = false;

    // Both sides of a change are normalised against their own snapshot, so a service
    // being updated resolves its task definition as it was before and as it will be.
    let before: Vec<Resource> = input.changes.iter().filter_map(|c| c.before.clone()).collect();
    let after: Vec<Resource> = input.changes.iter().filter_map(|c| c.after.clone()).collect();

    for change in &mut input.changes {
        for (side, snapshot) in [(&mut change.before, &before), (&mut change.after, &after)] {
            let Some(resource) = side else {
                continue;
            };
            if resource.provider != "aws" {
                continue;
            }

            if resource.region.is_none() {
                resource.region = match (resource.get_str("region"), &provider_region) {
                    (Some(region), _) => Some(region.to_string()),
                    (None, Some(region)) => Some(region.clone()),
                    (None, None) => {
                        region_assumed = true;
                        Some(default_region.to_string())
                    }
                };
            }
            derive(resource, snapshot);
        }
    }

    region_assumed
}

fn derive(resource: &mut Resource, all: &[Resource]) {
    match resource.resource_type.as_str() {
        "aws_instance" => {
            let tenancy = match resource.get_str("tenancy") {
                Some("dedicated") => "Dedicated",
                Some("host") => "Host",
                _ => "Shared",
            };
            resource.set_derived("_tenancy", tenancy);
        }
        "aws_db_instance" => derive_db_instance(resource),
        "aws_lb" | "aws_alb" => {
            let (family, operation) = match resource.get_str("load_balancer_type") {
                Some("network") => ("Load Balancer-Network", "LoadBalancing:Network"),
                Some("gateway") => ("Load Balancer-Gateway", "LoadBalancing:Gateway"),
                _ => ("Load Balancer-Application", "LoadBalancing:Application"),
            };
            resource.set_derived("_lb_family", family);
            resource.set_derived("_lb_operation", operation);
        }
        "aws_ecs_service" => derive_ecs_service(resource, all),
        "aws_lambda_function" => {
            let arm = resource.get_str("architectures.0") == Some("arm64");
            let suffix = if arm { "-ARM" } else { "" };
            resource.set_derived("_request_usagetype", format!("Request{suffix}"));
            resource.set_derived("_duration_usagetype", format!("Lambda-GB-Second{suffix}"));
        }
        "aws_dynamodb_table" => {
            copy_or_default(resource, "billing_mode", "_billing_mode", "PROVISIONED");
        }
        "aws_elasticache_cluster" => {
            derive_cache_engine(resource, None);
            match resource.get("num_cache_nodes") {
                Lookup::Unknown(reason) => mark_unknown(resource, "_nodes", reason.to_string()),
                Lookup::Known(nodes) => resource.set_derived("_nodes", nodes.clone()),
                Lookup::Missing => resource.set_derived("_nodes", 1),
            }
        }
        "aws_elasticache_replication_group" => {
            derive_cache_engine(resource, Some("redis"));
            derive_replication_group_nodes(resource);
        }
        _ => {}
    }
}

fn derive_db_instance(resource: &mut Resource) {
    match resource.get_str("engine").map(str::to_string) {
        Some(engine) => match engine.as_str() {
            "postgres" => resource.set_derived("_engine", "PostgreSQL"),
            "mysql" => resource.set_derived("_engine", "MySQL"),
            "mariadb" => resource.set_derived("_engine", "MariaDB"),
            other => mark_unknown(
                resource,
                "_engine",
                format!("engine `{other}` is not modelled yet (supported: postgres, mysql, mariadb)"),
            ),
        },
        None => copy_unknown(resource, "engine", "_engine"),
    }

    let multi_az = match resource.get("multi_az") {
        Lookup::Unknown(reason) => {
            let reason = reason.to_string();
            mark_unknown(resource, "_deployment", reason.clone());
            mark_unknown(resource, "_iops_usagetype", reason);
            return;
        }
        Lookup::Known(Value::Bool(true)) => true,
        _ => false,
    };
    resource.set_derived("_deployment", if multi_az { "Multi-AZ" } else { "Single-AZ" });
    resource.set_derived(
        "_iops_usagetype",
        if multi_az { "RDS:Multi-AZ-PIOPS" } else { "RDS:PIOPS" },
    );

    let storage_type = match resource.get("storage_type") {
        Lookup::Unknown(reason) => {
            let reason = reason.to_string();
            return mark_unknown(resource, "_volume_type", reason);
        }
        Lookup::Known(Value::String(storage_type)) => storage_type.clone(),
        // The provider defaults to io1 when iops is set and gp2 otherwise.
        _ if matches!(resource.get("iops"), Lookup::Known(_)) => "io1".to_string(),
        _ => "gp2".to_string(),
    };
    match storage_type.as_str() {
        "gp2" => resource.set_derived("_volume_type", "General Purpose"),
        "gp3" => resource.set_derived("_volume_type", "General Purpose-GP3"),
        "io1" => resource.set_derived("_volume_type", "Provisioned IOPS"),
        "io2" => resource.set_derived("_volume_type", "Provisioned IOPS-IO2"),
        "standard" => resource.set_derived("_volume_type", "Magnetic"),
        other => mark_unknown(
            resource,
            "_volume_type",
            format!("storage type `{other}` is not modelled"),
        ),
    }
}

fn derive_ecs_service(resource: &mut Resource, all: &[Resource]) {
    let on_fargate_capacity = (0..8).any(|index| {
        resource
            .get_str(&format!("capacity_provider_strategy.{index}.capacity_provider"))
            .is_some_and(|provider| provider.starts_with("FARGATE"))
    });
    let launch_type = match (resource.get_str("launch_type"), on_fargate_capacity) {
        (Some(launch_type), _) => launch_type.to_string(),
        (None, true) => "FARGATE".to_string(),
        (None, false) => "EC2".to_string(),
    };
    resource.set_derived("_launch_type", launch_type.clone());
    if launch_type != "FARGATE" {
        return;
    }

    let Some(definition) = task_definition(resource, all) else {
        let reason = "its task definition is not part of this configuration, so task CPU and memory are not known";
        mark_unknown(resource, "_vcpu", reason.to_string());
        mark_unknown(resource, "_memory_gb", reason.to_string());
        return;
    };

    let per_1024 = Decimal::from(1024);
    for (source, target) in [("cpu", "_vcpu"), ("memory", "_memory_gb")] {
        match definition.get(source) {
            Lookup::Known(value) => match number(value) {
                Some(units) => resource.set_derived(target, (units / per_1024).normalize().to_string()),
                None => mark_unknown(
                    resource,
                    target,
                    format!("task definition {source} `{value}` is not a number"),
                ),
            },
            Lookup::Unknown(reason) => mark_unknown(resource, target, format!("task definition {source}: {reason}")),
            Lookup::Missing => mark_unknown(resource, target, format!("the task definition does not set {source}")),
        }
    }
}

fn task_definition<'a>(service: &Resource, all: &'a [Resource]) -> Option<&'a Resource> {
    let mut definitions = all.iter().filter(|r| r.resource_type == "aws_ecs_task_definition");

    if let Some(references) = service.refs.get("task_definition") {
        return definitions.find(|definition| references.iter().any(|r| r == definition.base_address()));
    }

    let literal = service.get_str("task_definition")?;
    definitions.find(|definition| {
        definition.get_str("family").is_some_and(|family| {
            literal == family
                || literal.starts_with(&format!("{family}:"))
                || literal.contains(&format!("task-definition/{family}"))
        })
    })
}

fn derive_cache_engine(resource: &mut Resource, default: Option<&str>) {
    let engine = match (resource.get_str("engine"), default) {
        (Some(engine), _) => engine.to_string(),
        (None, Some(default)) if resource.get("engine") == Lookup::Missing => default.to_string(),
        _ => return copy_unknown(resource, "engine", "_engine"),
    };
    match engine.as_str() {
        "redis" => resource.set_derived("_engine", "Redis"),
        "valkey" => resource.set_derived("_engine", "Valkey"),
        "memcached" => resource.set_derived("_engine", "Memcached"),
        other => mark_unknown(resource, "_engine", format!("cache engine `{other}` is not modelled")),
    }
}

fn derive_replication_group_nodes(resource: &mut Resource) {
    if let Lookup::Known(clusters) = resource.get("num_cache_clusters") {
        return resource.set_derived("_nodes", clusters.clone());
    }

    let read = |path: &str, default: i64| match resource.get(path) {
        Lookup::Known(value) => number(value),
        Lookup::Missing => Some(Decimal::from(default)),
        Lookup::Unknown(_) => None,
    };
    match (read("num_node_groups", 1), read("replicas_per_node_group", 0)) {
        (Some(groups), Some(replicas)) => {
            let nodes = groups * (replicas + Decimal::ONE);
            resource.set_derived("_nodes", nodes.normalize().to_string());
        }
        _ => mark_unknown(
            resource,
            "_nodes",
            "the node count is known only after apply".to_string(),
        ),
    }
}

fn copy_or_default(resource: &mut Resource, source: &str, target: &str, default: &str) {
    match resource.get(source) {
        Lookup::Known(value) => resource.set_derived(target, value.clone()),
        Lookup::Missing => resource.set_derived(target, default),
        Lookup::Unknown(reason) => mark_unknown(resource, target, reason.to_string()),
    }
}

/// Carries "unknown" from a source attribute to the derived one, keeping the reason.
/// A missing source is left alone so the engine reports the attribute as not set.
fn copy_unknown(resource: &mut Resource, source: &str, target: &str) {
    if let Lookup::Unknown(reason) = resource.get(source) {
        mark_unknown(resource, target, reason.to_string());
    }
}

fn mark_unknown(resource: &mut Resource, path: &str, reason: String) {
    resource.unknown.insert(path.to_string(), reason);
}

fn number(value: &Value) -> Option<Decimal> {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    Decimal::from_str(text.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Action, IacSource, ResourceChange};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn input(resources: Vec<Resource>) -> Input {
        Input {
            source: IacSource::TerraformStatic,
            origin: ".".into(),
            provider_regions: BTreeMap::new(),
            changes: resources
                .into_iter()
                .map(|resource| ResourceChange {
                    address: resource.address.clone(),
                    resource_type: resource.resource_type.clone(),
                    action: Action::Create,
                    before: None,
                    after: Some(resource),
                })
                .collect(),
            warnings: vec![],
        }
    }

    fn after(input: &Input, index: usize) -> &Resource {
        input.changes[index].after.as_ref().unwrap()
    }

    #[test]
    fn region_precedence_is_resource_then_provider_then_default() {
        let mut explicit = Resource::new("aws_instance.a", "aws_instance", json!({"region": "eu-west-1"}));
        explicit.region = None;
        let implicit = Resource::new("aws_instance.b", "aws_instance", json!({}));

        let mut with_provider = input(vec![explicit.clone(), implicit.clone()]);
        with_provider.provider_regions.insert("aws".into(), "ap-south-1".into());
        assert!(!normalise(&mut with_provider, FALLBACK_REGION));
        assert_eq!(after(&with_provider, 0).region.as_deref(), Some("eu-west-1"));
        assert_eq!(after(&with_provider, 1).region.as_deref(), Some("ap-south-1"));

        let mut without = input(vec![implicit]);
        assert!(normalise(&mut without, FALLBACK_REGION));
        assert_eq!(after(&without, 0).region.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn fargate_service_takes_cpu_and_memory_from_the_referenced_task_definition() {
        let mut service = Resource::new(
            "module.app.aws_ecs_service.api",
            "aws_ecs_service",
            json!({"launch_type": "FARGATE", "desired_count": 3}),
        );
        service
            .unknown
            .insert("task_definition".into(), "known only after apply".into());
        service.refs.insert(
            "task_definition".into(),
            vec!["module.app.aws_ecs_task_definition.api".into()],
        );
        let definition = Resource::new(
            "module.app.aws_ecs_task_definition.api",
            "aws_ecs_task_definition",
            json!({"family": "api", "cpu": "512", "memory": "1024"}),
        );
        let other = Resource::new(
            "aws_ecs_task_definition.worker",
            "aws_ecs_task_definition",
            json!({"family": "worker", "cpu": "4096", "memory": "8192"}),
        );

        let mut input = input(vec![service, other, definition]);
        normalise(&mut input, FALLBACK_REGION);

        assert_eq!(after(&input, 0).get_str("_vcpu"), Some("0.5"));
        assert_eq!(after(&input, 0).get_str("_memory_gb"), Some("1"));
    }

    #[test]
    fn fargate_service_without_a_task_definition_is_unknown_not_guessed() {
        let service = Resource::new(
            "aws_ecs_service.api",
            "aws_ecs_service",
            json!({"launch_type": "FARGATE"}),
        );
        let mut input = input(vec![service]);
        normalise(&mut input, FALLBACK_REGION);
        assert!(matches!(after(&input, 0).get("_vcpu"), Lookup::Unknown(_)));
    }

    #[test]
    fn rds_derivations() {
        let database = Resource::new(
            "aws_db_instance.main",
            "aws_db_instance",
            json!({"engine": "postgres", "instance_class": "db.t3.medium", "multi_az": true, "storage_type": "gp3"}),
        );
        let licensed = Resource::new("aws_db_instance.ora", "aws_db_instance", json!({"engine": "oracle-ee"}));
        let mut input = input(vec![database, licensed]);
        normalise(&mut input, FALLBACK_REGION);

        let database = after(&input, 0);
        assert_eq!(database.get_str("_engine"), Some("PostgreSQL"));
        assert_eq!(database.get_str("_deployment"), Some("Multi-AZ"));
        assert_eq!(database.get_str("_volume_type"), Some("General Purpose-GP3"));
        assert!(matches!(after(&input, 1).get("_engine"), Lookup::Unknown(reason) if reason.contains("oracle-ee")));
    }

    #[test]
    fn replication_group_node_count() {
        let explicit = Resource::new(
            "a.x",
            "aws_elasticache_replication_group",
            json!({"num_cache_clusters": 3}),
        );
        let sharded = Resource::new(
            "a.y",
            "aws_elasticache_replication_group",
            json!({"num_node_groups": 2, "replicas_per_node_group": 1}),
        );
        let mut input = input(vec![explicit, sharded]);
        normalise(&mut input, FALLBACK_REGION);
        assert_eq!(after(&input, 0).get("_nodes"), Lookup::Known(&json!(3)));
        assert_eq!(after(&input, 1).get_str("_nodes"), Some("4"));
        assert_eq!(after(&input, 0).get_str("_engine"), Some("Redis"));
    }
}
