//! Repository discovery: a bounded, read-only walk that finds IaC inputs.
//! Symlinks are never followed, so a link cannot lead the scan outside the repository.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use ignore::WalkBuilder;

use crate::iac::terraform_plan;

/// Directories that never contain first-party IaC and are often huge.
const SKIPPED_DIRS: &[&str] = &["node_modules", "vendor", "target", "cdk.out", "dist", "build"];

const TEMPLATE_EXTENSIONS: &[&str] = &[".yaml", ".yml", ".template", ".json"];

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_depth: usize,
    pub max_files: usize,
    pub max_file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 12,
            max_files: 50_000,
            max_file_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub root: PathBuf,
    pub project: String,
    /// Terraform plan JSON files, validated by content.
    pub plans: Vec<PathBuf>,
    /// Terraform root modules: directories with `.tf` files that no other scanned
    /// directory uses as a local module.
    pub terraform_roots: Vec<PathBuf>,
    /// Every `terragrunt.hcl`; the adapter decides which of them are units.
    pub terragrunt: Vec<PathBuf>,
    /// CloudFormation templates, recognised by `AWSTemplateFormatVersion`.
    pub templates: Vec<PathBuf>,
    /// Synthesized CDK cloud assemblies (directories holding a `manifest.json`).
    pub cdk_assemblies: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub fn discover(root: &Path, limits: &Limits) -> Result<Discovery> {
    if !root.exists() {
        bail!("{} does not exist", root.display());
    }
    if root.is_file() {
        bail!(
            "{} is a file. Pass a directory to `scan`, or use `estimate <plan.json>` for a single plan file.",
            root.display()
        );
    }

    let mut discovery = Discovery {
        root: root.to_path_buf(),
        project: project_name(root),
        ..Discovery::default()
    };
    let mut terraform_dirs: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let mut files = 0;

    let walker = WalkBuilder::new(root)
        .standard_filters(false)
        .hidden(true)
        .follow_links(false)
        .max_depth(Some(limits.max_depth))
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !SKIPPED_DIRS.contains(&name.as_ref())
        })
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                discovery.warnings.push(format!("skipped an unreadable path: {error}"));
                continue;
            }
        };
        let path = entry.path();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }

        files += 1;
        if files > limits.max_files {
            discovery.warnings.push(format!(
                "stopped after {} files (limit). Scan a subdirectory or raise `limits.max_files`.",
                limits.max_files
            ));
            break;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        let Some(dir) = path.parent() else {
            continue;
        };
        if name.ends_with(".tf") || name.ends_with(".tf.json") {
            terraform_dirs
                .entry(dir.to_path_buf())
                .or_default()
                .push(path.to_path_buf());
        } else if name == "terragrunt.hcl" {
            discovery.terragrunt.push(path.to_path_buf());
        } else if name == "cdk.json" {
            // The assembly directory itself is not walked; it is found through the app.
            let assembly = dir.join(cdk_output(path, limits));
            if assembly.join("manifest.json").is_file() {
                discovery.cdk_assemblies.push(assembly);
            } else {
                discovery.warnings.push(format!(
                    "{} is a CDK app with no synthesized cloud assembly. Run `cdk synth` there and scan again; CDK code is never executed by this tool.",
                    dir.strip_prefix(root).unwrap_or(dir).display()
                ));
            }
        } else if name.ends_with(".json") && name.to_lowercase().contains("plan") {
            match read_limited(path, limits.max_file_bytes) {
                Ok(text) if terraform_plan::looks_like_plan(&text) => discovery.plans.push(path.to_path_buf()),
                Ok(_) => {}
                Err(reason) => discovery.warnings.push(format!("skipped {}: {reason}", path.display())),
            }
        } else if TEMPLATE_EXTENSIONS.iter().any(|extension| name.ends_with(extension))
            // ponytail: a template without the optional version line is not recognised.
            // Parse candidates fully if that turns out to miss real templates.
            && head(path).contains("AWSTemplateFormatVersion")
        {
            discovery.templates.push(path.to_path_buf());
        }
    }

    discovery.terraform_roots = root_modules(&terraform_dirs, limits);
    Ok(discovery)
}

fn project_name(root: &Path) -> String {
    root.canonicalize()
        .ok()
        .and_then(|path| path.file_name().map(|name| name.to_string_lossy().to_string()))
        .unwrap_or_else(|| root.display().to_string())
}

pub fn read_limited(path: &Path, max_bytes: u64) -> Result<String, String> {
    let size = fs::metadata(path).map_err(|error| error.to_string())?.len();
    if size > max_bytes {
        return Err(format!("{size} bytes is above the {max_bytes} byte limit"));
    }
    fs::read_to_string(path).map_err(|error| error.to_string())
}

/// The `output` directory named in `cdk.json`, or the CDK default.
fn cdk_output(cdk_json: &Path, limits: &Limits) -> String {
    read_limited(cdk_json, limits.max_file_bytes)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|app| app["output"].as_str().map(str::to_string))
        .unwrap_or_else(|| "cdk.out".to_string())
}

