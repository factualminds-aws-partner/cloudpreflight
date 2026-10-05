//! Provider-neutral resource and cost model shared by every layer.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single planned or existing resource, independent of the IaC tool that declared it.
#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    pub address: String,
    pub provider: String,
    pub resource_type: String,
    pub region: Option<String>,
    /// Attribute tree. Nested blocks are arrays of objects, as in Terraform plan JSON.
    pub attrs: Value,
    /// Dotted attribute path to the reason its value is not known.
    pub unknown: BTreeMap<String, String>,
    /// Top-level attribute name to the resource addresses (without index) it references.
    pub refs: BTreeMap<String, Vec<String>>,
    /// Assumptions made while building this resource (for example an unknown `count`).
    pub notes: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum Lookup<'a> {
    Known(&'a Value),
    Unknown(&'a str),
    Missing,
}

impl Resource {
    pub fn new(address: &str, resource_type: &str, attrs: Value) -> Self {
        Self {
            address: address.to_string(),
            provider: resource_type.split('_').next().unwrap_or_default().to_string(),
            resource_type: resource_type.to_string(),
            region: None,
            attrs,
            unknown: BTreeMap::new(),
            refs: BTreeMap::new(),
            notes: Vec::new(),
        }
    }

    /// Looks up a dotted path such as `root_block_device.0.volume_size`.
    pub fn get(&self, path: &str) -> Lookup<'_> {
        for (unknown_path, reason) in &self.unknown {
            let is_prefix = path
                .strip_prefix(unknown_path.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'));
            if is_prefix {
                return Lookup::Unknown(reason);
            }
        }

        let mut current = &self.attrs;
        for segment in path.split('.') {
            let next = match current {
                // Terraform JSON syntax may write a single nested block as a bare object.
                Value::Object(map) if segment == "0" && !map.contains_key("0") => Some(current),
                Value::Object(map) => map.get(segment),
                Value::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get(i)),
                _ => None,
            };
            match next {
                Some(value) => current = value,
                None => return Lookup::Missing,
            }
        }

        if current.is_null() {
            return Lookup::Missing;
        }

        Lookup::Known(current)
    }

    pub fn get_str(&self, path: &str) -> Option<&str> {
        match self.get(path) {
            Lookup::Known(Value::String(s)) => Some(s),
            _ => None,
        }
    }

    pub fn set_derived(&mut self, key: &str, value: impl Into<Value>) {
        if let Value::Object(map) = &mut self.attrs {
            map.insert(key.to_string(), value.into());
        }
    }

    /// Address without a trailing instance key: `a.b[0]` becomes `a.b`.
    pub fn base_address(&self) -> &str {
        strip_index(&self.address)
    }
}

pub fn strip_index(address: &str) -> &str {
    if address.ends_with(']')
        && let Some(open) = address.rfind('[')
    {
        return &address[..open];
    }
    address
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Update,
    Delete,
    Replace,
    NoOp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceChange {
    pub address: String,
    pub resource_type: String,
    pub action: Action,
    pub before: Option<Resource>,
    pub after: Option<Resource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IacSource {
    TerraformPlan,
    TerraformStatic,
}

impl IacSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::TerraformPlan => "Terraform (plan JSON)",
            Self::TerraformStatic => "Terraform (static analysis)",
        }
    }
}

/// Everything one IaC adapter extracted from one plan file or root module.
#[derive(Debug, Clone)]
pub struct Input {
    pub source: IacSource,
    pub origin: String,
    pub provider_regions: BTreeMap<String, String>,
    pub changes: Vec<ResourceChange>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Compute,
    Storage,
    Requests,
    DataTransfer,
    ProvisionedCapacity,
    Other,
}

