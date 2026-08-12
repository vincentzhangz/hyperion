use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::manager::{ProfileId, ProfileKind};

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("not found")]
    NotFound,
    #[error("lease held by {0}")]
    LeaseHeld(String),
    #[error("postgres: {0}")]
    Postgres(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileRecord {
    pub id: ProfileId,
    pub name: String,
    pub kind: ProfileKind,
    pub owner_node: Option<String>,
    pub storage_key: String,
    pub deleted: bool,
    pub version: u64,
}

pub trait Catalog: Send + Sync {
    fn upsert(&self, rec: &ProfileRecord) -> Result<(), CatalogError>;
    fn get(&self, id: &ProfileId) -> Result<Option<ProfileRecord>, CatalogError>;
    fn list(
        &self,
        offset: usize,
        limit: usize,
        query: Option<&str>,
    ) -> Result<Vec<ProfileRecord>, CatalogError>;
    fn acquire_lease(&self, id: &ProfileId, node: &str) -> Result<(), CatalogError>;
    fn release_lease(&self, id: &ProfileId, node: &str) -> Result<(), CatalogError>;
    fn tombstone(&self, id: &ProfileId) -> Result<(), CatalogError>;
    fn heartbeat(&self, node: &str) -> Result<(), CatalogError>;
    fn nodes(&self) -> Result<Vec<String>, CatalogError>;
}

#[derive(Clone, Default)]
pub struct MemoryCatalog {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    profiles: HashMap<String, ProfileRecord>,
    nodes: HashMap<String, u64>,
}

impl MemoryCatalog {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Catalog for MemoryCatalog {
    fn upsert(&self, rec: &ProfileRecord) -> Result<(), CatalogError> {
        self.inner
            .lock()
            .unwrap()
            .profiles
            .insert(rec.id.as_str().to_owned(), rec.clone());
        Ok(())
    }

    fn get(&self, id: &ProfileId) -> Result<Option<ProfileRecord>, CatalogError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .profiles
            .get(id.as_str())
            .cloned())
    }

    fn list(
        &self,
        offset: usize,
        limit: usize,
        query: Option<&str>,
    ) -> Result<Vec<ProfileRecord>, CatalogError> {
        let inner = self.inner.lock().unwrap();
        let mut rows: Vec<_> = inner
            .profiles
            .values()
            .filter(|r| !r.deleted)
            .filter(|r| {
                query
                    .map(|q| r.name.contains(q) || r.id.as_str().contains(q))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rows.into_iter().skip(offset).take(limit).collect())
    }

    fn acquire_lease(&self, id: &ProfileId, node: &str) -> Result<(), CatalogError> {
        let mut inner = self.inner.lock().unwrap();
        let rec = inner
            .profiles
            .get_mut(id.as_str())
            .ok_or(CatalogError::NotFound)?;
        if let Some(owner) = rec.owner_node.as_deref()
            && owner != node
        {
            return Err(CatalogError::LeaseHeld(owner.to_owned()));
        }
        rec.owner_node = Some(node.to_owned());
        Ok(())
    }

    fn release_lease(&self, id: &ProfileId, node: &str) -> Result<(), CatalogError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(rec) = inner.profiles.get_mut(id.as_str())
            && rec.owner_node.as_deref() == Some(node)
        {
            rec.owner_node = None;
        }
        Ok(())
    }

    fn tombstone(&self, id: &ProfileId) -> Result<(), CatalogError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(rec) = inner.profiles.get_mut(id.as_str()) {
            rec.deleted = true;
            rec.owner_node = None;
        }
        Ok(())
    }

    fn heartbeat(&self, node: &str) -> Result<(), CatalogError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.inner
            .lock()
            .unwrap()
            .nodes
            .insert(node.to_owned(), now);
        Ok(())
    }

    fn nodes(&self) -> Result<Vec<String>, CatalogError> {
        Ok(self.inner.lock().unwrap().nodes.keys().cloned().collect())
    }
}
