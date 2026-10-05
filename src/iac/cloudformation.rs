//! CloudFormation templates (YAML or JSON). Nothing is deployed or executed.
//!
//! Parameters take their defaults, then `Ref`, `Fn::Sub`, `Fn::Join`, `Fn::Select`,
//! `Fn::Split`, `Fn::FindInMap`, `Fn::If` and the condition functions are evaluated.
//! Anything that exists only after deployment (`Fn::GetAtt`, a `Ref` to a resource,
//! imports, most pseudo parameters) is recorded as unknown with a reason.
//!
//! Resource types that have a price mapping are translated to the attribute names the
//! mappings use, so a template and the equivalent Terraform are priced by the same data.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use serde_saphyr::Tagged;

use crate::discovery::read_limited;
use crate::model::{Action, IacSource, Input, Resource, ResourceChange};

const MAX_NESTED_STACKS: usize = 10;
const MAX_EVAL_DEPTH: usize = 64;
const CUSTOM_RESOURCE: &str = "AWS::CloudFormation::CustomResource";
const NESTED_STACK: &str = "AWS::CloudFormation::Stack";

/// CloudFormation type, the mapped type it is priced as, and (mapped attribute,
/// property path) pairs. Keys below a copied property are converted to snake case.
type Translation = (&'static str, &'static str, &'static [(&'static str, &'static str)]);

const TRANSLATIONS: &[Translation] = &[
    (
        "AWS::EC2::Instance",
        "aws_instance",
        &[
            ("instance_type", "InstanceType"),
            ("tenancy", "Tenancy"),
            ("root_block_device", "BlockDeviceMappings.0.Ebs"),
        ],
    ),
    (
        "AWS::EC2::Volume",
        "aws_ebs_volume",
        &[("type", "VolumeType"), ("size", "Size"), ("iops", "Iops")],
    ),
    (
        "AWS::RDS::DBInstance",
        "aws_db_instance",
        &[
            ("instance_class", "DBInstanceClass"),
            ("engine", "Engine"),
            ("multi_az", "MultiAZ"),
            ("storage_type", "StorageType"),
            ("iops", "Iops"),
            ("allocated_storage", "AllocatedStorage"),
        ],
    ),
    (
        "AWS::ElasticLoadBalancingV2::LoadBalancer",
        "aws_lb",
        &[("load_balancer_type", "Type")],
    ),
    ("AWS::EC2::NatGateway", "aws_nat_gateway", &[]),
    (
        "AWS::ECS::Service",
        "aws_ecs_service",
        &[
            ("launch_type", "LaunchType"),
            ("desired_count", "DesiredCount"),
            ("capacity_provider_strategy", "CapacityProviderStrategy"),
            ("task_definition", "TaskDefinition"),
        ],
    ),
    (
        "AWS::ECS::TaskDefinition",
        "aws_ecs_task_definition",
        &[("family", "Family"), ("cpu", "Cpu"), ("memory", "Memory")],
    ),
    (
        "AWS::Lambda::Function",
        "aws_lambda_function",
        &[("architectures", "Architectures"), ("memory_size", "MemorySize")],
    ),
    (
        "AWS::S3::Bucket",
        "aws_s3_bucket",
        &[
            ("bucket", "BucketName"),
            ("lifecycle_rule", "LifecycleConfiguration.Rules"),
        ],
    ),
    ("AWS::CloudFront::Distribution", "aws_cloudfront_distribution", &[]),
    (
        "AWS::DynamoDB::Table",
        "aws_dynamodb_table",
        &[
            ("billing_mode", "BillingMode"),
            ("read_capacity", "ProvisionedThroughput.ReadCapacityUnits"),
            ("write_capacity", "ProvisionedThroughput.WriteCapacityUnits"),
        ],
    ),
    (
        "AWS::ElastiCache::CacheCluster",
        "aws_elasticache_cluster",
        &[
            ("engine", "Engine"),
            ("node_type", "CacheNodeType"),
            ("num_cache_nodes", "NumCacheNodes"),
        ],
    ),
    (
        "AWS::ElastiCache::ReplicationGroup",
        "aws_elasticache_replication_group",
        &[
            ("engine", "Engine"),
            ("node_type", "CacheNodeType"),
            ("num_cache_clusters", "NumCacheClusters"),
            ("num_node_groups", "NumNodeGroups"),
            ("replicas_per_node_group", "ReplicasPerNodeGroup"),
        ],
    ),
];

#[derive(Clone, Copy)]
pub struct Options<'a> {
    /// Templates, nested ones included, must resolve inside this directory.
    pub scan_root: &'a Path,
    pub max_file_bytes: u64,
    /// Value of `AWS::Region`, when the scan was told which region to assume.
    pub region: Option<&'a str>,
}

