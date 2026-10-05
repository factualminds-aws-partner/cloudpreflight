//! Static analysis of Terragrunt units (`terragrunt.hcl`). Nothing is executed.
//!
//! A unit is resolved to the local Terraform module its `terraform.source` names, and
//! that module is analysed with the unit's `inputs`. `include` blocks, `locals`,
//! `read_terragrunt_config` and the path helper functions are evaluated. Dependency
//! outputs, environment lookups and remote sources are not: an input that needs them is
//! unknown, and a unit with a remote source is reported and left out.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use hcl::eval::{Context, Evaluate, FuncArgs, FuncDef, ParamType};
use hcl::{Block, Body, Expression, ObjectKey, Value};

use crate::discovery::read_limited;
use crate::iac::terraform_hcl::{self, Inputs, Options, attribute, base_context, declare_locals, label};
use crate::model::{IacSource, Input};

const MAX_CONFIG_DEPTH: usize = 5;

pub struct Unit {
    pub input: Input,
    /// Canonical path of the module that was analysed; `None` when the unit was left out.
    pub module_dir: Option<PathBuf>,
}

/// What the Terragrunt functions need to know about the unit being evaluated.
#[derive(Clone, Default)]
struct Paths {
    unit: PathBuf,
    /// Directory of the first included configuration.
    parent: Option<PathBuf>,
    scan_root: PathBuf,
    max_file_bytes: u64,
    depth: usize,
}

// hcl-rs functions are plain `fn` pointers and cannot capture the unit they run for.
thread_local! {
    static PATHS: RefCell<Paths> = RefCell::default();
}

fn paths() -> Paths {
    PATHS.with(|paths| paths.borrow().clone())
}

#[derive(Default)]
struct Layer {
    locals: hcl::Map<String, Value>,
    inputs: Inputs,
    /// Some inputs exist but which variables they set could not be determined.
    opaque: bool,
    /// `terraform.source`; `Some(None)` when it is set but could not be evaluated.
    source: Option<Option<String>>,
    regions: BTreeMap<String, String>,
}

/// Returns `None` for a configuration that is not a unit (one that only exists to be included).
pub fn parse_unit(config: &Path, origin: &str, options: &Options) -> Result<Option<Unit>> {
    let dir = config.parent().unwrap_or(Path::new("."));
    let body = load(config, options.max_file_bytes)?;
    PATHS.set(Paths {
        unit: dir.to_path_buf(),
        parent: None,
        scan_root: options.scan_root.to_path_buf(),
        max_file_bytes: options.max_file_bytes,
        depth: 0,
    });

    let mut warnings = Vec::new();
    let mut merged = Layer::default();

    // Terragrunt reads `include` before anything else, so only functions are available to it.
    let mut include_context = Context::new();
    declare_functions(&mut include_context);
    let mut parents = Vec::new();
    for block in body.blocks().filter(|block| block.identifier.as_str() == "include") {
        let path = attribute(&block.body, "path")
            .and_then(|path| path.evaluate(&include_context).ok())
            .and_then(|path| path.as_str().map(|path| dir.join(path)))
            .filter(|path| path.is_file() && inside(options.scan_root, path).is_some());
        match path {
            Some(path) => parents.push((label(block, 0).unwrap_or_default(), path)),
            None => {
                merged.opaque = true;
                warnings.push(format!(
                    "Terragrunt unit {origin}: an `include` could not be resolved inside the scanned directory."
                ));
            }
        }
    }
    let parent_dir = parents
        .first()
        .and_then(|(_, path)| path.parent().map(Path::to_path_buf));
    PATHS.with(|paths| paths.borrow_mut().parent = parent_dir);

    let mut exposed = hcl::Map::new();
    for (name, path) in &parents {
        let layer = evaluate(&load(path, options.max_file_bytes)?, None);
        exposed.insert(name.clone(), layer.exposed());
        merged.absorb(layer);
    }
    merged.absorb(evaluate(&body, Some(Value::Object(exposed))));

    let left_out = |mut warnings: Vec<String>, reason: String| {
        warnings.push(format!(
            "Terragrunt unit {origin}: {reason}; its resources are not analysed. Scan a plan JSON for full coverage."
        ));
        Ok(Some(Unit {
            input: input(origin, warnings),
            module_dir: None,
        }))
    };
    let module_dir = match &merged.source {
        None if has_terraform_files(dir) => dir.to_path_buf(),
        None => return Ok(None),
        Some(None) => {
            return left_out(
                warnings,
                "its `terraform.source` could not be evaluated statically".into(),
            );
        }
        Some(Some(source)) => {
            let path = source.split('?').next().unwrap_or_default();
            if !(path.starts_with('.') || Path::new(path).is_absolute()) {
                return left_out(warnings, format!("it uses remote source `{source}`"));
            }
            // `modules//app` marks the module root for Terragrunt; on disk it is one path.
            dir.join(path.replacen("//", "/", 1))
        }
    };
    let Some(canonical) = inside(options.scan_root, &module_dir) else {
        return left_out(
            warnings,
            format!(
                "its source `{}` cannot be read inside the scanned directory",
                module_dir.display()
            ),
        );
    };

    let mut inputs = merged.inputs;
    if merged.opaque {
        for block in terraform_hcl::load_dir(&module_dir, options.max_file_bytes)? {
            if let ("variable", Some(name)) = (block.identifier.as_str(), label(&block, 0)) {
                inputs.entry(name).or_insert(None);
            }
        }
        warnings.push(format!(
            "Terragrunt unit {origin}: some inputs could not be evaluated statically; the module variables they may set are treated as unknown."
        ));
    }

    let mut analysed = terraform_hcl::parse_root_with(&module_dir, origin, options, inputs)?;
    analysed.source = IacSource::TerragruntStatic;
    for (provider, region) in merged.regions {
        analysed.provider_regions.entry(provider).or_insert(region);
    }
    warnings.append(&mut analysed.warnings);
    analysed.warnings = warnings;

    Ok(Some(Unit {
        input: analysed,
        module_dir: Some(canonical),
    }))
}

