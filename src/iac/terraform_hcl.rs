//! Static analysis of Terraform configuration (`.tf` and `.tf.json`). Nothing is executed.
//!
//! Resolves what can be known without a plan: variable defaults and tfvars, locals,
//! literal `count` and `for_each`, and local-path modules. Everything else (references
//! to other resources, data sources, remote modules, unsupported functions) is recorded
//! as unknown with a reason; it is never guessed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use hcl::eval::{Context, Evaluate, FuncArgs, FuncDef, ParamType};
use hcl::expr::TemplateExpr;
use hcl::{Attribute, Block, Body, Expression, ObjectKey, Structure, Value};

use crate::model::{Action, IacSource, Input, Resource, ResourceChange};

const MAX_MODULE_DEPTH: usize = 10;
const MAX_INSTANCES: u64 = 10_000;
const LOCALS_PASSES: usize = 10;
const META_ARGUMENTS: &[&str] = &[
    "count",
    "for_each",
    "depends_on",
    "provider",
    "lifecycle",
    "provisioner",
    "connection",
];
const MODULE_ARGUMENTS: &[&str] = &["source", "version", "count", "for_each", "depends_on", "providers"];

pub struct Options<'a> {
    /// Local modules must resolve inside this directory.
    pub scan_root: &'a Path,
    pub max_file_bytes: u64,
}

struct Walk<'a> {
    options: &'a Options<'a>,
    changes: Vec<ResourceChange>,
    warnings: Vec<String>,
    provider_regions: BTreeMap<String, String>,
}

/// Variable values handed to a module. `None` marks an input whose value is not known.
type Inputs = BTreeMap<String, Option<Value>>;

struct Instance {
    suffix: String,
    context: Vec<(&'static str, Value)>,
    note: Option<String>,
}

pub fn parse_root(dir: &Path, origin: &str, options: &Options) -> Result<Input> {
    let mut walk = Walk {
        options,
        changes: Vec::new(),
        warnings: Vec::new(),
        provider_regions: BTreeMap::new(),
    };
    walk.module(dir, "", None, 0)?;

    Ok(Input {
        source: IacSource::TerraformStatic,
        origin: origin.to_string(),
        provider_regions: walk.provider_regions,
        changes: walk.changes,
        warnings: walk.warnings,
    })
}

/// Parses every Terraform file in `dir` without evaluating it. Used by `validate`.
pub fn load_dir(dir: &Path, max_file_bytes: u64) -> Result<Vec<Block>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("cannot read directory {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| is_terraform_file(path))
        .collect();
    paths.sort();

    let mut blocks = Vec::new();
    for path in paths {
        let text = read_limited(&path, max_file_bytes)?;
        let body = if path.to_string_lossy().ends_with(".json") {
            let json = serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
            json_body(&json)
        } else {
            hcl::parse(&text).with_context(|| format!("{} is not valid HCL", path.display()))?
        };
        blocks.extend(body.0.into_iter().filter_map(|structure| match structure {
            Structure::Block(block) => Some(block),
            Structure::Attribute(_) => None,
        }));
    }
    Ok(blocks)
}

pub fn is_terraform_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    path.is_file() && (name.ends_with(".tf") || name.ends_with(".tf.json"))
}

fn read_limited(path: &Path, max_bytes: u64) -> Result<String> {
    let size = fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    if size > max_bytes {
        bail!("{} is {size} bytes, above the {max_bytes} byte limit", path.display());
    }
    fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))
}