pub struct Parsed {
    pub input: Input,
    /// Canonical paths of the nested templates that were read as part of this one.
    pub nested: Vec<PathBuf>,
}

/// CloudFormation types that are priced, with the mapped type each is priced as.
pub fn priced_types() -> impl Iterator<Item = (&'static str, &'static str)> {
    TRANSLATIONS
        .iter()
        .map(|(cloudformation, mapped, _)| (*cloudformation, *mapped))
}

/// Reads one template and the local nested templates it references. `prefix` is put in
/// front of every logical ID, so that stacks sharing an ID stay distinguishable.
pub fn parse_file(path: &Path, origin: &str, prefix: &str, source: IacSource, options: &Options) -> Result<Parsed> {
    let mut walk = Walk {
        options,
        changes: Vec::new(),
        warnings: Vec::new(),
        nested: Vec::new(),
    };
    walk.stack(path, prefix, &BTreeMap::new(), 0)?;

    Ok(Parsed {
        input: Input {
            source,
            origin: origin.to_string(),
            provider_regions: BTreeMap::new(),
            changes: walk.changes,
            warnings: walk.warnings,
        },
        nested: walk.nested,
    })
}

/// Parses template text. YAML short forms (`!Ref x`) become the long form (`{"Ref": "x"}`).
pub fn parse_text(text: &str, is_json: bool) -> Result<Value> {
    let template: Value = if is_json {
        serde_json::from_str(text).context("not valid JSON")?
    } else {
        serde_saphyr::from_str::<Node>(text).context("not valid YAML")?.0
    };
    if !template.get("Resources").is_some_and(Value::is_object) {
        bail!("not a CloudFormation template: there is no `Resources` section");
    }
    Ok(template)
}

struct Walk<'a> {
    options: &'a Options<'a>,
    changes: Vec<ResourceChange>,
    warnings: Vec<String>,
    nested: Vec<PathBuf>,
}

/// A parameter value, or the reason it is not known.
type Parameters = BTreeMap<String, Result<Value, String>>;

impl Walk<'_> {
    fn stack(&mut self, path: &Path, prefix: &str, overrides: &Parameters, depth: usize) -> Result<()> {
        let path = self
            .inside_scan_root(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let text = read_limited(&path, self.options.max_file_bytes)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let is_json = path.extension().is_some_and(|extension| extension == "json");
        let template = parse_text(&text, is_json).with_context(|| format!("{} is", path.display()))?;

        let scope = Scope {
            parameters: parameters(&template, overrides),
            mappings: &template["Mappings"],
            conditions: &template["Conditions"],
            region: self.options.region,
        };

        for (logical_id, definition) in template["Resources"].as_object().into_iter().flatten() {
            let address = format!("{prefix}{logical_id}");
            let Some(cloudformation_type) = definition["Type"].as_str() else {
                self.warnings.push(format!("{address} has no `Type`; skipped."));
                continue;
            };

            let mut notes = Vec::new();
            if let Some(condition) = definition["Condition"].as_str() {
                match scope.condition(condition, 0) {
                    Ok(false) => continue,
                    Ok(true) => {}
                    Err(reason) => notes.push(format!(
                        "condition `{condition}` could not be evaluated ({reason}); the resource is assumed to be created."
                    )),
                }
            }

            let empty = Value::Object(Map::new());
            let raw = definition
                .get("Properties")
                .filter(|value| value.is_object())
                .unwrap_or(&empty);
            let mut unknown = BTreeMap::new();
            let properties = scope.resolve(raw, "", &mut unknown);

            if cloudformation_type == NESTED_STACK {
                let local = definition["Metadata"]["aws:asset:path"]
                    .as_str()
                    .or_else(|| properties["TemplateURL"].as_str().filter(|url| !url.contains("://")))
                    .and_then(|file| self.inside_scan_root(&path.with_file_name(file)).ok());
                match local {
                    Some(_) if depth >= MAX_NESTED_STACKS => self.warnings.push(format!(
                        "nested stack {address} is deeper than {MAX_NESTED_STACKS} levels; its resources are not included."
                    )),
                    Some(child) => {
                        let passed = raw["Parameters"]
                            .as_object()
                            .into_iter()
                            .flatten()
                            .map(|(name, value)| (name.clone(), scope.eval(value, 0)))
                            .collect();
                        self.stack(&child, &format!("{address}."), &passed, depth + 1)?;
                        self.nested.push(child);
                        continue;
                    }
                    None => self.warnings.push(format!(
                        "nested stack {address}: its template is not a local file, so its resources are not included."
                    )),
                }
            }

            let mut resource = translate(cloudformation_type, &address, raw, properties, unknown, &scope, prefix);
            resource.notes.extend(notes);
            self.changes.push(ResourceChange {
                address,
                resource_type: resource.resource_type.clone(),
                action: Action::Create,
                before: None,
                after: Some(resource),
            });
        }
        Ok(())
    }

    /// Canonicalising resolves `..` and symlinks, so the check cannot be sidestepped.
    fn inside_scan_root(&self, path: &Path) -> Result<PathBuf> {
        let root = self.options.scan_root.canonicalize()?;
        let path = path.canonicalize()?;
        if !path.starts_with(&root) {
            bail!("it is outside the scanned directory");
        }
        Ok(path)
    }
}