fn input(origin: &str, warnings: Vec<String>) -> Input {
    Input {
        source: IacSource::TerragruntStatic,
        origin: origin.to_string(),
        provider_regions: BTreeMap::new(),
        changes: Vec::new(),
        warnings,
    }
}

fn load(path: &Path, max_file_bytes: u64) -> Result<Body> {
    let text = read_limited(path, max_file_bytes)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("cannot read {}", path.display()))?;
    hcl::parse(&text).with_context(|| format!("{} is not valid HCL", path.display()))
}

/// Canonical form of `path` when it lies inside `scan_root`.
fn inside(scan_root: &Path, path: &Path) -> Option<PathBuf> {
    let root = scan_root.canonicalize().ok()?;
    path.canonicalize().ok().filter(|path| path.starts_with(&root))
}

fn has_terraform_files(dir: &Path) -> bool {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .any(|entry| terraform_hcl::is_terraform_file(&entry.path()))
}

impl Layer {
    /// A later layer (the unit itself) overrides what an included one set.
    fn absorb(&mut self, other: Layer) {
        self.inputs.extend(other.inputs);
        self.opaque |= other.opaque;
        self.regions.extend(other.regions);
        if other.source.is_some() {
            self.source = other.source;
        }
    }

    /// The shape `include.<name>` and `read_terragrunt_config` give to a configuration.
    fn exposed(&self) -> Value {
        let inputs = self
            .inputs
            .iter()
            .filter_map(|(name, value)| Some((name.clone(), value.clone()?)))
            .collect();
        Value::Object(hcl::Map::from_iter([
            ("locals".to_string(), Value::Object(self.locals.clone())),
            ("inputs".to_string(), Value::Object(inputs)),
        ]))
    }
}

fn evaluate(body: &Body, include: Option<Value>) -> Layer {
    let mut context = base_context(&paths().unit);
    declare_functions(&mut context);
    if let Some(include) = include {
        context.declare_var("include", include);
    }
    let blocks: Vec<Block> = body.blocks().cloned().collect();
    let mut layer = Layer {
        locals: declare_locals(&blocks, &mut context),
        ..Layer::default()
    };

    match attribute(body, "inputs").map(|inputs| (inputs.evaluate(&context), inputs)) {
        None => {}
        Some((Ok(Value::Object(inputs)), _)) => {
            layer.inputs = inputs.into_iter().map(|(name, value)| (name, Some(value))).collect();
        }
        // One input that cannot be evaluated must not hide the others.
        Some((_, Expression::Object(entries))) => {
            for (key, value) in entries {
                let name = match key {
                    ObjectKey::Identifier(name) => Some(name.to_string()),
                    ObjectKey::Expression(name) => match name.evaluate(&context) {
                        Ok(Value::String(name)) => Some(name),
                        _ => None,
                    },
                    _ => None,
                };
                match name {
                    Some(name) => {
                        layer.inputs.insert(name, value.evaluate(&context).ok());
                    }
                    None => layer.opaque = true,
                }
            }
        }
        Some(_) => layer.opaque = true,
    }

    layer.source = body
        .blocks()
        .find(|block| block.identifier.as_str() == "terraform")
        .and_then(|block| attribute(&block.body, "source"))
        .map(|source| match source.evaluate(&context) {
            Ok(Value::String(source)) => Some(source),
            _ => None,
        });

    // A `generate` block usually writes the provider configuration, region included.
    let empty = Context::new();
    for block in body.blocks().filter(|block| block.identifier.as_str() == "generate") {
        let generated = attribute(&block.body, "contents")
            .and_then(|contents| contents.evaluate(&context).ok())
            .and_then(|contents| hcl::parse(contents.as_str()?).ok());
        for provider in generated.iter().flat_map(Body::blocks) {
            let region = attribute(&provider.body, "region").and_then(|region| region.evaluate(&empty).ok());
            if let (true, Some(name), None, Some(Value::String(region))) = (
                provider.identifier.as_str() == "provider",
                label(provider, 0),
                attribute(&provider.body, "alias"),
                region,
            ) {
                layer.regions.insert(name, region);
            }
        }
    }
    layer
}

