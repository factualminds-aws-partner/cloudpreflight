//! AWS CDK, read from an already synthesized cloud assembly (`cdk.out`).
//!
//! Synthesis runs the application's own code, so it is never started from here. Each
//! stack in the assembly manifest is a CloudFormation template and is read as one.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use crate::discovery::read_limited;
use crate::iac::cloudformation::{self, Options};
use crate::model::{IacSource, Input};

const MAX_NESTED_ASSEMBLIES: usize = 5;

/// One input per stack, in manifest order. Stage assemblies are followed.
pub fn parse_assembly(dir: &Path, origin: &str, options: &Options) -> Result<Vec<Input>> {
    let mut inputs = Vec::new();
    assembly(dir, origin, options, 0, &mut inputs)?;
    Ok(inputs)
}

fn assembly(dir: &Path, origin: &str, options: &Options, depth: usize, inputs: &mut Vec<Input>) -> Result<()> {
    let manifest_path = dir.join("manifest.json");
    let manifest: Value = read_limited(&manifest_path, options.max_file_bytes)
        .map_err(anyhow::Error::msg)
        .and_then(|text| Ok(serde_json::from_str(&text)?))
        .with_context(|| format!("cannot read {}", manifest_path.display()))?;
    let Some(artifacts) = manifest["artifacts"].as_object() else {
        bail!("{} is not a cloud assembly manifest", manifest_path.display());
    };

    for (id, artifact) in artifacts {
        let properties = &artifact["properties"];
        match artifact["type"].as_str() {
            Some("aws:cloudformation:stack") => {
                let Some(file) = properties["templateFile"].as_str() else {
                    continue;
                };
                // `aws://<account>/<region>`; either part may be `unknown-...`.
                let region = artifact["environment"]
                    .as_str()
                    .and_then(|environment| environment.rsplit('/').next())
                    .filter(|region| !region.is_empty() && !region.starts_with("unknown"));
                let stack_options = Options {
                    region: region.or(options.region),
                    ..*options
                };
                let mut input = cloudformation::parse_file(
                    &dir.join(file),
                    &format!("{origin}/{file}"),
                    &format!("{id}."),
                    IacSource::Cdk,
                    &stack_options,
                )?
                .input;
                if let Some(region) = region {
                    input.provider_regions.insert("aws".to_string(), region.to_string());
                }
                inputs.push(input);
            }
            Some("cdk:cloud-assembly") if depth < MAX_NESTED_ASSEMBLIES => {
                if let Some(name) = properties["directoryName"].as_str() {
                    assembly(&dir.join(name), &format!("{origin}/{name}"), options, depth + 1, inputs)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Lookup;
    use serde_json::json;
    use std::fs;

    #[test]
    fn stacks_stages_and_nested_stacks_are_read_from_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cdk.out");
        let write = |name: &str, content: Value| {
            fs::create_dir_all(out.join(name).parent().unwrap()).unwrap();
            fs::write(out.join(name), content.to_string()).unwrap();
        };
        write(
            "manifest.json",
            json!({"version": "36.0.0", "artifacts": {
                "Api.assets": {"type": "cdk:asset-manifest", "properties": {"file": "Api.assets.json"}},
                "Api": {"type": "aws:cloudformation:stack", "environment": "aws://123456789012/eu-west-1",
                    "properties": {"templateFile": "Api.template.json"}},
                "assembly-Prod": {"type": "cdk:cloud-assembly", "properties": {"directoryName": "assembly-Prod"}},
                "Tree": {"type": "cdk:tree", "properties": {"file": "tree.json"}}
            }}),
        );
        write(
            "Api.template.json",
            json!({"Resources": {
                "Fn": {"Type": "AWS::Lambda::Function", "Properties": {"MemorySize": 512,
                    "Description": {"Fn::Join": ["", ["runs in ", {"Ref": "AWS::Region"}]]}}},
                "Data": {"Type": "AWS::CloudFormation::Stack",
                    "Properties": {"TemplateURL": {"Fn::Join": ["", ["https://s3.", {"Ref": "AWS::URLSuffix"}, "/x.json"]]}},
                    "Metadata": {"aws:asset:path": "ApiData.nested.template.json"}},
                "CDKMetadata": {"Type": "AWS::CDK::Metadata"}
            }}),
        );
        write(
            "ApiData.nested.template.json",
            json!({"Resources": {"Table": {"Type": "AWS::DynamoDB::Table", "Properties": {"BillingMode": "PAY_PER_REQUEST"}}}}),
        );
        write(
            "assembly-Prod/manifest.json",
            json!({"version": "36.0.0", "artifacts": {
                "ProdApi": {"type": "aws:cloudformation:stack", "environment": "aws://unknown-account/unknown-region",
                    "properties": {"templateFile": "ProdApi.template.json"}}
            }}),
        );
        write(
            "assembly-Prod/ProdApi.template.json",
            json!({"Resources": {"Fn": {"Type": "AWS::Lambda::Function"}}}),
        );

        let options = Options {
            scan_root: dir.path(),
            max_file_bytes: 1 << 20,
            region: None,
        };
        let inputs = parse_assembly(&out, "cdk.out", &options).unwrap();

        let addresses: Vec<Vec<&str>> = inputs
            .iter()
            .map(|input| input.changes.iter().map(|change| change.address.as_str()).collect())
            .collect();
        assert_eq!(
            addresses,
            vec![vec!["Api.CDKMetadata", "Api.Data.Table", "Api.Fn"], vec!["ProdApi.Fn"]]
        );
        assert_eq!(inputs[0].origin, "cdk.out/Api.template.json");
        assert_eq!(inputs[0].source, IacSource::Cdk);
        assert_eq!(inputs[0].provider_regions["aws"], "eu-west-1");
        assert!(inputs[1].provider_regions.is_empty());
        assert_eq!(inputs[1].origin, "cdk.out/assembly-Prod/ProdApi.template.json");

        let function = inputs[0].changes[2].after.as_ref().unwrap();
        assert_eq!(function.resource_type, "aws_lambda_function");
        assert_eq!(function.get("memory_size"), Lookup::Known(&json!(512)));
    }

    #[test]
    fn a_directory_without_a_manifest_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let options = Options {
            scan_root: dir.path(),
            max_file_bytes: 1 << 20,
            region: None,
        };
        let error = format!("{:#}", parse_assembly(dir.path(), ".", &options).unwrap_err());
        assert!(error.contains("manifest.json"), "{error}");
    }
}