fn parameters(template: &Value, overrides: &Parameters) -> Parameters {
    let mut parameters = Parameters::new();
    for (name, definition) in template["Parameters"].as_object().into_iter().flatten() {
        let kind = definition["Type"].as_str().unwrap_or("String");
        let value = match (overrides.get(name), definition.get("Default")) {
            (Some(passed), _) => passed.clone(),
            (None, _) if kind.starts_with("AWS::SSM::Parameter::") => {
                Err(format!("parameter `{name}` is read from SSM at deploy time"))
            }
            (None, Some(default)) => Ok(default.clone()),
            (None, None) => Err(format!("parameter `{name}` has no default value")),
        };
        let is_list = kind == "CommaDelimitedList" || kind.starts_with("List<");
        let value = value.map(|value| match value {
            Value::String(text) if is_list => text.split(',').map(|item| Value::from(item.trim())).collect(),
            other => other,
        });
        parameters.insert(name.clone(), value);
    }
    parameters
}

fn translate(
    cloudformation_type: &str,
    address: &str,
    raw: &Value,
    properties: Value,
    unknown: BTreeMap<String, String>,
    scope: &Scope,
    prefix: &str,
) -> Resource {
    let Some((_, mapped_type, pairs)) = TRANSLATIONS.iter().find(|(name, ..)| *name == cloudformation_type) else {
        // Custom resources are backed by a Lambda function or SNS topic declared elsewhere.
        let shown = if cloudformation_type.starts_with("Custom::") {
            CUSTOM_RESOURCE
        } else {
            cloudformation_type
        };
        let mut resource = Resource::new(address, shown, properties);
        resource.provider = "aws".to_string();
        resource.unknown = unknown;
        return resource;
    };

    let mut attrs = Map::new();
    let mut resource = Resource::new(address, mapped_type, Value::Null);
    for (key, path) in *pairs {
        if let Some(value) = at(&properties, path).filter(|value| !value.is_null()) {
            attrs.insert((*key).to_string(), snake_keys(value));
        }
        for (unknown_path, reason) in &unknown {
            if let Some(rest) = below(unknown_path, path) {
                resource.unknown.insert(format!("{key}{}", snake(rest)), reason.clone());
            } else if below(path, unknown_path).is_some() {
                resource.unknown.insert((*key).to_string(), reason.clone());
            }
        }

        let mut referenced = BTreeSet::new();
        if let Some(value) = at(raw, path) {
            scope.references(value, &mut referenced);
        }
        if !referenced.is_empty() {
            let targets = referenced.into_iter().map(|id| format!("{prefix}{id}")).collect();
            resource.refs.insert((*key).to_string(), targets);
        }
    }

    // CloudFormation runs one task when the count is left out.
    if mapped_type == &"aws_ecs_service" && at(raw, "DesiredCount").is_none() {
        attrs.insert("desired_count".to_string(), Value::from(1));
    }
    if at(&properties, "BlockDeviceMappings.1").is_some() {
        resource
            .notes
            .push("Only the first block device mapping is priced.".to_string());
    }

    resource.attrs = Value::Object(attrs);
    resource
}

