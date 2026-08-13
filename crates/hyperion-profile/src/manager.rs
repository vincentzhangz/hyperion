use serde::{Deserialize, Serialize};

/// Opaque random profile identifier. Display names are metadata only.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileId(pub String);

impl ProfileId {
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        Self(hex::encode(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    Persistent,
    Ephemeral,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("catalog: {0}")]
    Catalog(#[from] crate::catalog::CatalogError),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("lease held by {0}")]
    LeaseHeld(String),
    #[error("profile not found")]
    NotFound,
    #[error("profile deleted")]
    Deleted,
    #[error("{0}")]
    Other(String),
}

/// In-memory profile state owned by one worker.
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    pub kind: ProfileKind,
    pub settings: crate::snapshot::ProfileSettings,
    pub cookies: hyperion_net::cookie::CookieJar,
    pub local_storage: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    pub dirty: bool,
    pub last_used: std::time::Instant,
}

impl Profile {
    pub fn ephemeral(name: impl Into<String>) -> Self {
        Self {
            id: ProfileId::new(),
            name: name.into(),
            kind: ProfileKind::Ephemeral,
            settings: crate::snapshot::ProfileSettings::default(),
            cookies: hyperion_net::cookie::CookieJar::new(),
            local_storage: std::collections::HashMap::new(),
            dirty: false,
            last_used: std::time::Instant::now(),
        }
    }

    pub fn touch(&mut self) {
        self.last_used = std::time::Instant::now();
    }

    pub fn snapshot(&self) -> crate::snapshot::ProfileSnapshot {
        crate::snapshot::ProfileSnapshot {
            version: crate::snapshot::SNAPSHOT_VERSION,
            id: self.id.clone(),
            name: self.name.clone(),
            settings: self.settings.clone(),
            cookies: self.cookies.all(),
            local_storage: self.local_storage.clone(),
            checksum: String::new(),
        }
    }

    pub fn from_snapshot(snap: crate::snapshot::ProfileSnapshot) -> Self {
        let mut cookies = hyperion_net::cookie::CookieJar::new();
        for c in snap.cookies {
            cookies.set_explicit(&c.name, &c.value, &c.domain, &c.path, c.secure, c.expires);
        }
        Self {
            id: snap.id,
            name: snap.name,
            kind: ProfileKind::Persistent,
            settings: snap.settings,
            cookies,
            local_storage: snap.local_storage,
            dirty: false,
            last_used: std::time::Instant::now(),
        }
    }
}

/// Process-local profile cache with LRU eviction of clean profiles.
pub struct ProfileManager {
    loaded: std::collections::HashMap<ProfileId, Profile>,
    catalog: Box<dyn crate::catalog::Catalog>,
    store: Box<dyn crate::store::ProfileStore>,
    key: crate::crypto::Key,
    node_id: String,
    max_loaded: usize,
}

impl ProfileManager {
    pub fn new(
        catalog: Box<dyn crate::catalog::Catalog>,
        store: Box<dyn crate::store::ProfileStore>,
        key: crate::crypto::Key,
        node_id: String,
        max_loaded: usize,
    ) -> Self {
        Self {
            loaded: std::collections::HashMap::new(),
            catalog,
            store,
            key,
            node_id,
            max_loaded: max_loaded.max(1),
        }
    }

    pub fn memory_for_tests() -> Self {
        Self::new(
            Box::new(crate::catalog::MemoryCatalog::new()),
            Box::new(crate::store::MemoryStore::new()),
            crate::crypto::Key::from_bytes([7u8; 32]),
            "test-node".into(),
            64,
        )
    }

    pub fn get(&self, id: &ProfileId) -> Option<&Profile> {
        self.loaded.get(id)
    }

    pub fn get_mut(&mut self, id: &ProfileId) -> Option<&mut Profile> {
        if let Some(p) = self.loaded.get_mut(id) {
            p.touch();
            Some(p)
        } else {
            None
        }
    }

    pub fn list_loaded(&self) -> Vec<&Profile> {
        self.loaded.values().collect()
    }

    pub fn catalog_list(
        &self,
        offset: usize,
        limit: usize,
        query: Option<&str>,
    ) -> Result<Vec<crate::catalog::ProfileRecord>, ProfileError> {
        Ok(self.catalog.list(offset, limit, query)?)
    }

