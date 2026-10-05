//! AWS public list prices from the Price List Bulk API. No credentials are needed.
//!
//! Regional price files can be very large (EC2 in us-east-1 is roughly 480 MB), so the
//! file is streamed to disk and then stream-parsed, keeping only the SKUs a scan asks
//! for. Answers are cached per query, so the big file is parsed once per new query.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::cache::{Cache, Meta, now};
use super::{Price, PriceQuery, PriceResult, Prices, Tier};
use crate::model::PricingSnapshot;

pub const DEFAULT_BASE_URL: &str = "https://pricing.us-east-1.amazonaws.com";
const MAX_CONCURRENT_DOWNLOADS: usize = 4;
const MAX_PRICE_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const ATTEMPTS: u32 = 5;
const PROGRESS_EVERY_BYTES: u64 = 64 * 1024 * 1024;

pub type Progress = Arc<dyn Fn(String) + Send + Sync>;

pub struct AwsPricing {
    pub client: reqwest::Client,
    pub base_url: String,
    pub cache: Cache,
    pub offline: bool,
    pub no_cache: bool,
    pub ttl_secs: u64,
    pub progress: Progress,
}

#[derive(Debug, Default)]
pub struct Resolved {
    pub prices: Prices,
    pub snapshots: Vec<PricingSnapshot>,
    pub warnings: Vec<String>,
}

enum Download {
    NotModified,
    Fetched { etag: Option<String> },
}