struct Scope<'a> {
    parameters: Parameters,
    mappings: &'a Value,
    conditions: &'a Value,
    region: Option<&'a str>,
}

impl Scope<'_> {
    /// Resolves a property tree. A value that cannot be known becomes null and its
    /// dotted path is recorded with the reason.
    fn resolve(&self, value: &Value, path: &str, unknown: &mut BTreeMap<String, String>) -> Value {
        let join = |key: &str| {
            if path.is_empty() {
                key.to_string()
            } else {
                format!("{path}.{key}")
            }
        };
        match value {
            Value::Object(map) => match intrinsic(map) {
                // The chosen branch is resolved in place, so an unknown inside it stays local.
                Some(("Fn::If", argument)) => match self.branch(argument, 0) {
                    Ok(branch) => self.resolve(branch, path, unknown),
                    Err(reason) => {
                        unknown.insert(path.to_string(), reason);
                        Value::Null
                    }
                },
                Some(_) => self.eval(value, 0).unwrap_or_else(|reason| {
                    unknown.insert(path.to_string(), reason);
                    Value::Null
                }),
                None => map
                    .iter()
                    .map(|(key, value)| (key.clone(), self.resolve(value, &join(key), unknown)))
                    .collect(),
            },
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(index, item)| self.resolve(item, &join(&index.to_string()), unknown))
                .collect(),
            scalar => scalar.clone(),
        }
    }

    /// Evaluates a value completely, or says why it cannot be known.
    fn eval(&self, value: &Value, depth: usize) -> Result<Value, String> {
        if depth > MAX_EVAL_DEPTH {
            return Err("the expression is nested too deeply or refers to itself".to_string());
        }
        match value {
            Value::Array(items) => items.iter().map(|item| self.eval(item, depth + 1)).collect(),
            Value::Object(map) => match intrinsic(map) {
                Some((name, argument)) => self.call(name, argument, depth + 1),
                None => map
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), self.eval(value, depth + 1)?)))
                    .collect::<Result<Map<_, _>, String>>()
                    .map(Value::Object),
            },
            scalar => Ok(scalar.clone()),
        }
    }

    fn call(&self, name: &str, argument: &Value, depth: usize) -> Result<Value, String> {
        let arguments = || {
            argument
                .as_array()
                .map(Vec::as_slice)
                .ok_or_else(|| format!("`{name}` needs a list of arguments"))
        };
        let malformed = || format!("`{name}` has the wrong number of arguments");

        match name {
            "Ref" => self.reference(argument.as_str().unwrap_or_default()),
            "Condition" => self
                .condition(argument.as_str().unwrap_or_default(), depth)
                .map(Value::Bool),
            "Fn::Sub" => self.substitute(argument, depth),
            "Fn::If" => self.eval(self.branch(argument, depth)?, depth),
            "Fn::Join" => {
                let [delimiter, items] = arguments()? else {
                    return Err(malformed());
                };
                let delimiter = text(&self.eval(delimiter, depth)?)?;
                let Value::Array(items) = self.eval(items, depth)? else {
                    return Err("`Fn::Join` needs a list to join".to_string());
                };
                let items = items.iter().map(text).collect::<Result<Vec<_>, _>>()?;
                Ok(Value::String(items.join(&delimiter)))
            }
            "Fn::Split" => {
                let [delimiter, source] = arguments()? else {
                    return Err(malformed());
                };
                let delimiter = text(&self.eval(delimiter, depth)?)?;
                let source = text(&self.eval(source, depth)?)?;
                Ok(source.split(delimiter.as_str()).map(Value::from).collect())
            }
            "Fn::Select" => {
                let [index, items] = arguments()? else {
                    return Err(malformed());
                };
                let index = text(&self.eval(index, depth)?)?;
                let items = self.eval(items, depth)?;
                index
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| items.get(index).cloned())
                    .ok_or_else(|| format!("`Fn::Select` index {index} is not in the list"))
            }
            "Fn::FindInMap" => {
                let [map, top, second, ..] = arguments()? else {
                    return Err(malformed());
                };
                let mut found = self.mappings;
                let mut keys = Vec::new();
                for key in [map, top, second] {
                    keys.push(text(&self.eval(key, depth)?)?);
                    found = &found[keys[keys.len() - 1].as_str()];
                }
                if found.is_null() {
                    return Err(format!("`Fn::FindInMap` found no entry for {}", keys.join(".")));
                }
                Ok(found.clone())
            }
            "Fn::Equals" => {
                let [left, right] = arguments()? else {
                    return Err(malformed());
                };
                Ok(Value::Bool(
                    text(&self.eval(left, depth)?)? == text(&self.eval(right, depth)?)?,
                ))
            }
            "Fn::Not" => {
                let [condition] = arguments()? else {
                    return Err(malformed());
                };
                Ok(Value::Bool(!self.truth(condition, depth)?))
            }
            "Fn::And" | "Fn::Or" => {
                let mut results = Vec::new();
                for condition in arguments()? {
                    results.push(self.truth(condition, depth)?);
                }
                Ok(Value::Bool(if name == "Fn::And" {
                    results.iter().all(|result| *result)
                } else {
                    results.iter().any(|result| *result)
                }))
            }
            "Fn::GetAtt" => Err("an attribute of another resource; known only after deployment".to_string()),
            "Fn::ImportValue" => Err("imported from another stack".to_string()),
            other => Err(format!("`{other}` is not evaluated statically")),
        }
    }

    fn reference(&self, name: &str) -> Result<Value, String> {
        match name {
            "AWS::NoValue" => Ok(Value::Null),
            "AWS::Region" => self
                .region
                .map(Value::from)
                .ok_or_else(|| "`AWS::Region` is set at deploy time".to_string()),
            _ if name.starts_with("AWS::") => Err(format!("`{name}` is set at deploy time")),
            _ => match self.parameters.get(name) {
                Some(value) => value.clone(),
                None => Err(format!("refers to resource `{name}`; known only after deployment")),
            },
        }
    }

    fn condition(&self, name: &str, depth: usize) -> Result<bool, String> {
        match self.conditions.get(name) {
            Some(condition) => self.truth(condition, depth + 1),
            None => Err(format!("condition `{name}` is not defined")),
        }
    }

    fn truth(&self, condition: &Value, depth: usize) -> Result<bool, String> {
        match self.eval(condition, depth)? {
            Value::Bool(result) => Ok(result),
            Value::String(text) if text == "true" || text == "false" => Ok(text == "true"),
            other => Err(format!("`{other}` is not a condition")),
        }
    }

    /// The branch of an `Fn::If` that applies, not yet evaluated.
    fn branch<'a>(&self, argument: &'a Value, depth: usize) -> Result<&'a Value, String> {
        let Some([Value::String(condition), when_true, when_false]) = argument.as_array().map(Vec::as_slice) else {
            return Err("`Fn::If` needs a condition name and two values".to_string());
        };
        Ok(if self.condition(condition, depth)? {
            when_true
        } else {
            when_false
        })
    }

    fn substitute(&self, argument: &Value, depth: usize) -> Result<Value, String> {
        let (template, variables) = match argument {
            Value::String(template) => (template.as_str(), None),
            Value::Array(parts) => match parts.as_slice() {
                [Value::String(template), Value::Object(variables)] => (template.as_str(), Some(variables)),
                _ => return Err("`Fn::Sub` needs a string and a map of variables".to_string()),
            },
            _ => return Err("`Fn::Sub` needs a string".to_string()),
        };

        let mut output = String::new();
        let mut rest = template;
        while let Some(start) = rest.find("${") {
            let Some(length) = rest[start..].find('}') else {
                break;
            };
            output.push_str(&rest[..start]);
            let name = rest[start + 2..start + length].trim();
            rest = &rest[start + length + 1..];

            if let Some(literal) = name.strip_prefix('!') {
                output.push_str(&format!("${{{literal}}}"));
            } else if let Some(value) = variables.and_then(|variables| variables.get(name)) {
                output.push_str(&text(&self.eval(value, depth)?)?);
            } else if name.contains('.') {
                return Err("an attribute of another resource; known only after deployment".to_string());
            } else {
                output.push_str(&text(&self.reference(name)?)?);
            }
        }
        output.push_str(rest);
        Ok(Value::String(output))
    }

    /// Logical IDs of the resources a property refers to through `Ref` or `Fn::GetAtt`.
    fn references(&self, value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                let target = match intrinsic(map) {
                    Some(("Ref", Value::String(name))) => Some(name.as_str()),
                    Some(("Fn::GetAtt", Value::String(path))) => path.split('.').next(),
                    Some(("Fn::GetAtt", Value::Array(parts))) => parts.first().and_then(Value::as_str),
                    _ => None,
                };
                match target {
                    Some(name) if !name.starts_with("AWS::") && !self.parameters.contains_key(name) => {
                        found.insert(name.to_string());
                    }
                    Some(_) => {}
                    None => map.values().for_each(|value| self.references(value, found)),
                }
            }
            Value::Array(items) => items.iter().for_each(|item| self.references(item, found)),
            _ => {}
        }
    }
}

