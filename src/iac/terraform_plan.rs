//! Terraform plan JSON (`terraform show -json <planfile>`): the high-fidelity input.
//!
//! Follows the documented compatibility rules: unknown fields are ignored and an
//! unsupported major `format_version` is rejected. Sensitive values are dropped while
//! parsing so they can never reach output or logs.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::model::{Action, IacSource, Input, Resource, ResourceChange, strip_index};

const SUPPORTED_MAJOR: &str = "1";
const NOT_REFERENCES: &[&str] = &[
    "var",
    "local",
    "module",
    "data",
    "each",
    "count",
    "path",
    "terraform",
    "self",
];

#[derive(Deserialize)]
struct Plan {
    format_version: Option<String>,
    resource_changes: Option<Vec<PlannedChange>>,
    #[serde(default)]
    configuration: Value,
    #[serde(default)]
    errored: bool,
}

#[derive(Deserialize)]
struct PlannedChange {
    address: String,
    #[serde(default)]
    mode: String,
    #[serde(rename = "type")]
    resource_type: String,
    change: Change,
}

#[derive(Deserialize)]
struct Change {
    actions: Vec<String>,
    #[serde(default)]
    before: Value,
    #[serde(default)]
    after: Value,
    #[serde(default)]
    after_unknown: Value,
    #[serde(default)]
    before_sensitive: Value,
    #[serde(default)]
    after_sensitive: Value,
}

/// Cheap structural check used by discovery to tell a plan from any other JSON file.
pub fn looks_like_plan(json: &str) -> bool {
    serde_json::from_str::<Plan>(json)
        .is_ok_and(|plan| plan.format_version.is_some() && plan.resource_changes.is_some())
}

pub fn parse(json: &str, origin: &str) -> Result<Input> {
    let plan: Plan = serde_json::from_str(json).context("not valid Terraform plan JSON")?;

    let Some(version) = &plan.format_version else {
        bail!("no `format_version` field. Produce the file with `terraform show -json <planfile>`.");
    };
    let major = version.split('.').next().unwrap_or_default();
    if major != SUPPORTED_MAJOR {
        bail!("plan format version {version} is not supported (supported major version: {SUPPORTED_MAJOR})");
    }

    let mut warnings = Vec::new();
    if plan.errored {
        warnings.push("Terraform reported that planning failed; the plan may be incomplete.".to_string());
    }

    let references = config_references(&plan.configuration);
    let mut changes = Vec::new();
    for planned in plan.resource_changes.unwrap_or_default() {
        if planned.mode != "managed" {
            continue;
        }
        let Some(action) = action(&planned.change.actions) else {
            // `read` (data sources resolved at apply) and anything newer create no billable change.
            continue;
        };

        let references = references.get(config_address(&planned.address).as_str());
        let build = |values: Value, sensitive: &Value, unknown: Option<&Value>| {
            let mut resource = Resource::new(&planned.address, &planned.resource_type, values);
            redact(&mut resource.attrs, sensitive, "", &mut resource.unknown);
            if let Some(unknown) = unknown {
                mark_unknown(unknown, "", &mut resource.unknown);
            }
            if let Some(references) = references {
                resource.refs = absolute_references(&planned.address, references);
            }
            resource
        };

        let change = planned.change;
        let before = (!change.before.is_null()).then(|| build(change.before, &change.before_sensitive, None));
        let after = (!change.after.is_null())
            .then(|| build(change.after, &change.after_sensitive, Some(&change.after_unknown)));

        changes.push(ResourceChange {
            address: planned.address.clone(),
            resource_type: planned.resource_type.clone(),
            action,
            before,
            after,
        });
    }

    Ok(Input {
        source: IacSource::TerraformPlan,
        origin: origin.to_string(),
        provider_regions: provider_regions(&plan.configuration),
        changes,
        warnings,
    })
}

fn action(actions: &[String]) -> Option<Action> {
    let actions: Vec<&str> = actions.iter().map(String::as_str).collect();
    match actions.as_slice() {
        ["create"] => Some(Action::Create),
        ["update"] => Some(Action::Update),
        ["delete"] => Some(Action::Delete),
        ["delete", "create"] | ["create", "delete"] => Some(Action::Replace),
        ["no-op"] => Some(Action::NoOp),
        _ => None,
    }
}