impl AwsPricing {
    /// Resolves all queries, one task per service and region, at most four at a time.
    pub async fn resolve(self: Arc<Self>, queries: BTreeSet<PriceQuery>) -> Resolved {
        let mut groups: BTreeMap<(String, String), Vec<PriceQuery>> = BTreeMap::new();
        for query in queries {
            groups
                .entry((query.service.clone(), query.region.clone()))
                .or_default()
                .push(query);
        }

        let limit = Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
        let mut tasks = JoinSet::new();
        for ((service, region), queries) in groups {
            let pricing = Arc::clone(&self);
            let limit = Arc::clone(&limit);
            tasks.spawn(async move {
                let _permit = limit.acquire_owned().await;
                pricing.resolve_group(&service, &region, queries).await
            });
        }

        let mut resolved = Resolved::default();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(group) => {
                    resolved.prices.extend(group.prices);
                    resolved.snapshots.extend(group.snapshots);
                    resolved.warnings.extend(group.warnings);
                }
                Err(error) => resolved.warnings.push(format!("a pricing task failed: {error}")),
            }
        }
        // Tasks finish in any order; sort so identical inputs give identical output.
        resolved.snapshots.sort();
        resolved.warnings.sort();
        resolved
    }

    async fn resolve_group(&self, service: &str, region: &str, queries: Vec<PriceQuery>) -> Resolved {
        let unavailable = |queries: Vec<PriceQuery>, reason: String| Resolved {
            prices: queries
                .into_iter()
                .map(|query| (query, PriceResult::Unavailable { reason: reason.clone() }))
                .collect(),
            ..Resolved::default()
        };

        let dir = match self.cache.dir("aws", service, region) {
            Ok(dir) => dir,
            Err(error) => return unavailable(queries, error.to_string()),
        };
        let raw = dir.join("index.json");
        let mut meta = self.cache.read_meta(&dir).filter(|_| raw.exists());
        let mut warnings = Vec::new();
        let mut downloaded = false;
        let mut etag = meta.as_ref().and_then(|meta| meta.etag.clone());

        let fresh = !self.no_cache
            && meta
                .as_ref()
                .is_some_and(|meta| now().saturating_sub(meta.fetched_at) < self.ttl_secs);
        if !fresh && self.offline && meta.is_none() {
            let reason = format!(
                "offline and no cached {service} price list for {region}. Run once without --offline to fetch it."
            );
            return unavailable(queries, reason);
        }
        if !fresh && !self.offline {
            match self.download(service, region, &dir, &raw, meta.as_ref()).await {
                Ok(Download::NotModified) => {
                    if let Some(meta) = &mut meta {
                        meta.fetched_at = now();
                        let _ = self.cache.write_meta(&dir, meta);
                    }
                }
                Ok(Download::Fetched { etag: new_etag }) => {
                    downloaded = true;
                    etag = new_etag;
                }
                Err(error) if meta.is_some() => warnings.push(format!(
                    "could not refresh {service} prices for {region} ({error:#}); using the cached copy"
                )),
                Err(error) => {
                    let reason = format!("could not fetch the {service} price list for {region}: {error:#}");
                    return unavailable(queries, reason);
                }
            }
        }

        let mut prices = Prices::new();
        let mut misses = Vec::new();
        for query in queries {
            let cached = match (&meta, downloaded) {
                (Some(meta), false) => self.cache.read_query(&dir, &meta.version, &query),
                _ => None,
            };
            match cached {
                Some(result) => {
                    prices.insert(query, result);
                }
                None => misses.push(query),
            }
        }

        if !misses.is_empty() {
            let path = raw.clone();
            let wanted = misses.clone();
            let parsed = tokio::task::spawn_blocking(move || parse_offer_file(&path, &wanted)).await;
            let offer = match parsed {
                Ok(Ok(offer)) => offer,
                Ok(Err(error)) => {
                    let _ = std::fs::remove_file(&raw);
                    let _ = std::fs::remove_file(dir.join("meta.json"));
                    let reason = format!(
                        "the cached {service} price list for {region} was unreadable and has been removed ({error:#}). Re-run to fetch it again."
                    );
                    prices.extend(unavailable(misses, reason).prices);
                    return Resolved {
                        prices,
                        warnings,
                        ..Resolved::default()
                    };
                }
                Err(error) => {
                    prices.extend(unavailable(misses, format!("price list parsing failed: {error}")).prices);
                    return Resolved {
                        prices,
                        warnings,
                        ..Resolved::default()
                    };
                }
            };

            let fetched_at = match (&meta, downloaded) {
                (Some(meta), false) => meta.fetched_at,
                _ => now(),
            };
            let new_meta = Meta {
                version: offer.version,
                publication_date: offer.publication_date,
                etag,
                fetched_at,
            };
            if let Err(error) = self.cache.write_meta(&dir, &new_meta) {
                warnings.push(format!("could not update the pricing cache: {error:#}"));
            }
            for (query, result) in misses.into_iter().zip(offer.results) {
                let _ = self.cache.write_query(&dir, &new_meta.version, &query, &result);
                prices.insert(query, result);
            }
            meta = Some(new_meta);
        }

        let snapshots = meta
            .map(|meta| PricingSnapshot {
                provider: "aws".into(),
                service: service.to_string(),
                region: region.to_string(),
                version: meta.version,
                publication_date: meta.publication_date,
            })
            .into_iter()
            .collect();

        Resolved {
            prices,
            snapshots,
            warnings,
        }
    }

    /// Streams the price list to a temp file and renames it into place when complete.
    /// A dropped connection resumes from the bytes already on disk (HTTP range request).
    async fn download(
        &self,
        service: &str,
        region: &str,
        dir: &Path,
        raw: &Path,
        meta: Option<&Meta>,
    ) -> Result<Download> {
        let url = format!(
            "{}/offers/v1.0/aws/{service}/current/{region}/index.json",
            self.base_url.trim_end_matches('/')
        );
        (self.progress)(format!("Fetching {service} prices for {region}"));
        tracing::info!(%url, "fetching price list");

        std::fs::create_dir_all(dir).with_context(|| format!("cannot create cache directory {}", dir.display()))?;
        // Dropping the temp file (error, or Ctrl-C cancelling this task) deletes it,
        // so a partial download never becomes the cached price list.
        let mut partial = Partial {
            file: tempfile::NamedTempFile::new_in(dir)?,
            bytes: 0,
            etag: None,
            label: format!("{service} prices for {region}"),
        };
        let known_etag = meta.and_then(|meta| meta.etag.as_deref()).filter(|_| !self.no_cache);

        let mut attempt = 1;
        loop {
            match self.fetch_into(&url, known_etag, &mut partial).await {
                Ok(true) => break,
                Ok(false) => return Ok(Download::NotModified),
                Err(Failure::Fatal(error)) => return Err(error),
                Err(Failure::Retry(reason)) if attempt == ATTEMPTS => bail!("{reason} (after {ATTEMPTS} attempts)"),
                Err(Failure::Retry(reason)) => {
                    tracing::info!(%reason, attempt, bytes = partial.bytes, "retrying price list download");
                    tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                    attempt += 1;
                }
            }
        }

        partial
            .file
            .persist(raw)
            .context("cannot store the downloaded price list")?;
        Ok(Download::Fetched { etag: partial.etag })
    }

    /// One HTTP exchange. `Ok(true)` when the file is complete, `Ok(false)` when the
    /// server says the cached copy is still current.
    async fn fetch_into(&self, url: &str, known_etag: Option<&str>, partial: &mut Partial) -> Result<bool, Failure> {
        let mut request = self.client.get(url);
        if partial.bytes > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={}-", partial.bytes));
            if let Some(etag) = &partial.etag {
                request = request.header(reqwest::header::IF_RANGE, etag);
            }
        } else if let Some(etag) = known_etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }

        let mut response = request
            .send()
            .await
            .map_err(|error| Failure::Retry(root_cause(&error)))?;
        match response.status() {
            StatusCode::NOT_MODIFIED => return Ok(false),
            StatusCode::PARTIAL_CONTENT => {}
            // The server ignored or rejected the range (the file changed): start over.
            StatusCode::OK | StatusCode::RANGE_NOT_SATISFIABLE => {
                partial.restart().map_err(|error| Failure::Fatal(error.into()))?;
                if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                    return Err(Failure::Retry("the price list changed during download".into()));
                }
            }
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => {
                return Err(Failure::Fatal(anyhow::anyhow!(
                    "AWS publishes no price list at {url}. Check the region name."
                )));
            }
            status if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() => {
                return Err(Failure::Retry(format!("HTTP {status}")));
            }
            status => return Err(Failure::Fatal(anyhow::anyhow!("unexpected HTTP {status}"))),
        }
        if partial.etag.is_none() {
            partial.etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
        }

        let expected = response.content_length().map(|remaining| partial.bytes + remaining);
        let mut next_report = partial.bytes + PROGRESS_EVERY_BYTES;
        // ponytail: synchronous writes on the async runtime; at most four run at once.
        // Move to tokio::fs if download concurrency ever grows.
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => return Err(Failure::Retry(root_cause(&error))),
            };
            partial.bytes += chunk.len() as u64;
            if partial.bytes > MAX_PRICE_FILE_BYTES {
                return Err(Failure::Fatal(anyhow::anyhow!(
                    "the price list exceeds the {MAX_PRICE_FILE_BYTES} byte limit"
                )));
            }
            partial
                .file
                .write_all(&chunk)
                .map_err(|error| Failure::Fatal(error.into()))?;

            if partial.bytes >= next_report {
                next_report += PROGRESS_EVERY_BYTES;
                let megabytes = |bytes: u64| bytes / (1024 * 1024);
                let of = expected.map_or_else(String::new, |total| format!(" of {}", megabytes(total)));
                (self.progress)(format!(
                    "Fetching {}: {}{of} MB",
                    partial.label,
                    megabytes(partial.bytes)
                ));
            }
        }

        match expected {
            Some(total) if partial.bytes < total => Err(Failure::Retry("the connection closed early".into())),
            _ => Ok(true),
        }
    }
}