impl Walk<'_> {
    fn module(&mut self, dir: &Path, prefix: &str, inputs: Option<Inputs>, depth: usize) -> Result<()> {
        let blocks = load_dir(dir, self.options.max_file_bytes)?;
        let is_root = inputs.is_none();
        let inputs = match inputs {
            Some(inputs) => inputs,
            None => self.tfvars(dir),
        };

        let mut context = base_context(dir);
        let mut variables = hcl::Map::new();
        for block in blocks.iter().filter(|block| block.identifier.as_str() == "variable") {
            let Some(name) = label(block, 0) else {
                continue;
            };
            let value = match inputs.get(&name) {
                Some(value) => value.clone(),
                None => attribute(&block.body, "default").and_then(|default| default.evaluate(&context).ok()),
            };
            if let Some(value) = value {
                variables.insert(name, value);
            }
        }
        context.declare_var("var", Value::Object(variables));

        self.locals(&blocks, &mut context);

        if is_root {
            for block in blocks.iter().filter(|block| block.identifier.as_str() == "provider") {
                let (Some(name), None) = (label(block, 0), attribute(&block.body, "alias")) else {
                    continue;
                };
                let region = attribute(&block.body, "region").and_then(|region| region.evaluate(&context).ok());
                if let Some(Value::String(region)) = region {
                    self.provider_regions.insert(name, region);
                }
            }
        }

        for block in &blocks {
            match block.identifier.as_str() {
                "resource" => self.resource(block, prefix, &context),
                "module" => self.module_call(block, dir, prefix, &context, depth)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Values from `terraform.tfvars` and `*.auto.tfvars`, in Terraform's precedence order.
    fn tfvars(&mut self, dir: &Path) -> Inputs {
        let mut inputs = Inputs::new();
        let mut paths: Vec<PathBuf> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.to_string_lossy().ends_with(".auto.tfvars"))
            .collect();
        paths.sort();
        paths.insert(0, dir.join("terraform.tfvars"));

        let empty = Context::new();
        for path in paths.iter().filter(|path| path.is_file()) {
            let parsed = read_limited(path, self.options.max_file_bytes).and_then(|text| Ok(hcl::parse(&text)?));
            let body = match parsed {
                Ok(body) => body,
                Err(error) => {
                    self.warnings.push(format!("{} was ignored: {error:#}", path.display()));
                    continue;
                }
            };
            for structure in body.0 {
                if let Structure::Attribute(Attribute { key, expr }) = structure {
                    inputs.insert(key.to_string(), expr.evaluate(&empty).ok());
                }
            }
        }
        inputs
    }

    /// Locals may reference each other in any order, so evaluate until nothing new resolves.
    fn locals(&mut self, blocks: &[Block], context: &mut Context) {
        let mut pending: Vec<(String, &Expression)> = blocks
            .iter()
            .filter(|block| block.identifier.as_str() == "locals")
            .flat_map(|block| &block.body.0)
            .filter_map(|structure| match structure {
                Structure::Attribute(attribute) => Some((attribute.key.to_string(), &attribute.expr)),
                Structure::Block(_) => None,
            })
            .collect();

        let mut resolved = hcl::Map::new();
        for _ in 0..LOCALS_PASSES {
            context.declare_var("local", Value::Object(resolved.clone()));
            let before = pending.len();
            pending.retain(|(name, expression)| match expression.evaluate(context) {
                Ok(value) => {
                    resolved.insert(name.clone(), value);
                    false
                }
                Err(_) => true,
            });
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        context.declare_var("local", Value::Object(resolved));
    }

    fn resource(&mut self, block: &Block, prefix: &str, context: &Context) {
        let (Some(resource_type), Some(name)) = (label(block, 0), label(block, 1)) else {
            return;
        };
        let base = format!("{prefix}{resource_type}.{name}");

        for instance in instances(&block.body, context) {
            let mut context = context.clone();
            for (variable, value) in instance.context {
                context.declare_var(variable, value);
            }

            let address = format!("{base}{}", instance.suffix);
            let mut resource = Resource::new(&address, &resource_type, serde_json::Value::Null);
            let mut references = BTreeMap::new();
            resource.attrs = body_attributes(&block.body, &context, "", &mut resource.unknown, &mut references);
            resource.refs = references
                .into_iter()
                .map(|(argument, targets): (String, BTreeSet<String>)| {
                    let targets = targets.into_iter().map(|target| format!("{prefix}{target}")).collect();
                    (argument, targets)
                })
                .collect();
            resource.notes.extend(instance.note);

            self.changes.push(ResourceChange {
                address,
                resource_type: resource_type.clone(),
                action: Action::Create,
                before: None,
                after: Some(resource),
            });
        }
    }

    fn module_call(&mut self, block: &Block, dir: &Path, prefix: &str, context: &Context, depth: usize) -> Result<()> {
        let Some(name) = label(block, 0) else {
            return Ok(());
        };
        let source = attribute(&block.body, "source").and_then(|source| source.evaluate(context).ok());
        let Some(Value::String(source)) = source else {
            self.warnings.push(format!(
                "module `{prefix}module.{name}` has no literal source; skipped."
            ));
            return Ok(());
        };
        if !(source.starts_with("./") || source.starts_with("../")) {
            self.warnings.push(format!(
                "module `{prefix}module.{name}` uses remote source `{source}`; its resources are not analysed. Scan a plan JSON for full coverage."
            ));
            return Ok(());
        }
        if depth >= MAX_MODULE_DEPTH {
            self.warnings.push(format!(
                "module `{prefix}module.{name}` is nested deeper than {MAX_MODULE_DEPTH} levels; skipped."
            ));
            return Ok(());
        }

        // Canonicalising resolves `..` and symlinks, so the check cannot be sidestepped.
        let root = self
            .options
            .scan_root
            .canonicalize()
            .unwrap_or_else(|_| self.options.scan_root.to_path_buf());
        let child = match dir.join(&source).canonicalize() {
            Ok(child) if child.starts_with(&root) => child,
            Ok(_) => {
                self.warnings.push(format!(
                    "module `{prefix}module.{name}` source `{source}` is outside the scanned directory; skipped."
                ));
                return Ok(());
            }
            Err(error) => {
                self.warnings.push(format!(
                    "module `{prefix}module.{name}` source `{source}` cannot be read: {error}."
                ));
                return Ok(());
            }
        };

        for instance in instances(&block.body, context) {
            let mut context = context.clone();
            for (variable, value) in instance.context {
                context.declare_var(variable, value);
            }

            let mut inputs = Inputs::new();
            for structure in &block.body.0 {
                if let Structure::Attribute(attribute) = structure
                    && !MODULE_ARGUMENTS.contains(&attribute.key.as_str())
                {
                    inputs.insert(attribute.key.to_string(), attribute.expr.evaluate(&context).ok());
                }
            }

            let child_prefix = format!("{prefix}module.{name}{}.", instance.suffix);
            let first = self.changes.len();
            self.module(&child, &child_prefix, Some(inputs), depth + 1)?;
            if let Some(note) = instance.note {
                for change in &mut self.changes[first..] {
                    if let Some(resource) = &mut change.after {
                        resource.notes.push(note.clone());
                    }
                }
            }
        }
        Ok(())
    }
}

/// Expands `count` and `for_each`. When either cannot be evaluated statically, one
/// instance is assumed and the assumption is attached to the resource.
fn instances(body: &Body, context: &Context) -> Vec<Instance> {
    let single = |note: Option<String>| {
        vec![Instance {
            suffix: String::new(),
            context: Vec::new(),
            note,
        }]
    };

    if let Some(count) = attribute(body, "count") {
        return match count.evaluate(context) {
            Ok(Value::Number(count)) if count.as_u64().is_some_and(|count| count <= MAX_INSTANCES) => {
                (0..count.as_u64().unwrap_or_default())
                    .map(|index| Instance {
                        suffix: format!("[{index}]"),
                        context: vec![("count", object([("index", Value::from(index))]))],
                        note: None,
                    })
                    .collect()
            }
            _ => single(Some(
                "`count` could not be determined statically; one instance assumed.".into(),
            )),
        };
    }

    if let Some(for_each) = attribute(body, "for_each") {
        let entries: Vec<(String, Value)> = match for_each.evaluate(context) {
            Ok(Value::Object(map)) => map.into_iter().collect(),
            Ok(Value::Array(items)) => items
                .into_iter()
                .filter_map(|item| match item {
                    Value::String(key) => Some((key.clone(), Value::String(key))),
                    _ => None,
                })
                .collect(),
            _ => {
                return single(Some(
                    "`for_each` could not be determined statically; one instance assumed.".into(),
                ));
            }
        };
        return entries
            .into_iter()
            .map(|(key, value)| Instance {
                suffix: format!("[\"{key}\"]"),
                context: vec![("each", object([("key", Value::String(key)), ("value", value)]))],
                note: None,
            })
            .collect();
    }

    single(None)
}

/// Converts a block body to the attribute tree used by the model: attributes become
/// values and nested blocks become arrays of objects, as in Terraform plan JSON.
fn body_attributes(
    body: &Body,
    context: &Context,
    path: &str,
    unknown: &mut BTreeMap<String, String>,
    references: &mut BTreeMap<String, BTreeSet<String>>,
) -> serde_json::Value {
    let mut attributes = serde_json::Map::new();
    let join = |key: &str| {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };

    for structure in &body.0 {
        match structure {
            Structure::Attribute(attribute) => {
                let key = attribute.key.as_str();
                if path.is_empty() && META_ARGUMENTS.contains(&key) {
                    continue;
                }
                let found = resource_references(&attribute.expr);
                let top_level = path.split('.').next().filter(|top| !top.is_empty()).unwrap_or(key);
                if !found.is_empty() {
                    references
                        .entry(top_level.to_string())
                        .or_default()
                        .extend(found.iter().cloned());
                }

                let evaluated = attribute
                    .expr
                    .evaluate(context)
                    .ok()
                    .and_then(|value| serde_json::to_value(value).ok());
                match evaluated {
                    Some(value) => {
                        attributes.insert(key.to_string(), value);
                    }
                    None if !found.is_empty() => {
                        unknown.insert(join(key), "depends on another resource; known only after apply".into());
                    }
                    None => {
                        unknown.insert(join(key), "could not be evaluated statically".into());
                    }
                }
            }
            Structure::Block(block) => {
                let key = block.identifier.as_str();
                if path.is_empty() && META_ARGUMENTS.contains(&key) {
                    continue;
                }
                if key == "dynamic" {
                    if let Some(target) = label(block, 0) {
                        unknown.insert(
                            join(&target),
                            "generated by a dynamic block; not evaluated statically".into(),
                        );
                    }
                    continue;
                }
                let items = attributes
                    .entry(key.to_string())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                if let serde_json::Value::Array(items) = items {
                    let item_path = join(&format!("{key}.{}", items.len()));
                    items.push(body_attributes(&block.body, context, &item_path, unknown, references));
                }
            }
        }
    }

    serde_json::Value::Object(attributes)
}

/// Resource addresses (`type.name`) an expression refers to.
fn resource_references(expression: &Expression) -> BTreeSet<String> {
    // ponytail: scans the formatted expression text instead of walking the syntax tree.
    // A string literal that happens to look like `aws_x.y` is a false positive; it only
    // ever makes a value "unknown" sooner. Walk the AST if that proves too coarse.
    let text = expression.to_string();
    let bytes = text.as_bytes();
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
    let mut found = BTreeSet::new();
    let mut index = 0;

    while index < bytes.len() {
        if !is_word(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && is_word(bytes[index]) {
            index += 1;
        }
        let first = &text[start..index];
        let after_dot = start > 0 && bytes[start - 1] == b'.';
        if after_dot || !first.contains('_') || index >= bytes.len() || bytes[index] != b'.' {
            continue;
        }

        let name_start = index + 1;
        let mut name_end = name_start;
        while name_end < bytes.len() && is_word(bytes[name_end]) {
            name_end += 1;
        }
        let looks_like_type = first.starts_with(|c: char| c.is_ascii_lowercase());
        if name_end > name_start && looks_like_type {
            found.insert(format!("{first}.{}", &text[name_start..name_end]));
        }
        index = name_end;
    }
    found
}

fn attribute<'a>(body: &'a Body, key: &str) -> Option<&'a Expression> {
    body.0.iter().find_map(|structure| match structure {
        Structure::Attribute(attribute) if attribute.key.as_str() == key => Some(&attribute.expr),
        _ => None,
    })
}

fn label(block: &Block, index: usize) -> Option<String> {
    block.labels.get(index).map(|label| label.as_str().to_string())
}

fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

fn base_context(dir: &Path) -> Context<'static> {
    let mut context = Context::new();
    let module_path = dir.to_string_lossy().to_string();
    context.declare_var(
        "path",
        object([
            ("module", Value::String(module_path.clone())),
            ("root", Value::String(module_path)),
        ]),
    );
    context.declare_var("terraform", object([("workspace", Value::String("default".into()))]));

    let any = || FuncDef::builder().variadic_param(ParamType::Any);
    context.declare_func(
        "lower",
        any().build(|args| text(&args).map(|s| Value::String(s.to_lowercase()))),
    );
    context.declare_func(
        "upper",
        any().build(|args| text(&args).map(|s| Value::String(s.to_uppercase()))),
    );
    context.declare_func("tostring", any().build(|args| text(&args).map(Value::String)));
    context.declare_func("toset", any().build(first));
    context.declare_func("tolist", any().build(first));
    context.declare_func("tomap", any().build(first));
    context.declare_func(
        "length",
        any().build(|args| match args.first() {
            Some(Value::Array(items)) => Ok(Value::from(items.len() as u64)),
            Some(Value::Object(map)) => Ok(Value::from(map.len() as u64)),
            Some(Value::String(text)) => Ok(Value::from(text.chars().count() as u64)),
            _ => Err("length() needs a list, map or string".to_string()),
        }),
    );
    context.declare_func(
        "merge",
        any().build(|args| {
            let mut merged = hcl::Map::new();
            for argument in args.iter() {
                match argument {
                    Value::Object(map) => merged.extend(map.clone()),
                    Value::Null => {}
                    _ => return Err("merge() needs maps".to_string()),
                }
            }
            Ok(Value::Object(merged))
        }),
    );
    context.declare_func(
        "concat",
        any().build(|args| {
            let mut joined = Vec::new();
            for argument in args.iter() {
                match argument {
                    Value::Array(items) => joined.extend(items.clone()),
                    _ => return Err("concat() needs lists".to_string()),
                }
            }
            Ok(Value::Array(joined))
        }),
    );
    context
}