/// `{"Ref": x}` or `{"Fn::Name": x}`: an object whose only key names an intrinsic function.
fn intrinsic(map: &Map<String, Value>) -> Option<(&str, &Value)> {
    let (name, argument) = map.iter().next().filter(|_| map.len() == 1)?;
    let is_function = name == "Ref" || name.starts_with("Fn::") || (name == "Condition" && argument.is_string());
    is_function.then_some((name.as_str(), argument))
}

fn text(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(_) | Value::Bool(_) => Ok(value.to_string()),
        other => Err(format!("`{other}` is not a string")),
    }
}

fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    value.pointer(&format!("/{}", path.replace('.', "/")))
}

/// What follows `ancestor` in `path` when `path` is `ancestor` or lies below it.
fn below<'a>(path: &'a str, ancestor: &str) -> Option<&'a str> {
    path.strip_prefix(ancestor)
        .filter(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// `VolumeSize` to `volume_size`, applied to every segment of a dotted path.
fn snake(name: &str) -> String {
    let mut output = String::new();
    let mut previous_lower = false;
    for character in name.chars() {
        if character.is_ascii_uppercase() && previous_lower {
            output.push('_');
        }
        previous_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        output.push(character.to_ascii_lowercase());
    }
    output
}

/// Converts keys to snake case. CloudFormation also accepts booleans written as strings.
fn snake_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => map.iter().map(|(key, value)| (snake(key), snake_keys(value))).collect(),
        Value::Array(items) => items.iter().map(snake_keys).collect(),
        Value::String(text) if text == "true" || text == "false" => Value::Bool(text == "true"),
        scalar => scalar.clone(),
    }
}