struct Partial {
    file: tempfile::NamedTempFile,
    bytes: u64,
    etag: Option<String>,
    label: String,
}

impl Partial {
    fn restart(&mut self) -> std::io::Result<()> {
        if self.bytes == 0 {
            return Ok(());
        }
        self.bytes = 0;
        self.etag = None;
        self.file.as_file_mut().set_len(0)?;
        self.file.as_file_mut().seek(SeekFrom::Start(0))?;
        Ok(())
    }
}

enum Failure {
    /// Worth trying again: throttling, server errors, dropped connections.
    Retry(String),
    Fatal(anyhow::Error),
}

/// The innermost error message; HTTP client errors wrap the useful part several layers deep.
fn root_cause(error: &dyn std::error::Error) -> String {
    let mut cause = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}

#[derive(Debug)]
pub struct ParsedOffer {
    pub version: String,
    pub publication_date: String,
    /// One result per query, in query order.
    pub results: Vec<PriceResult>,
}

pub fn parse_offer_file(path: &Path, queries: &[PriceQuery]) -> Result<ParsedOffer> {
    let open = |path: &PathBuf| -> Result<BufReader<File>> {
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        Ok(BufReader::with_capacity(1 << 20, file))
    };
    let path = path.to_path_buf();

    let mut state = State {
        queries,
        ..State::default()
    };
    Root(&mut state).deserialize(&mut serde_json::Deserializer::from_reader(open(&path)?))?;
    if state.terms_skipped {
        // `terms` came before `products` in this file, so read the terms in a second pass.
        state.products_done = true;
        Root(&mut state).deserialize(&mut serde_json::Deserializer::from_reader(open(&path)?))?;
    }

    Ok(state.finish())
}