fn first(args: FuncArgs) -> Result<Value, String> {
    args.first()
        .cloned()
        .ok_or_else(|| "one argument is required".to_string())
}

fn text(args: &FuncArgs) -> Result<String, String> {
    match args.first() {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Number(number)) => Ok(number.to_string()),
        Some(Value::Bool(flag)) => Ok(flag.to_string()),
        _ => Err("a string argument is required".to_string()),
    }
}

/// Terraform JSON syntax to the native structure. Block types take a fixed number of labels.
fn json_body(json: &serde_json::Value) -> Body {
    let mut structures = Vec::new();
    for (kind, content) in json.as_object().into_iter().flatten() {
        let labels = match kind.as_str() {
            "resource" | "data" => 2,
            "variable" | "module" | "provider" | "output" => 1,
            "locals" | "terraform" => 0,
            _ => continue,
        };
        json_blocks(kind, content, labels, &mut Vec::new(), &mut structures);
    }
    Body(structures)
}

fn json_blocks(
    kind: &str,
    content: &serde_json::Value,
    labels_left: usize,
    labels: &mut Vec<String>,
    output: &mut Vec<Structure>,
) {
    match content {
        // Repeated blocks may be written as an array of bodies at any level.
        serde_json::Value::Array(items) => {
            for item in items {
                json_blocks(kind, item, labels_left, labels, output);
            }
        }
        serde_json::Value::Object(map) if labels_left > 0 => {
            for (label, nested) in map {
                labels.push(label.clone());
                json_blocks(kind, nested, labels_left - 1, labels, output);
                labels.pop();
            }
        }
        serde_json::Value::Object(map) => {
            let mut block = Block::builder(kind).add_labels(labels.iter().cloned());
            for (key, value) in map {
                block = block.add_attribute((key.as_str(), json_expression(value)));
            }
            output.push(Structure::Block(block.build()));
        }
        _ => {}
    }
}

