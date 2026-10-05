//! Configuration file (`.factualminds-cost.yaml`). Every field is optional.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

pub const FILE_NAME: &str = ".factualminds-cost.yaml";
const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: Option<u32>,
    pub defaults: Defaults,
    pub aws: Aws,
    pub usage_profile: Option<String>,
    /// Shaped like a usage profile: `aws: { s3: { storage_gb: 500 } }`.
    pub usage: Value,
    /// Per-resource overrides keyed by resource address.
    pub resource: BTreeMap<String, ResourceOverride>,
    pub budget: Budget,
    pub cache: CacheConfig,
    pub limits: LimitsConfig,
    // Accepted so that configs written for the full specification load; not acted on yet.
    pub execution: Value,
    pub rules: Value,
    pub output: Value,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub currency: Option<String>,
    pub monthly_hours: Option<Decimal>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Aws {
    pub region: Option<String>,
    pub pricing_mode: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ResourceOverride {
    pub usage: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Budget {
    pub monthly: Option<Decimal>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub dir: Option<PathBuf>,
    pub ttl_hours: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    pub max_depth: Option<usize>,
    pub max_files: Option<usize>,
    pub max_file_mb: Option<u64>,
}

impl Config {
    /// Loads `explicit` if given, else the repository file, else the user-level file.
    /// Returns the config and the path it came from.
    pub fn load(explicit: Option<&Path>, repo: &Path) -> Result<(Self, Option<PathBuf>)> {
        let user_level = dirs::config_dir().map(|dir| dir.join("cloudpreflight").join("config.yaml"));
        let path = match explicit {
            Some(path) if !path.is_file() => bail!("config file {} does not exist", path.display()),
            Some(path) => Some(path.to_path_buf()),
            None => [Some(repo.join(FILE_NAME)), user_level]
                .into_iter()
                .flatten()
                .find(|path| path.is_file()),
        };
        let Some(path) = path else {
            return Ok((Self::default(), None));
        };

        let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let config = Self::parse(&text).with_context(|| format!("invalid config file {}", path.display()))?;
        Ok((config, Some(path)))
    }

    pub fn parse(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let config: Self = serde_saphyr::from_str(text)?;

        if let Some(version) = config.version
            && version != SUPPORTED_VERSION
        {
            bail!("config version {version} is not supported (supported: {SUPPORTED_VERSION})");
        }
        if let Some(hours) = config.defaults.monthly_hours
            && (hours <= Decimal::ZERO || hours > Decimal::from(744))
        {
            bail!("defaults.monthly_hours must be between 1 and 744, got {hours}");
        }
        if let Some(mode) = &config.aws.pricing_mode
            && mode != "public"
        {
            bail!("aws.pricing_mode `{mode}` is not available yet; only `public` list pricing is supported");
        }
        if let Some(budget) = config.budget.monthly
            && budget <= Decimal::ZERO
        {
            bail!("budget.monthly must be greater than zero");
        }
        Ok(config)
    }

    pub fn resource_usage(&self) -> BTreeMap<String, BTreeMap<String, Value>> {
        self.resource
            .iter()
            .map(|(address, resource)| (address.clone(), resource.usage.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    #[test]
    fn the_documented_example_loads() {
        let config = Config::parse(
            r#"
version: 1
defaults:
  currency: USD
  monthly_hours: 730
aws:
  region: us-east-1
  pricing_mode: public
usage_profile: high
usage:
  aws:
    s3:
      storage_gb: 500
resource:
  "aws_s3_bucket.catalog":
    usage:
      storage_gb: 1000
budget:
  monthly: 2500
  warning_at: 0.8
execution:
  terraform_plan: false
rules:
  enabled: true
"#,
        )
        .unwrap();

        assert_eq!(config.defaults.monthly_hours, Some(dec!(730)));
        assert_eq!(config.budget.monthly, Some(dec!(2500)));
        assert_eq!(config.usage["aws"]["s3"]["storage_gb"], 500);
        assert_eq!(config.resource_usage()["aws_s3_bucket.catalog"]["storage_gb"], 1000);
    }

    #[test]
    fn empty_file_is_the_default_config() {
        assert!(Config::parse("").unwrap().usage_profile.is_none());
    }

    #[test]
    fn invalid_yaml_and_invalid_values_are_rejected_with_a_reason() {
        assert!(Config::parse("defaults: [unclosed").is_err());
        assert!(
            Config::parse("version: 2")
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
        assert!(Config::parse("defaults: {monthly_hours: 0}").is_err());
        assert!(Config::parse("defaults: {monthly_hours: 9000}").is_err());
        assert!(
            Config::parse("aws: {pricing_mode: account-aware}")
                .unwrap_err()
                .to_string()
                .contains("only `public`")
        );
        assert!(Config::parse("budget: {monthly: -5}").is_err());
    }
}