#[cfg(test)]
pub fn parse_offer_str(json: &str, queries: &[PriceQuery]) -> Result<ParsedOffer> {
    let mut state = State {
        queries,
        ..State::default()
    };
    Root(&mut state).deserialize(&mut serde_json::Deserializer::from_str(json))?;
    if state.terms_skipped {
        state.products_done = true;
        Root(&mut state).deserialize(&mut serde_json::Deserializer::from_str(json))?;
    }
    Ok(state.finish())
}

#[derive(Deserialize)]
struct Product {
    #[serde(rename = "productFamily", default)]
    family: Option<String>,
    #[serde(default)]
    attributes: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Term {
    #[serde(rename = "priceDimensions", default)]
    dimensions: BTreeMap<String, Dimension>,
}

#[derive(Deserialize)]
struct Dimension {
    #[serde(default)]
    unit: String,
    #[serde(rename = "beginRange", default)]
    begin: String,
    #[serde(rename = "endRange", default)]
    end: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "pricePerUnit", default)]
    price: BTreeMap<String, String>,
}

#[derive(Default)]
struct State<'q> {
    queries: &'q [PriceQuery],
    version: String,
    publication_date: String,
    /// SKU to the indexes of the queries it satisfies.
    matched: BTreeMap<String, Vec<usize>>,
    terms: BTreeMap<String, Vec<Dimension>>,
    products_done: bool,
    terms_skipped: bool,
}

impl State<'_> {
    fn finish(self) -> ParsedOffer {
        let mut candidates: Vec<Vec<Price>> = vec![Vec::new(); self.queries.len()];
        for (sku, query_indexes) in &self.matched {
            let Some(price) = self.terms.get(sku).and_then(|dimensions| to_price(sku, dimensions)) else {
                continue;
            };
            for index in query_indexes {
                candidates[*index].push(price.clone());
            }
        }

        let results = candidates
            .into_iter()
            .map(|prices| {
                // `matched` is a BTreeMap, so candidates arrive in SKU order: the choice is stable.
                let distinct: BTreeSet<String> = prices
                    .iter()
                    .map(|price| format!("{}|{:?}", price.unit, price.tiers))
                    .collect();
                match prices.into_iter().next() {
                    Some(mut price) => {
                        price.candidates = distinct.len();
                        PriceResult::Found(price)
                    }
                    None => PriceResult::NotFound,
                }
            })
            .collect();

        ParsedOffer {
            version: self.version,
            publication_date: self.publication_date,
            results,
        }
    }
}

fn to_price(sku: &str, dimensions: &[Dimension]) -> Option<Price> {
    let mut tiers = Vec::new();
    for dimension in dimensions {
        let price = parse_decimal(dimension.price.get("USD")?)?;
        let begin = parse_decimal(&dimension.begin).unwrap_or_default();
        let end = parse_decimal(&dimension.end);
        tiers.push(Tier { begin, end, price });
    }
    tiers.sort_by_key(|tier| tier.begin);

    let first = dimensions.first()?;
    Some(Price {
        sku: sku.to_string(),
        description: first.description.clone(),
        unit: first.unit.clone(),
        tiers,
        candidates: 1,
    })
}

/// `Inf` and empty ranges parse to `None`.
fn parse_decimal(text: &str) -> Option<Decimal> {
    Decimal::from_str(text).or_else(|_| Decimal::from_scientific(text)).ok()
}

fn product_matches(product: &Product, query: &PriceQuery) -> bool {
    query.filters.iter().all(|(key, wanted)| {
        let actual = match key.as_str() {
            "productFamily" => product.family.as_deref(),
            _ => product.attributes.get(key).map(String::as_str),
        };
        let Some(actual) = actual else {
            return false;
        };
        match wanted.strip_prefix('@') {
            Some(usage_type) => regional_match(actual, usage_type),
            None => actual == wanted,
        }
    })
}

