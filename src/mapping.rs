//! Data-driven resource-to-price mappings, embedded from `data/`.
//! Adding a resource normally means adding one YAML file and one line here.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::model::Category;

const AWS_RESOURCES: &[&str] = &[
    include_str!("../data/aws/resources/aws_instance.yaml"),
    include_str!("../data/aws/resources/aws_ebs_volume.yaml"),
    include_str!("../data/aws/resources/aws_db_instance.yaml"),
    include_str!("../data/aws/resources/aws_lb.yaml"),
    include_str!("../data/aws/resources/aws_nat_gateway.yaml"),
    include_str!("../data/aws/resources/aws_ecs_service.yaml"),
    include_str!("../data/aws/resources/aws_lambda_function.yaml"),
    include_str!("../data/aws/resources/aws_s3_bucket.yaml"),
    include_str!("../data/aws/resources/aws_cloudfront_distribution.yaml"),
    include_str!("../data/aws/resources/aws_dynamodb_table.yaml"),
    include_str!("../data/aws/resources/aws_elasticache.yaml"),
];
const AWS_NO_CHARGE: &str = include_str!("../data/aws/no_charge.yaml");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub resource_types: Vec<String>,
    /// Human label used for grouping, for example `RDS`.
    pub service: String,
    /// Usage profile group, for example `aws.s3`.
    pub usage_group: Option<String>,
    /// Stated once per resource in the report.
    #[serde(default)]
    pub notes: Vec<String>,
    pub components: Vec<ComponentSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentSpec {
    pub name: String,
    pub category: Category,
    /// Provider price list service code, for example `AmazonEC2`.
    pub price_service: String,
    /// Price list region override for global services.
    pub price_region: Option<String>,
    pub filters: BTreeMap<String, String>,
    /// Unit the quantity is expressed in. Must agree with the unit of the matched price.
    pub unit: String,
    pub quantity: Vec<Factor>,
    pub when: Option<When>,
    /// Units included at no charge per resource (for example gp3 baseline IOPS).
    pub free_units: Option<Decimal>,
    /// An assumption this component always makes. Marks the result as estimated.
    pub assumes: Option<String>,
}

/// One multiplicand of a quantity.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Factor {
    /// `hours`: the configured hours per month.
    Named(String),
    Spec(FactorSpec),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactorSpec {
    pub attr: Option<String>,
    pub usage: Option<String>,
    #[serde(rename = "const")]
    pub constant: Option<Decimal>,
    /// Used when `attr` is not set in the IaC. Marks the result as estimated.
    pub default: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct When {
    pub attr: String,
    #[serde(rename = "in", default)]
    pub any_of: Vec<String>,
    #[serde(default)]
    pub not_in: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NoCharge {
    resource_types: BTreeSet<String>,
}

#[derive(Debug)]
pub struct Mappings {
    by_type: BTreeMap<String, Mapping>,
    no_charge: BTreeSet<String>,
}

impl Mappings {
    pub fn load() -> Result<Self> {
        let mut by_type = BTreeMap::new();
        for source in AWS_RESOURCES {
            let mapping: Mapping = serde_saphyr::from_str(source).context("embedded resource mapping is invalid")?;
            for resource_type in &mapping.resource_types {
                by_type.insert(resource_type.clone(), mapping.clone());
            }
        }

        let no_charge: NoCharge =
            serde_saphyr::from_str(AWS_NO_CHARGE).context("embedded no_charge list is invalid")?;

        Ok(Self {
            by_type,
            no_charge: no_charge.resource_types,
        })
    }

    pub fn get(&self, resource_type: &str) -> Option<&Mapping> {
        self.by_type.get(resource_type)
    }

    pub fn is_no_charge(&self, resource_type: &str) -> bool {
        self.no_charge.contains(resource_type)
    }

    pub fn supported_types(&self) -> impl Iterator<Item = (&str, &str)> {
        self.by_type
            .iter()
            .map(|(resource_type, mapping)| (resource_type.as_str(), mapping.service.as_str()))
    }

    pub fn no_charge_count(&self) -> usize {
        self.no_charge.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_mappings_load_and_are_well_formed() {
        let mappings = Mappings::load().unwrap();
        assert!(mappings.get("aws_instance").is_some());
        assert!(mappings.is_no_charge("aws_iam_role"));

        for (resource_type, _) in mappings.supported_types() {
            let mapping = mappings.get(resource_type).unwrap();
            assert!(
                !mappings.is_no_charge(resource_type),
                "{resource_type} is in both lists"
            );
            for component in &mapping.components {
                assert!(!component.quantity.is_empty(), "{resource_type}: empty quantity");
                for factor in &component.quantity {
                    match factor {
                        Factor::Named(name) => assert_eq!(name, "hours", "{resource_type}"),
                        Factor::Spec(spec) => {
                            let set = [spec.attr.is_some(), spec.usage.is_some(), spec.constant.is_some()];
                            assert_eq!(
                                set.iter().filter(|s| **s).count(),
                                1,
                                "{resource_type}/{}: a factor needs exactly one of attr, usage, const",
                                component.name
                            );
                            if spec.usage.is_some() {
                                assert!(
                                    mapping.usage_group.is_some(),
                                    "{resource_type}: usage needs usage_group"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
