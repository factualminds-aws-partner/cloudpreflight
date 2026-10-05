//! The cost engine. Pure arithmetic over the normalised model: it never touches the
//! network and does not know which IaC tool produced a resource.

use std::collections::BTreeSet;
use std::str::FromStr;

use rust_decimal::Decimal;
use serde_json::Value;

use crate::mapping::{ComponentSpec, Factor, Mapping, Mappings, When};
use crate::model::{
    Assumption, ChangeEstimate, ComponentEstimate, Lookup, Outcome, PriceKind, Resource, ResourceChange,
    ResourceEstimate, ResourceStatus, Totals,
};
use crate::pricing::{Price, PriceQuery, PriceResult, Prices, Tier};
use crate::usage::Usage;

pub struct Engine<'a> {
    pub mappings: &'a Mappings,
    pub usage: &'a Usage,
    pub monthly_hours: Decimal,
}

enum Planned {
    /// The component does not apply to this resource.
    Skip,
    Unresolved(String),
    Query(PriceQuery),
}

struct Quantity {
    value: Decimal,
    parts: Vec<String>,
    usage_based: bool,
    notes: Vec<String>,
}

impl Engine<'_> {
    /// Every price the given resources need, deduplicated.
    pub fn queries<'r>(&self, resources: impl Iterator<Item = &'r Resource>) -> BTreeSet<PriceQuery> {
        let mut queries = BTreeSet::new();
        for resource in resources {
            let Some(mapping) = self.mappings.get(&resource.resource_type) else {
                continue;
            };
            for spec in &mapping.components {
                if let Planned::Query(query) = self.plan(resource, spec) {
                    queries.insert(query);
                }
            }
        }
        queries
    }

    pub fn estimate(
        &self,
        resource: &Resource,
        prices: &Prices,
        assumptions: &mut BTreeSet<Assumption>,
    ) -> ResourceEstimate {
        let mut estimate = ResourceEstimate {
            address: resource.address.clone(),
            resource_type: resource.resource_type.clone(),
            service: String::new(),
            region: resource.region.clone(),
            status: ResourceStatus::Unsupported,
            monthly: None,
            components: Vec::new(),
            notes: resource.notes.clone(),
        };

        let Some(mapping) = self.mappings.get(&resource.resource_type) else {
            if self.mappings.is_no_charge(&resource.resource_type) {
                estimate.status = ResourceStatus::NoDirectCharge;
            }
            return estimate;
        };
        estimate.service = mapping.service.clone();
        estimate.notes.extend(mapping.notes.iter().cloned());

        for spec in &mapping.components {
            let planned = self.plan(resource, spec);
            if matches!(planned, Planned::Skip) {
                continue;
            }
            let component = self.component(resource, mapping, spec, planned, prices, assumptions);
            estimate.components.push(component);
        }

        let priced: Vec<Decimal> = estimate
            .components
            .iter()
            .filter_map(|component| match component.outcome {
                Outcome::Priced { monthly, .. } => Some(monthly),
                Outcome::Unresolved { .. } => None,
            })
            .collect();

        estimate.status = match (priced.len(), estimate.components.len()) {
            (_, 0) => ResourceStatus::NoDirectCharge,
            (0, _) => ResourceStatus::Unresolved,
            (p, all) if p == all => ResourceStatus::Priced,
            _ => ResourceStatus::PartiallyPriced,
        };
        if !priced.is_empty() {
            estimate.monthly = Some(priced.iter().sum());
        }

        estimate
    }

    fn plan(&self, resource: &Resource, spec: &ComponentSpec) -> Planned {
        if let Some(when) = &spec.when {
            match applies(resource, when) {
                Ok(true) => {}
                Ok(false) => return Planned::Skip,
                Err(reason) => return Planned::Unresolved(reason),
            }
        }

        let region = match (&spec.price_region, &resource.region) {
            (Some(region), _) | (None, Some(region)) => region.clone(),
            (None, None) => return Planned::Unresolved("the resource region is not known".into()),
        };

        let mut filters = std::collections::BTreeMap::new();
        for (key, template) in &spec.filters {
            match substitute(template, resource) {
                Ok(value) => filters.insert(key.clone(), value),
                Err(reason) => return Planned::Unresolved(reason),
            };
        }

        Planned::Query(PriceQuery {
            provider: resource.provider.clone(),
            service: spec.price_service.clone(),
            region,
            filters,
        })
    }

    fn component(
        &self,
        resource: &Resource,
        mapping: &Mapping,
        spec: &ComponentSpec,
        planned: Planned,
        prices: &Prices,
        assumptions: &mut BTreeSet<Assumption>,
    ) -> ComponentEstimate {
        let mut component = ComponentEstimate {
            name: spec.name.clone(),
            category: spec.category,
            outcome: Outcome::Unresolved { reason: String::new() },
            quantity: None,
            unit: Some(spec.unit.clone()),
            sku: None,
            price_description: None,
            formula: None,
            notes: Vec::new(),
        };
        let unresolved = |mut component: ComponentEstimate, reason: String| {
            component.outcome = Outcome::Unresolved { reason };
            component
        };

        let query = match planned {
            Planned::Query(query) => query,
            Planned::Unresolved(reason) => return unresolved(component, reason),
            Planned::Skip => unreachable!("skipped components are filtered out by the caller"),
        };

        let quantity = match self.quantity(resource, mapping, spec, assumptions) {
            Ok(quantity) => quantity,
            Err(reason) => return unresolved(component, reason),
        };
        component.quantity = Some(quantity.value);
        component.notes = quantity.notes;

        let price = match prices.get(&query) {
            Some(PriceResult::Found(price)) => price,
            Some(PriceResult::Unavailable { reason }) => return unresolved(component, reason.clone()),
            Some(PriceResult::NotFound) | None => {
                let filters: Vec<String> = query.filters.iter().map(|(k, v)| format!("{k}={v}")).collect();
                let reason = format!(
                    "no {} list price in {} matches {}. Check the value is valid for this region.",
                    query.service,
                    query.region,
                    filters.join(", ")
                );
                return unresolved(component, reason);
            }
        };
        component.sku = Some(price.sku.clone());
        component.price_description = Some(price.description.clone());

        if canonical_unit(&price.unit) != canonical_unit(&spec.unit) {
            let reason = format!(
                "mapping expects unit `{}` but the list price is per `{}`; refusing to mix units",
                spec.unit, price.unit
            );
            return unresolved(component, reason);
        }

        let free = spec.free_units.unwrap_or_default();
        let billable = (quantity.value - free).max(Decimal::ZERO);
        let Some(monthly) = tiered_cost(billable, &price.tiers) else {
            return unresolved(component, "the cost is too large to represent".into());
        };

        let mut kind = PriceKind::Exact;
        if let Some(assumption) = &spec.assumes {
            kind = PriceKind::Estimated;
            component.notes.push(format!("Assumes {assumption}."));
        }
        if price.candidates > 1 {
            kind = PriceKind::Estimated;
            component.notes.push(format!(
                "{} differently priced SKUs matched; using {}.",
                price.candidates, price.sku
            ));
        }
        if !resource.notes.is_empty() {
            kind = PriceKind::Estimated;
        }
        if quantity.usage_based {
            kind = PriceKind::UsageAssumed;
        }
        if let Some(note) = free_allowance_note(billable, price) {
            component.notes.push(note);
        }

        component.formula = Some(formula(&quantity.parts, quantity.value, free, price));
        component.outcome = Outcome::Priced { monthly, kind };
        component
    }

    fn quantity(
        &self,
        resource: &Resource,
        mapping: &Mapping,
        spec: &ComponentSpec,
        assumptions: &mut BTreeSet<Assumption>,
    ) -> Result<Quantity, String> {
        let mut quantity = Quantity {
            value: Decimal::ONE,
            parts: Vec::new(),
            usage_based: false,
            notes: Vec::new(),
        };

        for factor in &spec.quantity {
            let (value, label) = match factor {
                Factor::Named(name) if name == "hours" => {
                    if !canonical_unit(&spec.unit).contains("hour") {
                        return Err(format!(
                            "mapping error: hours per month cannot multiply a `{}` price",
                            spec.unit
                        ));
                    }
                    (self.monthly_hours, format!("{} hours", plain(self.monthly_hours)))
                }
                Factor::Named(name) => return Err(format!("mapping error: unknown factor `{name}`")),
                Factor::Spec(factor) => {
                    if let Some(constant) = factor.constant {
                        (constant, plain(constant))
                    } else if let Some(key) = &factor.usage {
                        let group = mapping.usage_group.as_deref().unwrap_or_default();
                        let Some((value, source)) = self.usage.get(&resource.address, group, key) else {
                            return Err(format!(
                                "no usage value for `{group}.{key}`. Set it under `usage:` in the config file."
                            ));
                        };
                        assumptions.insert(Assumption {
                            scope: resource.address.clone(),
                            key: key.clone(),
                            value: plain(value),
                            source,
                        });
                        quantity.usage_based = true;
                        (value, format!("{} {key}", plain(value)))
                    } else if let Some(path) = &factor.attr {
                        let name = path.trim_start_matches('_');
                        match resource.get(path) {
                            Lookup::Known(value) => {
                                let Some(number) = non_negative(value) else {
                                    return Err(format!("{name} is not a non-negative number: {value}"));
                                };
                                (number, format!("{} {name}", plain(number)))
                            }
                            Lookup::Unknown(reason) => return Err(format!("{name} is not known: {reason}")),
                            Lookup::Missing => match factor.default {
                                Some(default) => {
                                    quantity
                                        .notes
                                        .push(format!("{name} is not set; provider default {} used.", plain(default)));
                                    (default, format!("{} {name}", plain(default)))
                                }
                                None => return Err(format!("{name} is not set in the configuration")),
                            },
                        }
                    } else {
                        return Err("mapping error: empty quantity factor".into());
                    }
                }
            };

            quantity.value = quantity
                .value
                .checked_mul(value)
                .ok_or("the quantity is too large to represent")?;
            quantity.parts.push(label);
        }

        Ok(quantity)
    }
}