/// Replaces every value flagged in the sensitivity mask with null and records why.
fn redact(value: &mut Value, mask: &Value, path: &str, unknown: &mut BTreeMap<String, String>) {
    match (value, mask) {
        (value, Value::Bool(true)) => {
            *value = Value::Null;
            if !path.is_empty() {
                unknown.insert(path.to_string(), "sensitive value, redacted".to_string());
            }
        }
        (Value::Object(values), Value::Object(masks)) => {
            for (key, mask) in masks {
                if let Some(value) = values.get_mut(key) {
                    redact(value, mask, &join(path, key), unknown);
                }
            }
        }
        (Value::Array(values), Value::Array(masks)) => {
            for (index, (value, mask)) in values.iter_mut().zip(masks).enumerate() {
                redact(value, mask, &join(path, &index.to_string()), unknown);
            }
        }
        _ => {}
    }
}

fn mark_unknown(mask: &Value, path: &str, unknown: &mut BTreeMap<String, String>) {
    match mask {
        Value::Bool(true) if !path.is_empty() => {
            unknown
                .entry(path.to_string())
                .or_insert_with(|| "known only after apply".to_string());
        }
        Value::Object(masks) => {
            for (key, mask) in masks {
                mark_unknown(mask, &join(path, key), unknown);
            }
        }
        Value::Array(masks) => {
            for (index, mask) in masks.iter().enumerate() {
                mark_unknown(mask, &join(path, &index.to_string()), unknown);
            }
        }
        _ => {}
    }
}

fn join(path: &str, segment: &str) -> String {
    if path.is_empty() {
        return segment.to_string();
    }
    format!("{path}.{segment}")
}

fn provider_regions(configuration: &Value) -> BTreeMap<String, String> {
    let mut regions = BTreeMap::new();
    let Some(providers) = configuration.get("provider_config").and_then(Value::as_object) else {
        return regions;
    };
    for provider in providers.values() {
        // Aliased providers are secondary; the default configuration decides the region.
        if provider.get("alias").is_some() {
            continue;
        }
        let name = provider.get("name").and_then(Value::as_str);
        let region = provider
            .pointer("/expressions/region/constant_value")
            .and_then(Value::as_str);
        if let (Some(name), Some(region)) = (name, region) {
            regions.insert(name.to_string(), region.to_string());
        }
    }
    regions
}

/// Address as it appears in `configuration`: module and resource instance keys removed.
fn config_address(address: &str) -> String {
    let mut output = String::new();
    let mut depth = 0;
    for character in address.chars() {
        match character {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth == 0 => output.push(character),
            _ => {}
        }
    }
    output
}

/// Maps each configured resource to the resources its top-level arguments reference.
fn config_references(configuration: &Value) -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    let mut output = BTreeMap::new();
    if let Some(root) = configuration.get("root_module") {
        collect_module(root, "", 0, &mut output);
    }
    output
}

fn collect_module(
    module: &Value,
    prefix: &str,
    depth: usize,
    output: &mut BTreeMap<String, BTreeMap<String, Vec<String>>>,
) {
    // Terraform itself rejects module cycles; the cap guards against a hand-crafted file.
    if depth > 32 {
        return;
    }

    for resource in module.get("resources").and_then(Value::as_array).into_iter().flatten() {
        let (Some(address), Some(expressions)) = (
            resource.get("address").and_then(Value::as_str),
            resource.get("expressions").and_then(Value::as_object),
        ) else {
            continue;
        };
        let mut by_argument = BTreeMap::new();
        for (argument, expression) in expressions {
            let mut found = Vec::new();
            collect_references(expression, &mut found);
            if !found.is_empty() {
                found.sort();
                found.dedup();
                by_argument.insert(argument.clone(), found);
            }
        }
        output.insert(format!("{prefix}{address}"), by_argument);
    }

    for (name, call) in module
        .get("module_calls")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if let Some(child) = call.get("module") {
            collect_module(child, &format!("{prefix}module.{name}."), depth + 1, output);
        }
    }
}