fn head(path: &Path) -> String {
    let mut buffer = vec![0; 2048];
    let read = fs::File::open(path)
        .and_then(|mut file| file.read(&mut buffer))
        .unwrap_or(0);
    String::from_utf8_lossy(&buffer[..read]).to_string()
}

/// A directory is a root module unless another scanned directory calls it as a local module.
fn root_modules(terraform_dirs: &BTreeMap<PathBuf, Vec<PathBuf>>, limits: &Limits) -> Vec<PathBuf> {
    let mut called = BTreeSet::new();
    for (dir, files) in terraform_dirs {
        for file in files {
            let Ok(text) = read_limited(file, limits.max_file_bytes) else {
                continue;
            };
            // ponytail: line scan for local `source = "./..."` instead of a full parse;
            // parse errors are reported later by the adapter, with the file name.
            for line in text.lines() {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                let value = value.trim().trim_matches(|c| c == '"' || c == ',');
                let is_source = key.trim().trim_matches('"') == "source";
                if is_source
                    && (value.starts_with("./") || value.starts_with("../"))
                    && let Ok(target) = dir.join(value).canonicalize()
                {
                    called.insert(target);
                }
            }
        }
    }

    terraform_dirs
        .keys()
        .filter(|dir| !dir.canonicalize().is_ok_and(|canonical| called.contains(&canonical)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, name: &str, content: &str) {
        fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
        fs::write(root.join(name), content).unwrap();
    }

    fn relative(root: &Path, paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|path| path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn empty_repository_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let discovery = discover(dir.path(), &Limits::default()).unwrap();
        assert!(discovery.plans.is_empty() && discovery.terraform_roots.is_empty());
    }

    #[test]
    fn finds_roots_plans_and_other_iac_but_skips_modules_hidden_and_vendored_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "infra/main.tf",
            "module \"db\" {\n  source = \"./modules/db\"\n}\n",
        );
        write(root, "infra/modules/db/main.tf", "");
        write(root, "other/main.tf", "");
        write(root, ".terraform/modules/x/main.tf", "");
        write(root, "node_modules/pkg/main.tf", "");
        write(
            root,
            "infra/tfplan.json",
            r#"{"format_version": "1.2", "resource_changes": []}"#,
        );
        write(root, "infra/plan-notes.json", r#"{"hello": "world"}"#);
        write(root, "live/terragrunt.hcl", "");
        write(root, "app/cdk.json", "{}");
        write(root, "app/cdk.out/manifest.json", "{}");
        write(
            root,
            "app/cdk.out/App.template.json",
            r#"{"AWSTemplateFormatVersion": "2010-09-09"}"#,
        );
        write(root, "unsynthesized/cdk.json", r#"{"output": "build/out"}"#);
        write(root, "cfn/stack.yaml", "AWSTemplateFormatVersion: '2010-09-09'\n");
        write(root, "cfn/stack.json", r#"{"AWSTemplateFormatVersion": "2010-09-09"}"#);
        write(root, "cfn/values.yaml", "replicas: 2\n");

        let discovery = discover(root, &Limits::default()).unwrap();

        assert_eq!(relative(root, &discovery.terraform_roots), vec!["infra", "other"]);
        assert_eq!(relative(root, &discovery.plans), vec!["infra/tfplan.json"]);
        assert_eq!(relative(root, &discovery.terragrunt), vec!["live/terragrunt.hcl"]);
        assert_eq!(
            relative(root, &discovery.templates),
            vec!["cfn/stack.json", "cfn/stack.yaml"]
        );
        assert_eq!(relative(root, &discovery.cdk_assemblies), vec!["app/cdk.out"]);
        assert!(discovery.warnings[0].contains("no synthesized cloud assembly"));
    }

    #[test]
    fn depth_and_file_limits_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a/b/c/d/main.tf", "");
        for index in 0..20 {
            write(dir.path(), &format!("file{index}.txt"), "");
        }

        let shallow = Limits {
            max_depth: 2,
            ..Limits::default()
        };
        assert!(discover(dir.path(), &shallow).unwrap().terraform_roots.is_empty());

        let few = Limits {
            max_files: 5,
            ..Limits::default()
        };
        let discovery = discover(dir.path(), &few).unwrap();
        assert!(discovery.warnings.iter().any(|w| w.contains("stopped after 5 files")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "main.tf", "");
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();

        assert!(
            discover(dir.path(), &Limits::default())
                .unwrap()
                .terraform_roots
                .is_empty()
        );
    }

    #[test]
    fn a_file_argument_points_the_user_at_estimate() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plan.json", "{}");
        let error = discover(&dir.path().join("plan.json"), &Limits::default()).unwrap_err();
        assert!(error.to_string().contains("estimate <plan.json>"));
    }
}