fn declare_functions(context: &mut Context) {
    let function = || FuncDef::builder().variadic_param(ParamType::Any);
    context.declare_func(
        "get_terragrunt_dir",
        function().build(|_| Ok(path_value(&paths().unit))),
    );
    context.declare_func(
        "get_parent_terragrunt_dir",
        function().build(|_| {
            let paths = paths();
            Ok(path_value(paths.parent.as_ref().unwrap_or(&paths.unit)))
        }),
    );
    context.declare_func(
        "path_relative_to_include",
        function().build(|_| relative_to_include().map(|parts| Value::String(parts.join("/")))),
    );
    context.declare_func(
        "path_relative_from_include",
        function().build(|_| relative_to_include().map(|parts| Value::String(vec![".."; parts.len()].join("/")))),
    );
    context.declare_func("find_in_parent_folders", function().build(find_in_parent_folders));
    context.declare_func(
        "get_repo_root",
        function().build(|_| {
            let paths = paths();
            paths
                .unit
                .ancestors()
                .take_while(|dir| dir.starts_with(&paths.scan_root))
                .find(|dir| dir.join(".git").exists())
                .map(path_value)
                .ok_or_else(|| "no repository root inside the scanned directory".to_string())
        }),
    );
    context.declare_func("read_terragrunt_config", function().build(read_terragrunt_config));
}

fn path_value(path: &Path) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

/// The unit's directory relative to the included configuration's, as path segments.
fn relative_to_include() -> Result<Vec<String>, String> {
    let paths = paths();
    let Some(parent) = &paths.parent else {
        return Ok(vec![".".to_string()]);
    };
    let relative = paths
        .unit
        .strip_prefix(parent)
        .map_err(|_| "the unit is not below the included configuration".to_string())?;
    Ok(relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect())
}

/// Searches upwards from the unit, never above the scanned directory.
fn find_in_parent_folders(args: FuncArgs) -> Result<Value, String> {
    let paths = paths();
    let name = match args.first() {
        Some(Value::String(name)) => name.as_str(),
        _ => "terragrunt.hcl",
    };
    let found = paths
        .unit
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(&paths.scan_root))
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.exists());
    match (found, args.get(1)) {
        (Some(found), _) => Ok(path_value(&found)),
        (None, Some(fallback)) => Ok(fallback.clone()),
        (None, None) => Err(format!("`{name}` was not found in a parent folder")),
    }
}