    pub fn catalog_get(
        &self,
        id: &ProfileId,
    ) -> Result<Option<crate::catalog::ProfileRecord>, ProfileError> {
        Ok(self.catalog.get(id)?)
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn catalog_nodes(&self) -> Result<Vec<String>, ProfileError> {
        Ok(self.catalog.nodes()?)
    }

    pub fn heartbeat(&self) -> Result<(), ProfileError> {
        Ok(self.catalog.heartbeat(&self.node_id)?)
    }

    pub fn create_persistent(&mut self, name: &str) -> Result<ProfileId, ProfileError> {
        let profile = Profile {
            id: ProfileId::new(),
            name: name.to_owned(),
            kind: ProfileKind::Persistent,
            settings: crate::snapshot::ProfileSettings::default(),
            cookies: hyperion_net::cookie::CookieJar::new(),
            local_storage: std::collections::HashMap::new(),
            dirty: true,
            last_used: std::time::Instant::now(),
        };
        let rec = crate::catalog::ProfileRecord {
            id: profile.id.clone(),
            name: profile.name.clone(),
            kind: ProfileKind::Persistent,
            owner_node: Some(self.node_id.clone()),
            storage_key: profile.id.as_str().to_owned(),
            deleted: false,
            version: 1,
        };
        self.catalog.upsert(&rec)?;
        let id = profile.id.clone();
        self.loaded.insert(id.clone(), profile);
        self.flush(&id)?;
        Ok(id)
    }

    pub fn create_ephemeral(&mut self, name: &str) -> ProfileId {
        let p = Profile::ephemeral(name);
        let id = p.id.clone();
        self.loaded.insert(id.clone(), p);
        hyperion_metrics::global()
            .profiles_loaded
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        id
    }

    pub fn open(&mut self, id: &ProfileId) -> Result<(), ProfileError> {
        if self.loaded.contains_key(id) {
            return Ok(());
        }
        let rec = self.catalog.get(id)?.ok_or(ProfileError::NotFound)?;
        if rec.deleted {
            return Err(ProfileError::Deleted);
        }
        if let Some(owner) = rec.owner_node.as_deref()
            && owner != self.node_id
        {
            hyperion_metrics::global()
                .lease_contention
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err(ProfileError::LeaseHeld(owner.to_owned()));
        }
        self.catalog.acquire_lease(id, &self.node_id)?;
        let blob = self.store.load(&rec.storage_key)?;
        let plain = crate::crypto::decrypt(&self.key, &blob)
            .map_err(|e| ProfileError::Crypto(e.to_string()))?;
        let snap: crate::snapshot::ProfileSnapshot = serde_json::from_slice(&plain)
            .map_err(|e| ProfileError::Other(format!("snapshot: {e}")))?;
        snap.verify()
            .map_err(|e| ProfileError::Other(e.to_string()))?;
        let profile = Profile::from_snapshot(snap);
        self.evict_if_needed();
        self.loaded.insert(id.clone(), profile);
        hyperion_metrics::global().profiles_loaded.store(
            self.loaded.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        Ok(())
    }

    pub fn flush(&mut self, id: &ProfileId) -> Result<(), ProfileError> {
        let Some(profile) = self.loaded.get_mut(id) else {
            return Err(ProfileError::NotFound);
        };
        if profile.kind == ProfileKind::Ephemeral {
            return Ok(());
        }
        let snap = profile.snapshot().with_checksum();
        let json = serde_json::to_vec(&snap).map_err(|e| ProfileError::Other(e.to_string()))?;
        let blob = crate::crypto::encrypt(&self.key, &json)
            .map_err(|e| ProfileError::Crypto(e.to_string()))?;
        self.store.save(id.as_str(), &blob)?;
        profile.dirty = false;
        hyperion_metrics::global()
            .profile_flush_ok
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub fn flush_all(&mut self) -> Result<(), ProfileError> {
        let ids: Vec<_> = self
            .loaded
            .iter()
            .filter(|(_, p)| p.dirty && p.kind == ProfileKind::Persistent)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.flush(&id)?;
        }
        Ok(())
    }

    pub fn close(&mut self, id: &ProfileId) -> Result<(), ProfileError> {
        if self.loaded.get(id).map(|p| p.kind) == Some(ProfileKind::Persistent) {
            self.flush(id)?;
            let _ = self.catalog.release_lease(id, &self.node_id);
        }
        self.loaded.remove(id);
        hyperion_metrics::global().profiles_loaded.store(
            self.loaded.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        Ok(())
    }

    pub fn delete(&mut self, id: &ProfileId) -> Result<(), ProfileError> {
        let _ = self.close(id);
        self.store.delete(id.as_str())?;
        self.catalog.tombstone(id)?;
        Ok(())
    }

    fn evict_if_needed(&mut self) {
        while self.loaded.len() >= self.max_loaded {
            let victim = self
                .loaded
                .iter()
                .filter(|(_, p)| !p.dirty && p.kind == ProfileKind::Persistent)
                .min_by_key(|(_, p)| p.last_used)
                .map(|(id, _)| id.clone());
            let Some(id) = victim else {
                break;
            };
            self.loaded.remove(&id);
        }
    }
}
