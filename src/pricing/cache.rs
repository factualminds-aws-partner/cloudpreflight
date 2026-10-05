//! On-disk pricing cache. Writes are atomic (temp file then rename) and an unreadable
//! entry is deleted and treated as a miss, so a corrupt cache heals itself.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{PriceQuery, PriceResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub version: String,
    pub publication_date: String,
    pub etag: Option<String>,
    pub fetched_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedQuery {
    version: String,
    query: PriceQuery,
    result: PriceResult,
}

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Directory for one provider/service/region. Each part comes from IaC or mapping
    /// data, so it is restricted to a safe alphabet before it touches the filesystem.
    pub fn dir(&self, provider: &str, service: &str, region: &str) -> Result<PathBuf> {
        let mut dir = self.root.clone();
        for part in [provider, service, region] {
            let safe = !part.is_empty()
                && part.len() <= 64
                && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !safe {
                bail!("`{part}` is not a valid provider, service or region name");
            }
            dir.push(part);
        }
        Ok(dir)
    }

    pub fn read_meta(&self, dir: &Path) -> Option<Meta> {
        read_json(&dir.join("meta.json"))
    }

    pub fn write_meta(&self, dir: &Path, meta: &Meta) -> Result<()> {
        write_json(&dir.join("meta.json"), meta)
    }

    /// A cached answer is only valid for the price list version it was computed from.
    pub fn read_query(&self, dir: &Path, version: &str, query: &PriceQuery) -> Option<PriceResult> {
        let cached: CachedQuery = read_json(&query_path(dir, query))?;
        (cached.version == version && cached.query == *query).then_some(cached.result)
    }

    pub fn write_query(&self, dir: &Path, version: &str, query: &PriceQuery, result: &PriceResult) -> Result<()> {
        let cached = CachedQuery {
            version: version.to_string(),
            query: query.clone(),
            result: result.clone(),
        };
        write_json(&query_path(dir, query), &cached)
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn query_path(dir: &Path, query: &PriceQuery) -> PathBuf {
    // BTreeMap filters serialise in key order, so the key is deterministic.
    let canonical = serde_json::to_vec(query).unwrap_or_default();
    let digest = Sha256::digest(&canonical);
    let hex: String = digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect();
    dir.join(format!("q-{hex}.json"))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "removing corrupt pricing cache entry");
            let _ = fs::remove_file(path);
            None
        }
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path.parent().context("cache path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create cache directory {}", dir.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.persist(path)
        .with_context(|| format!("cannot write cache file {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn query() -> PriceQuery {
        PriceQuery {
            provider: "aws".into(),
            service: "AmazonEC2".into(),
            region: "us-east-1".into(),
            filters: BTreeMap::from([("instanceType".to_string(), "t3.micro".to_string())]),
        }
    }

    #[test]
    fn round_trip_is_bound_to_the_price_list_version() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache::new(root.path().to_path_buf());
        let dir = cache.dir("aws", "AmazonEC2", "us-east-1").unwrap();

        cache.write_query(&dir, "v1", &query(), &PriceResult::NotFound).unwrap();

        assert_eq!(cache.read_query(&dir, "v1", &query()), Some(PriceResult::NotFound));
        assert_eq!(cache.read_query(&dir, "v2", &query()), None);
    }

    #[test]
    fn corrupt_entries_are_removed_and_treated_as_a_miss() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache::new(root.path().to_path_buf());
        let dir = cache.dir("aws", "AmazonEC2", "us-east-1").unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("meta.json"), b"{not json").unwrap();

        assert!(cache.read_meta(&dir).is_none());
        assert!(!dir.join("meta.json").exists());
    }

    #[test]
    fn path_like_names_are_rejected() {
        let cache = Cache::new(PathBuf::from("/tmp/x"));
        assert!(cache.dir("aws", "AmazonEC2", "../../etc").is_err());
        assert!(cache.dir("aws", "", "us-east-1").is_err());
        assert!(cache.dir("aws", "Amazon/EC2", "us-east-1").is_err());
    }
}