fn read_terragrunt_config(args: FuncArgs) -> Result<Value, String> {
    let paths = paths();
    let Some(Value::String(path)) = args.first() else {
        return Err("read_terragrunt_config() needs a path".to_string());
    };
    let path = paths.unit.join(path);
    if paths.depth >= MAX_CONFIG_DEPTH || inside(&paths.scan_root, &path).is_none() {
        return Err(format!("{} is not read", path.display()));
    }

    PATHS.with(|paths| paths.borrow_mut().depth += 1);
    let layer = load(&path, paths.max_file_bytes).map(|body| evaluate(&body, None));
    PATHS.with(|paths| paths.borrow_mut().depth -= 1);
    layer.map(|layer| layer.exposed()).map_err(|error| format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Lookup, Resource};
    use serde_json::json;

    fn write(dir: &Path, name: &str, content: &str) {
        fs::create_dir_all(dir.join(name).parent().unwrap()).unwrap();
        fs::write(dir.join(name), content).unwrap();
    }

    fn parse(root: &Path, unit: &str) -> Option<Unit> {
        let options = Options {
            scan_root: root,
            max_file_bytes: 1 << 20,
        };
        parse_unit(&root.join(unit).join("terragrunt.hcl"), unit, &options).unwrap()
    }

    fn only(unit: &Unit) -> &Resource {
        assert_eq!(unit.input.changes.len(), 1, "{:?}", unit.input.warnings);
        unit.input.changes[0].after.as_ref().unwrap()
    }

    const MODULE: &str = r#"
        variable "size" { default = "t3.micro" }
        variable "disk" { default = 8 }
        variable "ami" { default = "ami-default" }
        variable "name" { default = "unnamed" }
        resource "aws_instance" "web" {
          instance_type = var.size
          ami           = var.ami
          tags          = { Name = var.name }
          root_block_device { volume_size = var.disk }
        }
    "#;

    #[test]
    fn a_unit_passes_its_own_and_included_inputs_to_a_local_module() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "modules/app/main.tf", MODULE);
        write(
            root,
            "live/terragrunt.hcl",
            r#"
            locals {
              env = read_terragrunt_config(find_in_parent_folders("env.hcl"))
            }
            generate "provider" {
              path      = "provider.tf"
              if_exists = "overwrite"
              contents  = "provider \"aws\" {\n  region = \"${local.env.locals.region}\"\n}\n"
            }
            inputs = {
              size = "t3.small"
              disk = local.env.locals.disk
              name = "${local.env.locals.name}-${path_relative_to_include()}"
            }
            "#,
        );
        write(
            root,
            "live/prod/env.hcl",
            "locals {\n  region = \"eu-west-1\"\n  disk = 100\n  name = \"shop\"\n}\n",
        );
        write(
            root,
            "live/prod/app/terragrunt.hcl",
            r#"
            include "root" {
              path = find_in_parent_folders()
            }
            terraform {
              source = "${get_terragrunt_dir()}/../../../modules//app?ref=v1"
            }
            dependency "vpc" {
              config_path = "../vpc"
            }
            inputs = {
              size = "m5.large"
              ami  = dependency.vpc.outputs.ami
            }
            "#,
        );

        let unit = parse(root, "live/prod/app").unwrap();
        let web = only(&unit);

        assert_eq!(unit.input.source, IacSource::TerragruntStatic);
        assert_eq!(unit.input.provider_regions["aws"], "eu-west-1");
        assert_eq!(unit.module_dir, Some(root.join("modules/app").canonicalize().unwrap()));
        assert_eq!(web.get("instance_type"), Lookup::Known(&json!("m5.large")));
        assert_eq!(web.get("root_block_device.0.volume_size"), Lookup::Known(&json!(100)));
        assert_eq!(web.get("tags.Name"), Lookup::Known(&json!("shop-prod/app")));
        // A dependency output is not known statically and must not fall back to the default.
        assert!(matches!(web.get("ami"), Lookup::Unknown(_)));
        // The included root configuration is not a unit itself.
        assert!(parse(root, "live").is_none());
    }

    #[test]
    fn a_unit_without_a_source_uses_the_terraform_in_its_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "app/main.tf", MODULE);
        write(
            dir.path(),
            "app/terragrunt.hcl",
            "inputs = {\n  size = \"c5.xlarge\"\n}\n",
        );
        write(dir.path(), "app/terraform.tfvars", "disk = 50\n");

        let unit = parse(dir.path(), "app").unwrap();

        assert_eq!(only(&unit).get("instance_type"), Lookup::Known(&json!("c5.xlarge")));
        assert_eq!(
            only(&unit).get("root_block_device.0.volume_size"),
            Lookup::Known(&json!(50))
        );
    }

    #[test]
    fn inputs_that_cannot_be_read_make_every_variable_unknown() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "app/main.tf", MODULE);
        write(
            dir.path(),
            "app/terragrunt.hcl",
            "inputs = merge(yamldecode(file(\"x.yaml\")), { size = \"c5.xlarge\" })\n",
        );

        let unit = parse(dir.path(), "app").unwrap();

        assert!(matches!(only(&unit).get("instance_type"), Lookup::Unknown(_)));
        assert!(matches!(
            only(&unit).get("root_block_device.0.volume_size"),
            Lookup::Unknown(_)
        ));
        assert!(unit.input.warnings[0].contains("treated as unknown"));
    }

    #[test]
    fn remote_escaping_and_unevaluable_sources_are_reported_not_analysed() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("repo");
        write(outer.path(), "outside/main.tf", MODULE);
        let unit_with = |source: &str| {
            write(
                &root,
                "app/terragrunt.hcl",
                &format!("terraform {{\n  source = {source}\n}}\n"),
            );
            let unit = parse(&root, "app").unwrap();
            assert!(unit.input.changes.is_empty() && unit.module_dir.is_none());
            unit.input.warnings.join(" ")
        };

        assert!(unit_with("\"git::https://example.com/modules.git//app?ref=v1\"").contains("remote source"));
        assert!(unit_with("\"../../outside\"").contains("inside the scanned directory"));
        assert!(unit_with("get_env(\"MODULE\")").contains("could not be evaluated"));
    }

    #[test]
    fn malformed_configuration_is_an_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "app/terragrunt.hcl", "inputs = {\n");
        let options = Options {
            scan_root: dir.path(),
            max_file_bytes: 1 << 20,
        };
        let error = parse_unit(&dir.path().join("app/terragrunt.hcl"), "app", &options)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("terragrunt.hcl is not valid HCL"));
    }
}