/// A YAML node, with a short-form function tag rewritten to the long form.
struct Node(Value);

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let Tagged(Untagged(value), tag) = Tagged::<Untagged>::deserialize(deserializer)?;
        let Some(name) = tag
            .as_deref()
            .and_then(|tag| tag.strip_prefix('!'))
            .filter(|name| !name.is_empty() && !name.starts_with('!'))
        else {
            return Ok(Self(value));
        };

        let (key, value) = match (name, value) {
            ("Ref" | "Condition", value) => (name.to_string(), value),
            ("GetAtt", Value::String(path)) => {
                let (resource, attribute) = path.split_once('.').unwrap_or((path.as_str(), ""));
                ("Fn::GetAtt".to_string(), Value::from(vec![resource, attribute]))
            }
            (_, value) => (format!("Fn::{name}"), value),
        };
        Ok(Self(Value::Object(Map::from_iter([(key, value)]))))
    }
}

struct Untagged(Value);

impl<'de> Deserialize<'de> for Untagged {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UntaggedVisitor).map(Self)
    }
}

struct UntaggedVisitor;

impl<'de> Visitor<'de> for UntaggedVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a YAML value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Node(item)) = sequence.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Value, A::Error> {
        let mut map = Map::new();
        while let Some((key, Node(value))) = entries.next_entry::<String, Node>()? {
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Lookup;
    use serde_json::json;
    use std::fs;

    fn write(dir: &Path, name: &str, content: &str) {
        fs::create_dir_all(dir.join(name).parent().unwrap()).unwrap();
        fs::write(dir.join(name), content).unwrap();
    }

    fn parse(dir: &Path, name: &str, region: Option<&str>) -> Parsed {
        let options = Options {
            scan_root: dir,
            max_file_bytes: 1 << 20,
            region,
        };
        parse_file(&dir.join(name), name, "", IacSource::CloudFormation, &options).unwrap()
    }

    fn find<'a>(input: &'a Input, address: &str) -> &'a Resource {
        input
            .changes
            .iter()
            .find(|change| change.address == address)
            .unwrap_or_else(|| {
                panic!(
                    "{address} not in {:?}",
                    input.changes.iter().map(|c| &c.address).collect::<Vec<_>>()
                )
            })
            .after
            .as_ref()
            .unwrap()
    }

    const YAML: &str = r#"