/// How much trust a priced number deserves. Ordered from most to least certain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceKind {
    /// Every input came from the IaC and exactly one list price matched.
    Exact,
    /// A default or an ambiguous price match was used.
    Estimated,
    /// The quantity comes from a usage profile, not from the infrastructure.
    UsageAssumed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Priced { monthly: Decimal, kind: PriceKind },
    Unresolved { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComponentEstimate {
    pub name: String,
    pub category: Category,
    #[serde(flatten)]
    pub outcome: Outcome,
    pub quantity: Option<Decimal>,
    pub unit: Option<String>,
    pub sku: Option<String>,
    pub price_description: Option<String>,
    pub formula: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceStatus {
    Priced,
    PartiallyPriced,
    Unresolved,
    Unsupported,
    NoDirectCharge,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceEstimate {
    pub address: String,
    pub resource_type: String,
    pub service: String,
    pub region: Option<String>,
    pub status: ResourceStatus,
    /// Sum of priced components. `None` when nothing could be priced; never a stand-in zero.
    pub monthly: Option<Decimal>,
    pub components: Vec<ComponentEstimate>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChangeEstimate {
    pub address: String,
    pub resource_type: String,
    pub action: Action,
    pub before: Option<ResourceEstimate>,
    pub after: Option<ResourceEstimate>,
    pub delta: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Assumption {
    pub scope: String,
    pub key: String,
    pub value: String,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Severity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub rule_id: String,
    pub title: String,
    pub resource: String,
    pub reason: String,
    pub estimated_impact: Option<Decimal>,
    pub recommendation: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Totals {
    pub before: Decimal,
    pub after: Decimal,
    pub delta: Decimal,
    pub annual_after: Decimal,
    pub annual_delta: Decimal,
    /// `None` when there is no previous cost to compare against.
    pub delta_percent: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PricingSnapshot {
    pub provider: String,
    pub service: String,
    pub region: String,
    pub version: String,
    pub publication_date: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    pub address: String,
    pub resource_type: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BudgetStatus {
    pub monthly: Decimal,
    pub used_fraction: Decimal,
    pub exceeded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Counts {
    pub total: usize,
    pub priced: usize,
    pub partially_priced: usize,
    pub unresolved: usize,
    pub unsupported: usize,
    pub no_direct_charge: usize,
}

/// The complete, versioned result of a scan. Serialised as the JSON output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub tool_version: String,
    pub schema_version: String,
    pub scan_time: String,
    pub project: String,
    pub iac: Vec<String>,
    /// The plan files and root modules the estimate was built from.
    pub sources: Vec<String>,
    pub clouds: Vec<String>,
    pub regions: Vec<String>,
    pub currency: String,
    pub pricing_source: String,
    pub pricing: Vec<PricingSnapshot>,
    pub monthly_hours: Decimal,
    pub usage_profile: Option<String>,
    pub has_baseline: bool,
    pub counts: Counts,
    pub costs: Totals,
    pub resources: Vec<ChangeEstimate>,
    pub assumptions: Vec<Assumption>,
    pub findings: Vec<Finding>,
    pub unsupported_resources: Vec<Skipped>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub confidence: Confidence,
    pub budget: Option<BudgetStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lookup_distinguishes_known_unknown_and_missing() {
        let mut resource = Resource::new(
            "aws_instance.web",
            "aws_instance",
            json!({"instance_type": "t3.micro", "root_block_device": [{"volume_size": 20}], "ami": null}),
        );
        resource
            .unknown
            .insert("ebs_block_device".into(), "known after apply".into());

        assert_eq!(
            resource.get("root_block_device.0.volume_size"),
            Lookup::Known(&json!(20))
        );
        assert_eq!(resource.get("ami"), Lookup::Missing);
        assert_eq!(resource.get("nope.deeper"), Lookup::Missing);
        assert_eq!(
            resource.get("ebs_block_device.0.volume_size"),
            Lookup::Unknown("known after apply")
        );
        assert_eq!(resource.provider, "aws");
    }

    #[test]
    fn strip_index_handles_count_and_for_each_keys() {
        assert_eq!(strip_index("module.a[0].aws_x.y[\"k\"]"), "module.a[0].aws_x.y");
        assert_eq!(strip_index("aws_x.y[3]"), "aws_x.y");
        assert_eq!(strip_index("aws_x.y"), "aws_x.y");
    }
}