/// True when `actual` is `wanted`, optionally behind a region prefix such as `USE1-` or `EU-`.
/// Prefixes like `IA-` or `Global-` are not regions and do not match. A trailing `*` in
/// `wanted` matches any suffix.
fn regional_match(actual: &str, wanted: &str) -> bool {
    let matches = |candidate: &str| match wanted.strip_suffix('*') {
        Some(prefix) => candidate.starts_with(prefix),
        None => candidate == wanted,
    };
    let without_region = actual
        .split_once('-')
        .filter(|(head, _)| is_region_prefix(head))
        .map(|(_, rest)| rest);
    matches(actual) || without_region.is_some_and(matches)
}

fn is_region_prefix(prefix: &str) -> bool {
    let (letters, digit) = prefix.split_at(prefix.len().saturating_sub(1));
    prefix == "EU"
        || ((2..=4).contains(&letters.len())
            && letters.chars().all(|c| c.is_ascii_uppercase())
            && digit.chars().all(|c| c.is_ascii_digit()))
}

struct Root<'a, 'q>(&'a mut State<'q>);

impl<'de> DeserializeSeed<'de> for Root<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Root<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an AWS price list object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let state = self.0;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "version" => state.version = map.next_value()?,
                "publicationDate" => state.publication_date = map.next_value()?,
                "products" if !state.products_done => {
                    map.next_value_seed(Products(&mut *state))?;
                    state.products_done = true;
                }
                "terms" if state.products_done => map.next_value_seed(Terms(&mut *state))?,
                "terms" => {
                    state.terms_skipped = true;
                    map.next_value::<IgnoredAny>()?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

struct Products<'a, 'q>(&'a mut State<'q>);

impl<'de> DeserializeSeed<'de> for Products<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Products<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a map of SKU to product")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(sku) = map.next_key::<String>()? {
            let product: Product = map.next_value()?;
            let hits: Vec<usize> = (0..self.0.queries.len())
                .filter(|index| product_matches(&product, &self.0.queries[*index]))
                .collect();
            if !hits.is_empty() {
                self.0.matched.insert(sku, hits);
            }
        }
        Ok(())
    }
}

struct Terms<'a, 'q>(&'a mut State<'q>);

