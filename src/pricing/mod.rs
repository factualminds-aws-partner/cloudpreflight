//! Pricing layer: turns provider-neutral price queries into list prices.
//! Knows nothing about how the infrastructure was authored.

pub mod aws_bulk;
pub mod cache;

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// One price lookup. Filters match product attributes exactly, with two extras:
/// the key `productFamily` matches the product family, and a value starting with
/// `@` matches a usage type with or without its region prefix (`@Request`
/// matches both `Request` and `EUW1-Request`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PriceQuery {
    pub provider: String,
    pub service: String,
    pub region: String,
    pub filters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier {
    pub begin: Decimal,
    /// `None` means unbounded.
    pub end: Option<Decimal>,
    pub price: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    pub sku: String,
    pub description: String,
    pub unit: String,
    pub tiers: Vec<Tier>,
    /// Number of matching SKUs with different prices. 1 means an unambiguous match.
    pub candidates: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PriceResult {
    Found(Price),
    /// The price list was read but no SKU matched the query.
    NotFound,
    /// The price list itself could not be obtained.
    Unavailable {
        reason: String,
    },
}

pub type Prices = BTreeMap<PriceQuery, PriceResult>;