fn collect_references(expression: &Value, found: &mut Vec<String>) {
    match expression {
        Value::Object(map) => {
            for reference in map.get("references").and_then(Value::as_array).into_iter().flatten() {
                let mut parts = reference.as_str().unwrap_or_default().split('.');
                if let (Some(kind), Some(name)) = (parts.next(), parts.next())
                    && !NOT_REFERENCES.contains(&kind)
                {
                    found.push(format!("{kind}.{name}"));
                }
            }
            // Nested blocks carry their own expression objects.
            for (key, nested) in map {
                if key != "references" {
                    collect_references(nested, found);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_references(item, found)),
        _ => {}
    }
}

/// References are relative to the module instance that contains the referencing resource.
fn absolute_references(address: &str, references: &BTreeMap<String, Vec<String>>) -> BTreeMap<String, Vec<String>> {
    let base = strip_index(address);
    let module_prefix = match base.rmatch_indices('.').nth(1) {
        Some((index, _)) => &base[..=index],
        None => "",
    };
    references
        .iter()
        .map(|(argument, targets)| {
            let targets = targets
                .iter()
                .map(|target| format!("{module_prefix}{target}"))
                .collect();
            (argument.clone(), targets)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Lookup;
    use serde_json::json;

    fn plan(resource_changes: Value) -> String {
        json!({"format_version": "1.2", "terraform_version": "1.9.0", "resource_changes": resource_changes}).to_string()
    }

    fn change(address: &str, actions: &[&str], before: Value, after: Value) -> Value {
        json!({
            "address": address, "mode": "managed", "type": "aws_instance", "name": "web",
            "change": {"actions": actions, "before": before, "after": after, "after_unknown": {}}
        })
    }

    #[test]
    fn every_action_maps_to_the_right_sides() {
        let size = |instance_type: &str| json!({"instance_type": instance_type});
        let input = parse(
            &plan(json!([
                change("aws_instance.create", &["create"], Value::Null, size("t3.micro")),
                change("aws_instance.update", &["update"], size("t3.micro"), size("t3.large")),
                change("aws_instance.delete", &["delete"], size("t3.micro"), Value::Null),
                change(
                    "aws_instance.replace",
                    &["delete", "create"],
                    size("t3.micro"),
                    size("t3.small")
                ),
                change(
                    "aws_instance.cbd",
                    &["create", "delete"],
                    size("t3.micro"),
                    size("t3.small")
                ),
                change("aws_instance.noop", &["no-op"], size("t3.micro"), size("t3.micro")),
                change("aws_instance.read", &["read"], Value::Null, size("t3.micro")),
            ])),
            "plan.json",
        )
        .unwrap();

        let summary: Vec<_> = input
            .changes
            .iter()
            .map(|c| (c.address.as_str(), c.action, c.before.is_some(), c.after.is_some()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("aws_instance.create", Action::Create, false, true),
                ("aws_instance.update", Action::Update, true, true),
                ("aws_instance.delete", Action::Delete, true, false),
                ("aws_instance.replace", Action::Replace, true, true),
                ("aws_instance.cbd", Action::Replace, true, true),
                ("aws_instance.noop", Action::NoOp, true, true),
            ]
        );
    }

    #[test]
    fn data_sources_are_skipped_and_instance_addresses_are_kept_verbatim() {
        let mut data = change("data.aws_ami.x", &["read"], Value::Null, json!({}));
        data["mode"] = json!("data");
        let input = parse(
            &plan(json!([
                data,
                change("module.foo.aws_instance.web[0]", &["create"], Value::Null, json!({})),
                change(
                    "module.foo[\"a.b\"].aws_instance.web[\"x\"]",
                    &["create"],
                    Value::Null,
                    json!({})
                ),
            ])),
            "p",
        )
        .unwrap();
        let addresses: Vec<_> = input.changes.iter().map(|c| c.address.as_str()).collect();
        assert_eq!(
            addresses,
            vec![
                "module.foo.aws_instance.web[0]",
                "module.foo[\"a.b\"].aws_instance.web[\"x\"]"
            ]
        );
    }

    #[test]
    fn unknown_null_and_sensitive_values_are_distinguished_and_secrets_never_survive() {
        let mut planned = change(
            "aws_instance.web",
            &["create"],
            Value::Null,
            json!({"instance_type": "t3.micro", "user_data": "#!/bin/sh\nexport TOKEN=hunter2", "ami": null,
                   "root_block_device": [{"volume_size": 50, "kms_key_id": "secret-key"}]}),
        );
        planned["change"]["after_unknown"] =
            json!({"id": true, "ebs_block_device": true, "root_block_device": [{"iops": true}]});
        planned["change"]["after_sensitive"] = json!({"user_data": true, "root_block_device": [{"kms_key_id": true}]});

        let input = parse(&plan(json!([planned])), "p").unwrap();
        let resource = input.changes[0].after.as_ref().unwrap();

        assert_eq!(resource.get("instance_type"), Lookup::Known(&json!("t3.micro")));
        assert_eq!(resource.get("ami"), Lookup::Missing);
        assert_eq!(resource.get("id"), Lookup::Unknown("known only after apply"));
        assert_eq!(
            resource.get("ebs_block_device.0.volume_size"),
            Lookup::Unknown("known only after apply")
        );
        assert_eq!(
            resource.get("root_block_device.0.iops"),
            Lookup::Unknown("known only after apply")
        );
        assert_eq!(
            resource.get("root_block_device.0.volume_size"),
            Lookup::Known(&json!(50))
        );
        assert_eq!(resource.get("user_data"), Lookup::Unknown("sensitive value, redacted"));

        let debug = format!("{resource:?}");
        assert!(!debug.contains("hunter2") && !debug.contains("secret-key"), "{debug}");
    }

    #[test]
    fn unsupported_major_version_is_rejected_and_newer_minor_fields_are_ignored() {
        let future = json!({"format_version": "2.0", "resource_changes": []}).to_string();
        let error = parse(&future, "p").unwrap_err().to_string();
        assert!(error.contains("2.0") && error.contains("not supported"), "{error}");

        let newer_minor = json!({"format_version": "1.99", "brand_new_field": {"x": 1}, "resource_changes": [
            {"address": "aws_instance.a", "mode": "managed", "type": "aws_instance", "name": "a", "future": true,
             "change": {"actions": ["create"], "before": null, "after": {}, "also_new": 1}}
        ]})
        .to_string();
        assert_eq!(parse(&newer_minor, "p").unwrap().changes.len(), 1);
    }

    #[test]
    fn malformed_input_is_an_error_and_other_json_is_not_a_plan() {
        assert!(parse("{", "p").is_err());
        assert!(parse("{}", "p").unwrap_err().to_string().contains("format_version"));
        assert!(!looks_like_plan("{\"name\": \"package\"}"));
        assert!(!looks_like_plan("not json"));
        assert!(looks_like_plan(&plan(json!([]))));
    }

    #[test]
    fn provider_region_and_module_relative_references_come_from_configuration() {
        let mut document: Value = serde_json::from_str(&plan(json!([{
            "address": "module.app[0].aws_ecs_service.api", "mode": "managed", "type": "aws_ecs_service", "name": "api",
            "change": {"actions": ["create"], "before": null, "after": {}, "after_unknown": {"task_definition": true}}
        }])))
        .unwrap();
        document["configuration"] = json!({
            "provider_config": {
                "aws": {"name": "aws", "expressions": {"region": {"constant_value": "eu-central-1"}}},
                "aws.replica": {"name": "aws", "alias": "replica", "expressions": {"region": {"constant_value": "us-west-2"}}}
            },
            "root_module": {"module_calls": {"app": {"module": {"resources": [{
                "address": "aws_ecs_service.api",
                "expressions": {"task_definition": {"references": ["aws_ecs_task_definition.api.arn", "aws_ecs_task_definition.api", "var.name"]}}
            }]}}}}
        });

        let input = parse(&document.to_string(), "p").unwrap();

        assert_eq!(input.provider_regions["aws"], "eu-central-1");
        assert_eq!(
            input.changes[0].after.as_ref().unwrap().refs["task_definition"],
            vec!["module.app[0].aws_ecs_task_definition.api"]
        );
    }
}