fn applies(resource: &Resource, when: &When) -> Result<bool, String> {
    let actual = match resource.get(&when.attr) {
        Lookup::Known(value) => scalar(value),
        Lookup::Missing => String::new(),
        Lookup::Unknown(reason) => {
            return Err(format!("{} is not known: {reason}", when.attr.trim_start_matches('_')));
        }
    };

    if !when.any_of.is_empty() && !when.any_of.contains(&actual) {
        return Ok(false);
    }
    Ok(!when.not_in.contains(&actual))
}

/// Expands `${path}` and `${path:-default}` against the resource's attributes.
fn substitute(template: &str, resource: &Resource) -> Result<String, String> {
    let mut output = String::new();
    let mut rest = template;

    while let Some(start) = rest.find("${") {
        let Some(length) = rest[start..].find('}') else {
            return Err(format!("mapping error: unterminated placeholder in `{template}`"));
        };
        output.push_str(&rest[..start]);

        let inner = &rest[start + 2..start + length];
        let (path, default) = match inner.split_once(":-") {
            Some((path, default)) => (path, Some(default)),
            None => (inner, None),
        };
        let name = path.trim_start_matches('_');
        match (resource.get(path), default) {
            (Lookup::Known(value), _) => output.push_str(&scalar(value)),
            (Lookup::Unknown(reason), _) => return Err(format!("{name} is not known: {reason}")),
            (Lookup::Missing, Some(default)) => output.push_str(default),
            (Lookup::Missing, None) => return Err(format!("{name} is not set in the configuration")),
        }

        rest = &rest[start + length + 1..];
    }

    output.push_str(rest);
    Ok(output)
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn non_negative(value: &Value) -> Option<Decimal> {
    let text = scalar(value);
    let number = Decimal::from_str(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .ok()?;
    (!number.is_sign_negative()).then_some(number)
}

/// Cost of `quantity` units under graduated tiers `[begin, end)`.
/// Returns `None` on overflow rather than a wrong number.
pub fn tiered_cost(quantity: Decimal, tiers: &[Tier]) -> Option<Decimal> {
    let mut total = Decimal::ZERO;
    for tier in tiers {
        if quantity <= tier.begin {
            continue;
        }
        let upper = tier.end.map_or(quantity, |end| end.min(quantity));
        let portion = (upper - tier.begin).checked_mul(tier.price)?;
        total = total.checked_add(portion)?;
    }
    Some(total)
}

/// Normalises unit spellings so that `Hrs`, `hours` and `Hour` compare equal.
/// Deliberately does not convert between different units.
pub fn canonical_unit(unit: &str) -> String {
    unit.to_ascii_lowercase()
        .split('-')
        .map(|part| match part {
            "hrs" | "hr" | "hour" | "hours" => "hour",
            "mo" | "month" | "months" => "month",
            "request" | "requests" => "request",
            "second" | "seconds" => "second",
            other => other,
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn free_allowance_note(billable: Decimal, price: &Price) -> Option<String> {
    let free_tier = price
        .tiers
        .iter()
        .find(|tier| tier.price.is_zero() && tier.end.is_some() && billable > tier.begin)?;
    Some(format!(
        "Includes the free allowance of {} {} from the price list. It is account-wide; it is applied per resource here.",
        plain(free_tier.end?),
        price.unit
    ))
}

fn formula(parts: &[String], quantity: Decimal, free: Decimal, price: &Price) -> String {
    let mut text = parts.join(" × ");
    if parts.len() > 1 {
        text.push_str(&format!(" = {} {}", plain(quantity), price.unit));
    }
    if !free.is_zero() {
        text.push_str(&format!(", less {} included", plain(free)));
    }

    if let [tier] = price.tiers.as_slice() {
        text.push_str(&format!(" × ${} per {}", plain(tier.price), price.unit));
        return text;
    }

    let tiers: Vec<String> = price
        .tiers
        .iter()
        .map(|tier| match tier.end {
            Some(end) => format!("{}–{} at ${}", plain(tier.begin), plain(end), plain(tier.price)),
            None => format!("above {} at ${}", plain(tier.begin), plain(tier.price)),
        })
        .collect();
    text.push_str(&format!(", tiered per {}: {}", price.unit, tiers.join("; ")));
    text
}

/// Decimal without trailing zeros.
pub fn plain(value: Decimal) -> String {
    value.normalize().to_string()
}

/// Pairs a change with its estimates and computes a like-for-like delta.
pub fn change_estimate(
    change: &ResourceChange,
    before: Option<ResourceEstimate>,
    after: Option<ResourceEstimate>,
) -> ChangeEstimate {
    let (delta, _) = like_for_like(&before, &after);
    ChangeEstimate {
        address: change.address.clone(),
        resource_type: change.resource_type.clone(),
        action: change.action,
        before,
        after,
        delta,
    }
}

/// Delta between the two sides, and whether any component had to be left out.
///
/// When a resource exists on both sides, a component counts only if it is priced on
/// both sides or exists on just one. Subtracting an unpriced component from a priced
/// one would invent a saving or a cost that is not there.
fn like_for_like(before: &Option<ResourceEstimate>, after: &Option<ResourceEstimate>) -> (Decimal, bool) {
    let (Some(before), Some(after)) = (before, after) else {
        let monthly = |side: &Option<ResourceEstimate>| side.as_ref().and_then(|e| e.monthly).unwrap_or_default();
        return (monthly(after) - monthly(before), false);
    };

    let priced = |component: &ComponentEstimate| match component.outcome {
        Outcome::Priced { monthly, .. } => Some(monthly),
        Outcome::Unresolved { .. } => None,
    };
    let one_side = |side: &ResourceEstimate, other: &ResourceEstimate| {
        let mut sum = Decimal::ZERO;
        let mut left_out = false;
        for component in &side.components {
            let counterpart = other
                .components
                .iter()
                .find(|candidate| candidate.name == component.name);
            match (priced(component), counterpart.map(priced)) {
                (Some(monthly), None | Some(Some(_))) => sum += monthly,
                _ => left_out = true,
            }
        }
        (sum, left_out)
    };

    let (before_sum, before_left_out) = one_side(before, after);
    let (after_sum, after_left_out) = one_side(after, before);
    (after_sum - before_sum, before_left_out || after_left_out)
}

/// `before` and `after` are everything that could be priced on each side. `delta` is
/// like-for-like, so it can differ from `after - before` when a component is priced
/// on one side only; `partial_deltas` lists those changes.
pub fn totals(changes: &[ChangeEstimate]) -> Totals {
    let monthly = |side: &Option<ResourceEstimate>| side.as_ref().and_then(|e| e.monthly).unwrap_or_default();
    let before: Decimal = changes.iter().map(|change| monthly(&change.before)).sum();
    let after: Decimal = changes.iter().map(|change| monthly(&change.after)).sum();
    let delta: Decimal = changes.iter().map(|change| change.delta).sum();

    let twelve = Decimal::from(12);
    Totals {
        before,
        after,
        delta,
        annual_after: after * twelve,
        annual_delta: delta * twelve,
        delta_percent: (!before.is_zero()).then(|| delta / before * Decimal::ONE_HUNDRED),
    }
}

/// Changes whose delta leaves out a component that is priced on one side only.
pub fn partial_deltas(changes: &[ChangeEstimate]) -> Vec<&ChangeEstimate> {
    changes
        .iter()
        .filter(|change| like_for_like(&change.before, &change.after).1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Action;
    use proptest::prelude::*;
    use rust_decimal::dec;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn tier(begin: Decimal, end: Option<Decimal>, price: Decimal) -> Tier {
        Tier { begin, end, price }
    }

    fn s3_tiers() -> Vec<Tier> {
        vec![
            tier(dec!(0), Some(dec!(51200)), dec!(0.023)),
            tier(dec!(51200), Some(dec!(512000)), dec!(0.022)),
            tier(dec!(512000), None, dec!(0.021)),
        ]
    }

    fn price(unit: &str, tiers: Vec<Tier>) -> PriceResult {
        PriceResult::Found(Price {
            sku: "SKU1".into(),
            description: "test price".into(),
            unit: unit.into(),
            tiers,
            candidates: 1,
        })
    }

    #[test]
    fn tier_boundaries() {
        assert_eq!(tiered_cost(dec!(0), &s3_tiers()), Some(dec!(0)));
        assert_eq!(tiered_cost(dec!(51200), &s3_tiers()), Some(dec!(1177.6)));
        assert_eq!(tiered_cost(dec!(51201), &s3_tiers()), Some(dec!(1177.622)));
        assert_eq!(
            tiered_cost(dec!(600000), &s3_tiers()),
            Some(dec!(1177.6) + dec!(460800) * dec!(0.022) + dec!(88000) * dec!(0.021))
        );
    }

    #[test]
    fn free_tier_boundary() {
        let tiers = vec![
            tier(dec!(0), Some(dec!(18600)), dec!(0)),
            tier(dec!(18600), None, dec!(0.00013)),
        ];
        assert_eq!(tiered_cost(dec!(18600), &tiers), Some(dec!(0)));
        assert_eq!(tiered_cost(dec!(18601), &tiers), Some(dec!(0.00013)));
    }

    #[test]
    fn overflow_is_reported_not_wrapped() {
        let tiers = vec![tier(dec!(0), None, Decimal::MAX)];
        assert_eq!(tiered_cost(dec!(2), &tiers), None);
    }

    #[test]
    fn unit_spellings_compare_equal_but_units_are_not_converted() {
        assert_eq!(canonical_unit("Hrs"), canonical_unit("hours"));
        assert_eq!(canonical_unit("GB-Mo"), canonical_unit("GB-Month"));
        assert_eq!(canonical_unit("LCU-Hrs"), "lcu-hour");
        assert_ne!(canonical_unit("GB-Mo"), canonical_unit("GB"));
    }

    proptest! {
        #[test]
        fn tiered_cost_is_monotonic_and_bounded(a in 0u64..10_000_000, b in 0u64..10_000_000) {
            let (low, high) = (Decimal::from(a.min(b)), Decimal::from(a.max(b)));
            let cost_low = tiered_cost(low, &s3_tiers()).unwrap();
            let cost_high = tiered_cost(high, &s3_tiers()).unwrap();
            prop_assert!(cost_low <= cost_high);
            prop_assert!(cost_high <= high * dec!(0.023));
            prop_assert!(cost_high >= high * dec!(0.021));
        }

        #[test]
        fn splitting_a_flat_rate_quantity_never_changes_the_total(a in 0u64..1_000_000_000, b in 0u64..1_000_000_000) {
            let flat = vec![tier(dec!(0), None, dec!(0.0000166667))];
            let whole = tiered_cost(Decimal::from(a) + Decimal::from(b), &flat).unwrap();
            let parts = tiered_cost(Decimal::from(a), &flat).unwrap() + tiered_cost(Decimal::from(b), &flat).unwrap();
            prop_assert_eq!(whole, parts);
        }
    }

    fn engine_fixture() -> (Mappings, Usage) {
        (
            Mappings::load().unwrap(),
            Usage::new(None, Value::Null, BTreeMap::new()).unwrap(),
        )
    }

    fn nat_gateway() -> Resource {
        let mut resource = Resource::new("aws_nat_gateway.main", "aws_nat_gateway", json!({}));
        resource.region = Some("us-east-1".into());
        resource
    }

    fn priced_all(engine: &Engine, resource: &Resource, unit_for: impl Fn(&PriceQuery) -> PriceResult) -> Prices {
        engine
            .queries(std::iter::once(resource))
            .into_iter()
            .map(|query| {
                let result = unit_for(&query);
                (query, result)
            })
            .collect()
    }

    #[test]
    fn hourly_and_usage_components_are_priced_and_labelled() {
        let (mappings, usage) = engine_fixture();
        let engine = Engine {
            mappings: &mappings,
            usage: &usage,
            monthly_hours: dec!(730),
        };
        let resource = nat_gateway();
        let prices = priced_all(&engine, &resource, |query| {
            if query.filters["usagetype"].ends_with("Hours") {
                price("Hrs", vec![tier(dec!(0), None, dec!(0.045))])
            } else {
                price("GB", vec![tier(dec!(0), None, dec!(0.045))])
            }
        });

        let mut assumptions = BTreeSet::new();
        let estimate = engine.estimate(&resource, &prices, &mut assumptions);

        assert_eq!(estimate.status, ResourceStatus::Priced);
        assert_eq!(estimate.monthly, Some(dec!(32.85) + dec!(22.5)));
        assert_eq!(
            estimate.components[0].outcome,
            Outcome::Priced {
                monthly: dec!(32.850),
                kind: PriceKind::Exact
            }
        );
        assert!(matches!(
            estimate.components[1].outcome,
            Outcome::Priced {
                kind: PriceKind::UsageAssumed,
                ..
            }
        ));
        assert_eq!(assumptions.len(), 1);
        assert_eq!(assumptions.first().unwrap().source, "profile: standard (default)");
    }

    #[test]
    fn missing_price_is_unresolved_with_an_actionable_reason_never_zero() {
        let (mappings, usage) = engine_fixture();
        let engine = Engine {
            mappings: &mappings,
            usage: &usage,
            monthly_hours: dec!(730),
        };
        let resource = nat_gateway();
        let prices = priced_all(&engine, &resource, |_| PriceResult::NotFound);

        let estimate = engine.estimate(&resource, &prices, &mut BTreeSet::new());

        assert_eq!(estimate.status, ResourceStatus::Unresolved);
        assert_eq!(estimate.monthly, None);
        let Outcome::Unresolved { reason } = &estimate.components[0].outcome else {
            panic!("expected unresolved");
        };
        assert!(
            reason.contains("no AmazonEC2 list price in us-east-1 matches"),
            "{reason}"
        );
    }

    #[test]
    fn a_monthly_price_is_never_multiplied_by_hours() {
        let (mappings, usage) = engine_fixture();
        let engine = Engine {
            mappings: &mappings,
            usage: &usage,
            monthly_hours: dec!(730),
        };
        let resource = nat_gateway();
        let prices = priced_all(&engine, &resource, |_| {
            price("GB-Mo", vec![tier(dec!(0), None, dec!(1))])
        });

        let estimate = engine.estimate(&resource, &prices, &mut BTreeSet::new());

        assert_eq!(estimate.monthly, None);
        let Outcome::Unresolved { reason } = &estimate.components[0].outcome else {
            panic!("expected unresolved");
        };
        assert!(reason.contains("refusing to mix units"), "{reason}");
    }

    #[test]
    fn unknown_attribute_is_unresolved_and_unmapped_types_are_unsupported() {
        let (mappings, usage) = engine_fixture();
        let engine = Engine {
            mappings: &mappings,
            usage: &usage,
            monthly_hours: dec!(730),
        };

        let mut instance = Resource::new("aws_instance.web", "aws_instance", json!({"_tenancy": "Shared"}));
        instance.region = Some("us-east-1".into());
        instance
            .unknown
            .insert("instance_type".into(), "known only after apply".into());
        let estimate = engine.estimate(&instance, &Prices::new(), &mut BTreeSet::new());
        assert_eq!(estimate.status, ResourceStatus::Unresolved);
        assert_eq!(estimate.monthly, None);

        let exotic = Resource::new("aws_new_thing.x", "aws_new_thing", json!({}));
        assert_eq!(
            engine.estimate(&exotic, &Prices::new(), &mut BTreeSet::new()).status,
            ResourceStatus::Unsupported
        );

        let role = Resource::new("aws_iam_role.x", "aws_iam_role", json!({}));
        assert_eq!(
            engine.estimate(&role, &Prices::new(), &mut BTreeSet::new()).status,
            ResourceStatus::NoDirectCharge
        );
    }

    fn component(name: &str, monthly: Option<Decimal>) -> ComponentEstimate {
        ComponentEstimate {
            name: name.into(),
            category: crate::model::Category::Compute,
            outcome: match monthly {
                Some(monthly) => Outcome::Priced {
                    monthly,
                    kind: PriceKind::Exact,
                },
                None => Outcome::Unresolved {
                    reason: "unknown".into(),
                },
            },
            quantity: None,
            unit: None,
            sku: None,
            price_description: None,
            formula: None,
            notes: vec![],
        }
    }

    fn side(components: &[(&str, Option<Decimal>)]) -> ResourceEstimate {
        let priced: Vec<Decimal> = components.iter().filter_map(|(_, monthly)| *monthly).collect();
        ResourceEstimate {
            address: "a.b".into(),
            resource_type: "a".into(),
            service: "S".into(),
            region: None,
            status: ResourceStatus::Priced,
            monthly: (!priced.is_empty()).then(|| priced.iter().sum()),
            components: components
                .iter()
                .map(|(name, monthly)| component(name, *monthly))
                .collect(),
            notes: vec![],
        }
    }

    fn change(action: Action, before: Option<ResourceEstimate>, after: Option<ResourceEstimate>) -> ChangeEstimate {
        let source = ResourceChange {
            address: "a.b".into(),
            resource_type: "a".into(),
            action,
            before: None,
            after: None,
        };
        change_estimate(&source, before, after)
    }

    #[test]
    fn deltas_cover_create_delete_update_and_negative_totals() {
        let changes = vec![
            change(Action::Create, None, Some(side(&[("hours", Some(dec!(100)))]))),
            change(Action::Delete, Some(side(&[("hours", Some(dec!(250)))])), None),
            change(
                Action::Update,
                Some(side(&[("hours", Some(dec!(40)))])),
                Some(side(&[("hours", Some(dec!(60)))])),
            ),
        ];
        assert_eq!(changes[1].delta, dec!(-250));

        let totals = totals(&changes);
        assert_eq!(
            (totals.before, totals.after, totals.delta),
            (dec!(290), dec!(160), dec!(-130))
        );
        assert_eq!(totals.annual_delta, dec!(-1560));
        assert_eq!(totals.delta_percent.unwrap().round_dp(2), dec!(-44.83));
        assert!(partial_deltas(&changes).is_empty());
    }

    #[test]
    fn a_component_priced_on_one_side_only_is_left_out_of_the_delta() {
        // Storage is priced on both sides; instance hours could not be priced after the change.
        let before = side(&[("hours", Some(dec!(116))), ("storage", Some(dec!(11.5)))]);
        let after = side(&[("hours", None), ("storage", Some(dec!(46)))]);
        let changes = vec![change(Action::Update, Some(before), Some(after))];

        assert_eq!(
            changes[0].delta,
            dec!(34.5),
            "not 46 - 127.5, which would invent a saving"
        );
        assert_eq!(partial_deltas(&changes).len(), 1);
        let totals = totals(&changes);
        assert_eq!(
            (totals.before, totals.after, totals.delta),
            (dec!(127.5), dec!(46), dec!(34.5))
        );
    }

    #[test]
    fn a_component_that_stops_applying_counts_in_full() {
        let before = side(&[("hours", Some(dec!(10))), ("iops", Some(dec!(65)))]);
        let after = side(&[("hours", Some(dec!(10)))]);
        let changes = vec![change(Action::Update, Some(before), Some(after))];
        assert_eq!(changes[0].delta, dec!(-65));
        assert!(partial_deltas(&changes).is_empty());
    }

    #[test]
    fn percentage_is_absent_without_a_previous_cost() {
        let changes = vec![change(Action::Create, None, Some(side(&[("hours", Some(dec!(10)))])))];
        assert_eq!(totals(&changes).delta_percent, None);
    }
}