fn json_expression(value: &serde_json::Value) -> Expression {
    match value {
        serde_json::Value::String(text) if text.contains("${") || text.contains("%{") => {
            Expression::TemplateExpr(Box::new(TemplateExpr::QuotedString(text.clone())))
        }
        serde_json::Value::Array(items) => Expression::Array(items.iter().map(json_expression).collect()),
        serde_json::Value::Object(map) => Expression::Object(
            map.iter()
                .map(|(key, value)| (ObjectKey::from(key.clone()), json_expression(value)))
                .collect(),
        ),
        scalar => hcl::to_expression(scalar).unwrap_or(Expression::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Lookup;
    use serde_json::json;

    fn write(dir: &Path, name: &str, content: &str) {
        fs::create_dir_all(dir.join(name).parent().unwrap()).unwrap();
        fs::write(dir.join(name), content).unwrap();
    }

    fn parse(dir: &Path) -> Input {
        let options = Options {
            scan_root: dir,
            max_file_bytes: 1 << 20,
        };
        parse_root(dir, ".", &options).unwrap()
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

    #[test]
    fn variables_tfvars_locals_and_provider_region_resolve() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "main.tf",
            r#"
            variable "size" { default = "t3.micro" }
            variable "disk" { default = 20 }
            variable "undefined" {}
            locals {
              full = "${local.prefix}-web"
              prefix = "shop"
            }
            provider "aws" { region = "eu-west-1" }
            provider "aws" {
              alias  = "replica"
              region = "us-west-2"
            }
            resource "aws_instance" "web" {
              instance_type = var.size
              ami           = var.undefined
              tags          = { Name = local.full }
              subnet_id     = aws_subnet.main.id
              user_data     = file("init.sh")
              root_block_device {
                volume_size = var.disk * 2
                volume_type = lower("GP3")
              }
            }
        "#,
        );
        write(dir.path(), "terraform.tfvars", "size = \"t3.large\"\n");

        let input = parse(dir.path());
        let web = find(&input, "aws_instance.web");

        assert_eq!(input.provider_regions["aws"], "eu-west-1");
        assert_eq!(web.get("instance_type"), Lookup::Known(&json!("t3.large")));
        assert_eq!(web.get("tags.Name"), Lookup::Known(&json!("shop-web")));
        assert_eq!(web.get("root_block_device.0.volume_size"), Lookup::Known(&json!(40)));
        assert_eq!(web.get("root_block_device.0.volume_type"), Lookup::Known(&json!("gp3")));
        assert_eq!(web.get("ami"), Lookup::Unknown("could not be evaluated statically"));
        assert_eq!(
            web.get("user_data"),
            Lookup::Unknown("could not be evaluated statically")
        );
        assert_eq!(
            web.get("subnet_id"),
            Lookup::Unknown("depends on another resource; known only after apply")
        );
        assert_eq!(web.refs["subnet_id"], vec!["aws_subnet.main"]);
    }

    #[test]
    fn count_and_for_each_expand_and_unknown_counts_are_flagged() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "main.tf",
            r#"
            variable "buckets" { default = { logs = 30, assets = 365 } }
            resource "aws_instance" "worker" {
              count         = 2
              instance_type = "t3.micro"
              tags          = { Index = count.index }
            }
            resource "aws_instance" "none" { count = 0 }
            resource "aws_s3_bucket" "b" {
              for_each = var.buckets
              bucket   = "shop-${each.key}"
            }
            resource "aws_s3_bucket" "set" {
              for_each = toset(["a", "b"])
              bucket   = each.value
            }
            resource "aws_instance" "dynamic" {
              count = length(data.aws_availability_zones.all.names)
            }
        "#,
        );

        let input = parse(dir.path());
        let addresses: Vec<_> = input.changes.iter().map(|c| c.address.as_str()).collect();

        assert_eq!(
            addresses,
            vec![
                "aws_instance.worker[0]",
                "aws_instance.worker[1]",
                "aws_s3_bucket.b[\"logs\"]",
                "aws_s3_bucket.b[\"assets\"]",
                "aws_s3_bucket.set[\"a\"]",
                "aws_s3_bucket.set[\"b\"]",
                "aws_instance.dynamic",
            ]
        );
        assert_eq!(
            find(&input, "aws_instance.worker[1]").get("tags.Index"),
            Lookup::Known(&json!(1))
        );
        assert_eq!(
            find(&input, "aws_s3_bucket.b[\"logs\"]").get("bucket"),
            Lookup::Known(&json!("shop-logs"))
        );
        assert!(find(&input, "aws_instance.dynamic").notes[0].contains("one instance assumed"));
    }

    #[test]
    fn local_modules_receive_inputs_and_remote_or_escaping_modules_are_reported() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("repo");
        write(
            &root,
            "main.tf",
            r#"
            module "db" {
              source = "./modules/db"
              count  = 2
              class  = "db.t3.medium"
              subnet = aws_subnet.main.id
            }
            module "vpc" {
              source  = "terraform-aws-modules/vpc/aws"
              version = "5.0.0"
            }
            module "escape" { source = "../outside" }
        "#,
        );
        write(
            &root,
            "modules/db/main.tf",
            r#"
            variable "class" {}
            variable "subnet" { default = "must-not-be-used" }
            variable "storage" { default = 50 }
            resource "aws_db_instance" "this" {
              instance_class    = var.class
              allocated_storage = var.storage
              db_subnet_group_name = var.subnet
              parameter_group_name = aws_db_parameter_group.this.name
            }
        "#,
        );
        write(
            outer.path(),
            "outside/main.tf",
            "resource \"aws_instance\" \"leak\" {}\n",
        );

        let input = parse(&root);
        let addresses: Vec<_> = input.changes.iter().map(|c| c.address.as_str()).collect();

        assert_eq!(
            addresses,
            vec!["module.db[0].aws_db_instance.this", "module.db[1].aws_db_instance.this"]
        );
        let database = find(&input, "module.db[1].aws_db_instance.this");
        assert_eq!(database.get("instance_class"), Lookup::Known(&json!("db.t3.medium")));
        assert_eq!(database.get("allocated_storage"), Lookup::Known(&json!(50)));
        // An input the caller could not resolve must not fall back to the variable default.
        assert!(matches!(database.get("db_subnet_group_name"), Lookup::Unknown(_)));
        assert_eq!(
            database.refs["parameter_group_name"],
            vec!["module.db[1].aws_db_parameter_group.this"]
        );
        assert!(
            input
                .warnings
                .iter()
                .any(|w| w.contains("remote source `terraform-aws-modules/vpc/aws`")),
            "{:?}",
            input.warnings
        );
        assert!(
            input
                .warnings
                .iter()
                .any(|w| w.contains("outside the scanned directory")),
            "{:?}",
            input.warnings
        );
    }

    #[test]
    fn json_syntax_is_supported() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "main.tf.json",
            r#"{
            "variable": {"size": {"default": "t3.small"}},
            "resource": {"aws_instance": {"web": {
                "instance_type": "${var.size}",
                "root_block_device": {"volume_size": 30}
            }}}
        }"#,
        );

        let input = parse(dir.path());
        let web = find(&input, "aws_instance.web");

        assert_eq!(web.get("instance_type"), Lookup::Known(&json!("t3.small")));
        assert_eq!(web.get("root_block_device.0.volume_size"), Lookup::Known(&json!(30)));
    }

    #[test]
    fn malformed_hcl_is_an_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "broken.tf",
            "resource \"aws_instance\" \"web\" {\n  instance_type = \n",
        );
        let options = Options {
            scan_root: dir.path(),
            max_file_bytes: 1 << 20,
        };

        let error = format!("{:#}", parse_root(dir.path(), ".", &options).unwrap_err());

        assert!(
            error.contains("broken.tf") && error.contains("not valid HCL"),
            "{error}"
        );
    }

    #[test]
    fn oversized_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "big.tf", &"# padding\n".repeat(100));
        let options = Options {
            scan_root: dir.path(),
            max_file_bytes: 64,
        };
        assert!(format!("{:#}", parse_root(dir.path(), ".", &options).unwrap_err()).contains("byte limit"));
    }

    #[test]
    fn reference_scanner_ignores_variables_locals_and_data_sources() {
        let expression: Expression = hcl::parse("x = [aws_subnet.a.id, var.my_var.aws_thing, data.aws_ami.x.id, local.some_name.y, \"${aws_lb.main.arn}/x\"]")
            .unwrap()
            .0
            .into_iter()
            .find_map(|structure| match structure {
                Structure::Attribute(attribute) => Some(attribute.expr),
                Structure::Block(_) => None,
            })
            .unwrap();
        let found: Vec<_> = resource_references(&expression).into_iter().collect();
        assert_eq!(found, vec!["aws_lb.main", "aws_subnet.a"]);
    }
}