impl<'de> DeserializeSeed<'de> for Terms<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Terms<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a map of term type to terms")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(term_type) = map.next_key::<String>()? {
            // Reserved-instance terms are large and never used: on-demand list prices only.
            if term_type == "OnDemand" {
                map.next_value_seed(OnDemand(&mut *self.0))?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

struct OnDemand<'a, 'q>(&'a mut State<'q>);

impl<'de> DeserializeSeed<'de> for OnDemand<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for OnDemand<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a map of SKU to on-demand terms")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(sku) = map.next_key::<String>()? {
            if !self.0.matched.contains_key(&sku) {
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            let terms: BTreeMap<String, Term> = map.next_value()?;
            let dimensions = terms
                .into_values()
                .flat_map(|term| term.dimensions.into_values())
                .collect();
            self.0.terms.insert(sku, dimensions);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    const OFFER: &str = r#"{
      "formatVersion": "v1.0", "offerCode": "AmazonS3", "version": "20260101", "publicationDate": "2026-01-01T00:00:00Z",
      "products": {
        "S1": {"sku": "S1", "productFamily": "Storage", "attributes": {"volumeType": "Standard", "usagetype": "EUW1-TimedStorage-ByteHrs"}},
        "S2": {"sku": "S2", "productFamily": "Storage", "attributes": {"volumeType": "Standard", "usagetype": "IA-TimedStorage-ByteHrs"}},
        "S3": {"sku": "S3", "attributes": {"usagetype": "Global-Request"}},
        "S4": {"sku": "S4", "productFamily": "Storage", "attributes": {"volumeType": "NoTerms"}}
      },
      "terms": {
        "Reserved": {"S1": {"ignored": true}},
        "OnDemand": {
          "S1": {"S1.T": {"priceDimensions": {
            "b": {"unit": "GB-Mo", "beginRange": "51200", "endRange": "Inf", "description": "next", "pricePerUnit": {"USD": "0.0220000000"}},
            "a": {"unit": "GB-Mo", "beginRange": "0", "endRange": "51200", "description": "first", "pricePerUnit": {"USD": "0.0230000000"}}
          }}},
          "S2": {"S2.T": {"priceDimensions": {"a": {"unit": "GB-Mo", "beginRange": "0", "endRange": "Inf", "pricePerUnit": {"USD": "0.0125"}}}}},
          "S3": {"S3.T": {"priceDimensions": {"a": {"unit": "Requests", "beginRange": "0", "endRange": "Inf", "pricePerUnit": {"USD": "0"}}}}}
        }
      }
    }"#;

    fn query(filters: &[(&str, &str)]) -> PriceQuery {
        PriceQuery {
            provider: "aws".into(),
            service: "AmazonS3".into(),
            region: "eu-west-1".into(),
            filters: filters.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    #[test]
    fn matches_filters_and_sorts_tiers() {
        let queries = [
            query(&[("productFamily", "Storage"), ("usagetype", "@TimedStorage-ByteHrs")]),
            query(&[("usagetype", "@Request")]),
            query(&[("volumeType", "NoTerms")]),
            query(&[("productFamily", "Storage"), ("volumeType", "Standard")]),
        ];
        let offer = parse_offer_str(OFFER, &queries).unwrap();

        assert_eq!(offer.version, "20260101");
        let PriceResult::Found(storage) = &offer.results[0] else {
            panic!("expected a price");
        };
        assert_eq!(storage.sku, "S1");
        assert_eq!(storage.candidates, 1);
        assert_eq!(
            storage.tiers,
            vec![
                Tier {
                    begin: dec!(0),
                    end: Some(dec!(51200)),
                    price: dec!(0.023)
                },
                Tier {
                    begin: dec!(51200),
                    end: None,
                    price: dec!(0.022)
                },
            ]
        );
        // `Global-Request` is the free-tier SKU, not a regional variant of `Request`.
        assert_eq!(offer.results[1], PriceResult::NotFound);
        // A product without on-demand terms has no price; it is not a zero price.
        assert_eq!(offer.results[2], PriceResult::NotFound);
        let PriceResult::Found(ambiguous) = &offer.results[3] else {
            panic!("expected a price");
        };
        assert_eq!(ambiguous.candidates, 2);
    }

    #[test]
    fn terms_before_products_still_resolve() {
        let value: serde_json::Value = serde_json::from_str(OFFER).unwrap();
        let reordered = format!(
            r#"{{"terms": {}, "version": "v", "products": {}}}"#,
            value["terms"], value["products"]
        );
        let offer = parse_offer_str(&reordered, &[query(&[("usagetype", "@TimedStorage-ByteHrs")])]).unwrap();
        assert!(matches!(offer.results[0], PriceResult::Found(_)));
    }

    #[test]
    fn malformed_payloads_are_errors_not_panics() {
        for payload in [
            "",
            "[]",
            "{\"products\": 7}",
            "{\"products\": {\"a\": {\"attributes\": 1}}}",
            "{\"terms\"",
        ] {
            assert!(parse_offer_str(payload, &[query(&[("a", "b")])]).is_err(), "{payload}");
        }
    }

    fn pricing(base_url: &str, cache_dir: &Path, offline: bool) -> Arc<AwsPricing> {
        Arc::new(AwsPricing {
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
            cache: Cache::new(cache_dir.to_path_buf()),
            offline,
            no_cache: false,
            ttl_secs: 3600,
            progress: Arc::new(|_| {}),
        })
    }

    const OFFER_PATH: &str = "/offers/v1.0/aws/AmazonS3/current/eu-west-1/index.json";

    fn storage_query() -> BTreeSet<PriceQuery> {
        BTreeSet::from([query(&[
            ("productFamily", "Storage"),
            ("usagetype", "@TimedStorage-ByteHrs"),
        ])])
    }

    #[tokio::test]
    async fn fetches_once_then_serves_from_cache_and_works_offline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(OFFER_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_string(OFFER),
            )
            .expect(1)
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();

        let first = pricing(&server.uri(), cache.path(), false)
            .resolve(storage_query())
            .await;
        let second = pricing(&server.uri(), cache.path(), false)
            .resolve(storage_query())
            .await;
        let offline = pricing("http://127.0.0.1:9", cache.path(), true)
            .resolve(storage_query())
            .await;

        assert!(matches!(first.prices.values().next(), Some(PriceResult::Found(_))));
        assert_eq!(first.prices, second.prices);
        assert_eq!(first.prices, offline.prices);
        assert_eq!(first.snapshots[0].version, "20260101");
        // The temp file was renamed into place; nothing is left behind.
        let files: Vec<_> = std::fs::read_dir(cache.path().join("aws/AmazonS3/eu-west-1"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 3, "index.json, meta.json and one cached query");
    }

    #[tokio::test]
    async fn offline_without_a_cache_is_unavailable_with_a_remedy() {
        let cache = tempfile::tempdir().unwrap();
        let resolved = pricing("http://127.0.0.1:9", cache.path(), true)
            .resolve(storage_query())
            .await;
        let Some(PriceResult::Unavailable { reason }) = resolved.prices.values().next() else {
            panic!("expected unavailable");
        };
        assert!(
            reason.contains("offline") && reason.contains("without --offline"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn throttling_is_retried_and_a_missing_region_is_explained() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(OFFER_PATH))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(OFFER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(OFFER))
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();

        let resolved = pricing(&server.uri(), cache.path(), false)
            .resolve(storage_query())
            .await;
        assert!(matches!(resolved.prices.values().next(), Some(PriceResult::Found(_))));

        let mut elsewhere = query(&[("productFamily", "Storage")]);
        elsewhere.region = "xx-nowhere-1".into();
        let resolved = pricing(&server.uri(), cache.path(), false)
            .resolve(BTreeSet::from([elsewhere]))
            .await;
        let Some(PriceResult::Unavailable { reason }) = resolved.prices.values().next() else {
            panic!("expected unavailable");
        };
        assert!(
            reason.contains("xx-nowhere-1") && reason.contains("Check the region name"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn a_corrupt_price_list_is_removed_and_reported_then_refetched() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(OFFER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(OFFER))
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();
        pricing(&server.uri(), cache.path(), false)
            .resolve(storage_query())
            .await;

        // Truncate the cached file and ask something the query cache has not seen.
        let dir = cache.path().join("aws/AmazonS3/eu-west-1");
        std::fs::write(dir.join("index.json"), &OFFER[..OFFER.len() / 2]).unwrap();
        let other = BTreeSet::from([query(&[("volumeType", "Standard")])]);

        let broken = pricing(&server.uri(), cache.path(), false).resolve(other.clone()).await;
        let Some(PriceResult::Unavailable { reason }) = broken.prices.values().next() else {
            panic!("expected unavailable");
        };
        assert!(reason.contains("unreadable and has been removed"), "{reason}");
        assert!(!dir.join("index.json").exists());

        let healed = pricing(&server.uri(), cache.path(), false).resolve(other).await;
        assert!(matches!(healed.prices.values().next(), Some(PriceResult::Found(_))));
    }

    #[tokio::test]
    async fn an_interrupted_download_resumes_with_a_range_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A raw socket server: the first response promises the whole body but closes
        // half way; the second must be a range request for the rest.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let half = OFFER.len() / 2;
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..read]).to_lowercase());
                let response = if attempt == 0 {
                    format!(
                        "HTTP/1.1 200 OK\r\netag: \"v1\"\r\ncontent-length: {}\r\n\r\n{}",
                        OFFER.len(),
                        &OFFER[..half]
                    )
                } else {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\n\r\n{}",
                        OFFER.len() - half,
                        &OFFER[half..]
                    )
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            requests
        });
        let cache = tempfile::tempdir().unwrap();

        let resolved = pricing(&format!("http://{address}"), cache.path(), false)
            .resolve(storage_query())
            .await;
        let requests = server.await.unwrap();

        assert!(
            matches!(resolved.prices.values().next(), Some(PriceResult::Found(_))),
            "{:?}",
            resolved.prices
        );
        assert!(!requests[0].contains("range:"));
        assert!(
            requests[1].contains(&format!("range: bytes={half}-")),
            "{}",
            requests[1]
        );
        assert!(requests[1].contains("if-range: \"v1\""), "{}", requests[1]);
    }

    #[test]
    fn region_prefixes() {
        assert!(regional_match("Request", "Request"));
        assert!(regional_match("USE1-Request", "Request"));
        assert!(regional_match("EU-Request", "Request"));
        assert!(regional_match("APS3-Request", "Request"));
        assert!(!regional_match("Global-Request", "Request"));
        assert!(!regional_match("IA-TimedStorage-ByteHrs", "TimedStorage-ByteHrs"));
        assert!(!regional_match("Request-ARM", "Request"));
        assert!(regional_match("NodeUsage:cache.m6g.xl", "NodeUsage:*"));
        assert!(regional_match("USE1-NodeUsage:cache.t3.micro", "NodeUsage:*"));
        assert!(!regional_match(
            "USE1-ExtendedSupportYr3-NodeUsage:cache.t3.micro",
            "NodeUsage:*"
        ));
    }
}