AWSTemplateFormatVersion: "2010-09-09"
Parameters:
  Env: {Type: String, Default: prod}
  Class: {Type: String}
  Sizes: {Type: CommaDelimitedList, Default: "20, 40"}
  Ami: {Type: "AWS::SSM::Parameter::Value<String>", Default: /ami/latest}
Mappings:
  Size:
    prod: {Instance: m5.large}
    dev: {Instance: t3.micro}
Conditions:
  IsProd: !Equals [!Ref Env, prod]
  IsDev: !Not [!Condition IsProd]
  InRegion: !Equals [!Ref "AWS::Region", eu-west-1]
Resources:
  Web:
    Type: AWS::EC2::Instance
    Properties:
      InstanceType: !FindInMap [Size, !Ref Env, Instance]
      ImageId: !Ref Ami
      SubnetId: !Ref Subnet
      BlockDeviceMappings:
        - DeviceName: /dev/xvda
          Ebs:
            VolumeType: gp3
            VolumeSize: !Select [1, !Ref Sizes]
  Database:
    Type: AWS::RDS::DBInstance
    Properties:
      DBInstanceClass: !Ref Class
      Engine: postgres
      MultiAZ: !If [IsProd, "true", "false"]
      AllocatedStorage: "100"
      DBName: !Sub "shop-${Env}-${!Literal}"
      Endpoint: !GetAtt Web.PrivateIp
      Iops: !If [IsProd, !Ref "AWS::NoValue", 3000]
  DevOnly:
    Type: AWS::EC2::NatGateway
    Condition: IsDev
  Regional:
    Type: AWS::EC2::NatGateway
    Condition: InRegion
  Service:
    Type: AWS::ECS::Service
    Properties:
      LaunchType: FARGATE
      TaskDefinition: !Ref Task
  Task:
    Type: AWS::ECS::TaskDefinition
    Properties: {Cpu: "512", Memory: "1024"}
  Queue:
    Type: AWS::SQS::Queue
    Properties:
      QueueName: !Join ["-", [shop, !Ref Env]]
  Cleanup:
    Type: Custom::Cleanup
  Subnet:
    Type: AWS::EC2::Subnet
