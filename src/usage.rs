//! Usage resolution. Infrastructure alone cannot say how much traffic a resource sees,
//! so every usage value carries the name of where it came from.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

use crate::model::strip_index;

const PROFILES: &[&str] = &[
    include_str!("../data/usage/light.yaml"),
    include_str!("../data/usage/standard.yaml"),
    include_str!("../data/usage/high.yaml"),
];
pub const DEFAULT_PROFILE: &str = "standard";

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub name: String,
    pub description: String,
    pub usage: Value,
}

pub fn builtin_profiles() -> Result<Vec<Profile>> {
    PROFILES
        .iter()
        .map(|source| serde_saphyr::from_str(source).context("embedded usage profile is invalid"))
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    profile: Option<Profile>,
    profile_is_default: bool,
    /// The `usage:` block of the config file, shaped like a profile's `usage`.
    config: Value,
    /// Resource address to usage key to value.
    overrides: BTreeMap<String, BTreeMap<String, Value>>,
}

impl Usage {
    pub fn new(
        profile_name: Option<&str>,
        config: Value,
        overrides: BTreeMap<String, BTreeMap<String, Value>>,
    ) -> Result<Self> {
        let wanted = profile_name.unwrap_or(DEFAULT_PROFILE);
        let profiles = builtin_profiles()?;
        let Some(profile) = profiles.iter().find(|p| p.name == wanted).cloned() else {
            let names: Vec<_> = profiles.iter().map(|p| p.name.as_str()).collect();
            bail!(
                "unknown usage profile `{wanted}`. Available profiles: {}",
                names.join(", ")
            );
        };

        // An unusable value must stop the run: skipping it would silently fall back to
        // the profile's number.
        validate(&config, "usage")?;
        for (address, usage) in &overrides {
            for (key, value) in usage {
                if to_decimal(value).is_none() {
                    bail!("resource.\"{address}\".usage.{key}: `{value}` is not a non-negative number within range");
                }
            }
        }

        Ok(Self {
            profile: Some(profile),
            profile_is_default: profile_name.is_none(),
            config,
            overrides,
        })
    }

    pub fn profile_label(&self) -> Option<String> {
        let profile = self.profile.as_ref()?;
        if self.profile_is_default {
            return Some(format!("{} (default)", profile.name));
        }
        Some(profile.name.clone())
    }

    /// Returns the value and a description of its source, most specific source first.
    pub fn get(&self, address: &str, group: &str, key: &str) -> Option<(Decimal, String)> {
        let by_resource = self
            .overrides
            .get(address)
            .or_else(|| self.overrides.get(strip_index(address)))
            .and_then(|usage| usage.get(key))
            .and_then(to_decimal);
        if let Some(value) = by_resource {
            return Some((value, "config: resource override".to_string()));
        }

        if let Some(value) = nested(&self.config, group, key) {
            return Some((value, "config: usage".to_string()));
        }

        let profile = self.profile.as_ref()?;
        let value = nested(&profile.usage, group, key)?;
        Some((value, format!("profile: {}", self.profile_label()?)))
    }
}

fn validate(value: &Value, path: &str) -> Result<()> {
    match value {
        Value::Null => Ok(()),
        Value::Object(map) => map
            .iter()
            .try_for_each(|(key, value)| validate(value, &format!("{path}.{key}"))),
        leaf if to_decimal(leaf).is_some() => Ok(()),
        leaf => bail!("{path}: `{leaf}` is not a non-negative number within range"),
    }
}

fn nested(root: &Value, group: &str, key: &str) -> Option<Decimal> {
    let mut current = root;
    for segment in group.split('.') {
        current = current.get(segment)?;
    }
    to_decimal(current.get(key)?)
}

/// Converts through the number's text so `0.1` stays `0.1` rather than a binary float.
fn to_decimal(value: &Value) -> Option<Decimal> {
    let text = match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => return None,
    };
    let parsed = Decimal::from_str(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .ok()?;
    (!parsed.is_sign_negative()).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;
    use serde_json::json;

    #[test]
    fn most_specific_source_wins() {
        let overrides = BTreeMap::from([(
            "aws_s3_bucket.catalog".to_string(),
            BTreeMap::from([("storage_gb".to_string(), json!(1000))]),
        )]);
        let config = json!({"aws": {"s3": {"storage_gb": 750, "requests_monthly": 42}}});
        let usage = Usage::new(None, config, overrides).unwrap();

        let (value, source) = usage.get("aws_s3_bucket.catalog[0]", "aws.s3", "storage_gb").unwrap();
        assert_eq!((value, source.as_str()), (dec!(1000), "config: resource override"));

        let (value, source) = usage.get("aws_s3_bucket.other", "aws.s3", "storage_gb").unwrap();
        assert_eq!((value, source.as_str()), (dec!(750), "config: usage"));

        let (value, source) = usage
            .get("aws_s3_bucket.other", "aws.s3", "write_requests_monthly")
            .unwrap();
        assert_eq!((value, source.as_str()), (dec!(1000000), "profile: standard (default)"));

        assert_eq!(usage.get("aws_s3_bucket.other", "aws.s3", "nope"), None);
    }

    #[test]
    fn unknown_profile_lists_the_available_ones() {
        let error = Usage::new(Some("enormous"), Value::Null, BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("light, standard, high"));
    }

    #[test]
    fn unusable_config_values_stop_the_run_instead_of_falling_back_to_the_profile() {
        for bad in [json!("abc"), json!(-1), json!(1e40), json!([1])] {
            let config = json!({"aws": {"s3": {"storage_gb": bad}}});
            let error = Usage::new(None, config, BTreeMap::new()).unwrap_err().to_string();
            assert!(error.starts_with("usage.aws.s3.storage_gb:"), "{error}");
        }

        let overrides = BTreeMap::from([(
            "a.b".to_string(),
            BTreeMap::from([("storage_gb".to_string(), json!("x"))]),
        )]);
        assert!(Usage::new(None, Value::Null, overrides).is_err());
    }

    #[test]
    fn negative_and_non_numeric_usage_is_rejected() {
        assert_eq!(to_decimal(&json!(-5)), None);
        assert_eq!(to_decimal(&json!("abc")), None);
        assert_eq!(to_decimal(&json!(0.1)), Some(dec!(0.1)));
    }
}