"#;

    #[test]
    fn yaml_template_resolves_functions_and_translates_priced_types() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "stack.yaml", YAML);
        let input = parse(dir.path(), "stack.yaml", None).input;

        let web = find(&input, "Web");
        assert_eq!(web.resource_type, "aws_instance");
        assert_eq!(web.provider, "aws");
        assert_eq!(web.get("instance_type"), Lookup::Known(&json!("m5.large")));
        assert_eq!(web.get("root_block_device.0.volume_type"), Lookup::Known(&json!("gp3")));
        assert_eq!(web.get("root_block_device.0.volume_size"), Lookup::Known(&json!("40")));

        let database = find(&input, "Database");
        assert_eq!(database.get("multi_az"), Lookup::Known(&json!(true)));
        assert_eq!(database.get("allocated_storage"), Lookup::Known(&json!("100")));
        assert_eq!(database.get("iops"), Lookup::Missing);
        assert_eq!(
            database.get("instance_class"),
            Lookup::Unknown("parameter `Class` has no default value")
        );

        let service = find(&input, "Service");
        assert_eq!(service.get("desired_count"), Lookup::Known(&json!(1)));
        assert!(matches!(service.get("task_definition"), Lookup::Unknown(_)));
        assert_eq!(service.refs["task_definition"], vec!["Task"]);
        assert_eq!(find(&input, "Task").resource_type, "aws_ecs_task_definition");

        let queue = find(&input, "Queue");
        assert_eq!(queue.resource_type, "AWS::SQS::Queue");
        assert_eq!(queue.get("QueueName"), Lookup::Known(&json!("shop-prod")));
        assert_eq!(find(&input, "Cleanup").resource_type, CUSTOM_RESOURCE);

        assert!(input.changes.iter().all(|change| change.address != "DevOnly"));
        assert!(find(&input, "Regional").notes[0].contains("assumed to be created"));
    }

    #[test]
    fn the_scan_region_decides_region_conditions() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "stack.yaml", YAML);

        let elsewhere = parse(dir.path(), "stack.yaml", Some("us-east-1")).input;
        assert!(elsewhere.changes.iter().all(|change| change.address != "Regional"));
        let matching = parse(dir.path(), "stack.yaml", Some("eu-west-1")).input;
        assert!(find(&matching, "Regional").notes.is_empty());
    }

    #[test]
    fn unresolved_values_keep_their_reason_and_never_fall_back() {
        let scope = Scope {
            parameters: Parameters::new(),
            mappings: &Value::Null,
            conditions: &json!({"Loop": {"Condition": "Loop"}}),
            region: None,
        };
        assert!(scope.condition("Loop", 0).unwrap_err().contains("refers to itself"));
        assert!(
            scope
                .eval(&json!({"Fn::GetAZs": ""}), 0)
                .unwrap_err()
                .contains("Fn::GetAZs")
        );
        assert_eq!(
            scope.eval(&json!({"Fn::Sub": ["${A}-${B}", {"A": 1, "B": "x"}]}), 0),
            Ok(json!("1-x"))
        );
        assert_eq!(
            scope.eval(&json!({"Fn::Split": [",", "a,b"]}), 0),
            Ok(json!(["a", "b"]))
        );
    }

    #[test]
    fn nested_stacks_are_inlined_with_the_parameters_the_parent_passes() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "root.json",
            r#"{"AWSTemplateFormatVersion": "2010-09-09", "Resources": {
                "Data": {"Type": "AWS::CloudFormation::Stack", "Properties": {
                    "TemplateURL": "nested/data.yaml",
                    "Parameters": {"Class": "db.t3.medium", "Subnet": {"Ref": "Net"}}}},
                "Remote": {"Type": "AWS::CloudFormation::Stack", "Properties": {
                    "TemplateURL": "https://s3.amazonaws.com/bucket/other.yaml"}},
                "Net": {"Type": "AWS::EC2::Subnet"}
            }}"#,
        );
        write(
            dir.path(),
            "nested/data.yaml",
            "Parameters:\n  Class: {Type: String, Default: db.r5.4xlarge}\n  Subnet: {Type: String, Default: must-not-be-used}\nResources:\n  Db:\n    Type: AWS::RDS::DBInstance\n    Properties:\n      DBInstanceClass: !Ref Class\n      Engine: !Ref Subnet\n",
        );

        let parsed = parse(dir.path(), "root.json", None);
        let database = find(&parsed.input, "Data.Db");

        assert_eq!(database.get("instance_class"), Lookup::Known(&json!("db.t3.medium")));
        assert!(matches!(database.get("engine"), Lookup::Unknown(_)));
        assert_eq!(find(&parsed.input, "Remote").resource_type, NESTED_STACK);
        assert!(parsed.input.warnings[0].contains("not a local file"));
        assert_eq!(
            parsed.nested,
            vec![dir.path().join("nested/data.yaml").canonicalize().unwrap()]
        );
    }

    #[test]
    fn files_that_are_not_templates_or_lie_outside_the_scan_are_refused() {
        let outer = tempfile::tempdir().unwrap();
        write(outer.path(), "repo/notes.yaml", "AWSTemplateFormatVersion: x\n");
        write(outer.path(), "outside.yaml", "Resources: {}\n");
        let options = Options {
            scan_root: &outer.path().join("repo"),
            max_file_bytes: 1 << 20,
            region: None,
        };
        let attempt = |name: &str| {
            let path = outer.path().join(name);
            let result = parse_file(&path, name, "", IacSource::CloudFormation, &options);
            format!("{:#}", result.err().unwrap())
        };

        assert!(attempt("repo/notes.yaml").contains("no `Resources` section"));
        assert!(attempt("outside.yaml").contains("outside the scanned directory"));
        assert!(parse_text("Resources: [", false).is_err());
    }
}
