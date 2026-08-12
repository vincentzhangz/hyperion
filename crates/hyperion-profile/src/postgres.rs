//! PostgreSQL cluster catalog: profile metadata, leases, node heartbeats.

use postgres::{Client, NoTls};
use std::sync::Mutex;

use super::{Catalog, CatalogError, ProfileRecord};
use crate::manager::{ProfileId, ProfileKind};

pub struct PostgresCatalog {
    client: Mutex<Client>,
}

impl PostgresCatalog {
    pub fn connect(url: &str) -> Result<Self, CatalogError> {
        let mut client =
            Client::connect(url, NoTls).map_err(|e| CatalogError::Postgres(e.to_string()))?;
        client
            .batch_execute(
                r#"
                CREATE TABLE IF NOT EXISTS hyperion_profiles (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    owner_node TEXT,
                    storage_key TEXT NOT NULL,
                    deleted BOOLEAN NOT NULL DEFAULT FALSE,
                    version BIGINT NOT NULL DEFAULT 1,
                    lease_until TIMESTAMPTZ
                );
                CREATE TABLE IF NOT EXISTS hyperion_nodes (
                    id TEXT PRIMARY KEY,
                    heartbeat TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    drain BOOLEAN NOT NULL DEFAULT FALSE
                );
                "#,
            )
            .map_err(|e| CatalogError::Postgres(e.to_string()))?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }
}

impl Catalog for PostgresCatalog {
    fn upsert(&self, rec: &ProfileRecord) -> Result<(), CatalogError> {
        let mut c = self.client.lock().unwrap();
        c.execute(
            r#"INSERT INTO hyperion_profiles (id, name, kind, owner_node, storage_key, deleted, version)
               VALUES ($1,$2,$3,$4,$5,$6,$7)
               ON CONFLICT (id) DO UPDATE SET
                 name = EXCLUDED.name,
                 kind = EXCLUDED.kind,
                 owner_node = EXCLUDED.owner_node,
                 storage_key = EXCLUDED.storage_key,
                 deleted = EXCLUDED.deleted,
                 version = EXCLUDED.version"#,
            &[
                &rec.id.as_str(),
                &rec.name,
                &kind_str(rec.kind),
                &rec.owner_node,
                &rec.storage_key,
                &rec.deleted,
                &(rec.version as i64),
            ],
        )
        .map_err(|e| CatalogError::Postgres(e.to_string()))?;
        Ok(())
    }

    fn get(&self, id: &ProfileId) -> Result<Option<ProfileRecord>, CatalogError> {
        let mut c = self.client.lock().unwrap();
        let row = c
            .query_opt(
                "SELECT id, name, kind, owner_node, storage_key, deleted, version FROM hyperion_profiles WHERE id = $1",
                &[&id.as_str()],
            )
            .map_err(|e| CatalogError::Postgres(e.to_string()))?;
        Ok(row.map(|r| row_to_rec(&r)))
    }

    fn list(
        &self,
        offset: usize,
        limit: usize,
        query: Option<&str>,
    ) -> Result<Vec<ProfileRecord>, CatalogError> {
        let mut c = self.client.lock().unwrap();
        let rows = if let Some(q) = query {
            let like = format!("%{q}%");
            c.query(
                "SELECT id, name, kind, owner_node, storage_key, deleted, version
                 FROM hyperion_profiles WHERE deleted = FALSE AND (name ILIKE $1 OR id ILIKE $1)
                 ORDER BY name LIMIT $2 OFFSET $3",
                &[&like, &(limit as i64), &(offset as i64)],
            )
        } else {
            c.query(
                "SELECT id, name, kind, owner_node, storage_key, deleted, version
                 FROM hyperion_profiles WHERE deleted = FALSE
                 ORDER BY name LIMIT $1 OFFSET $2",
                &[&(limit as i64), &(offset as i64)],
            )
        }
        .map_err(|e| CatalogError::Postgres(e.to_string()))?;
        Ok(rows.iter().map(row_to_rec).collect())
    }

    fn acquire_lease(&self, id: &ProfileId, node: &str) -> Result<(), CatalogError> {
        let n = {
            let mut c = self.client.lock().unwrap();
            c.execute(
                r#"UPDATE hyperion_profiles SET owner_node = $2, lease_until = NOW() + INTERVAL '60 seconds'
                   WHERE id = $1 AND deleted = FALSE
                     AND (owner_node IS NULL OR owner_node = $2
                          OR lease_until IS NULL OR lease_until < NOW())"#,
                &[&id.as_str(), &node],
            )
            .map_err(|e| CatalogError::Postgres(e.to_string()))?
        };
        if n == 0 {
            if let Some(rec) = self.get(id)?
                && let Some(owner) = rec.owner_node
            {
                return Err(CatalogError::LeaseHeld(owner));
            }
            return Err(CatalogError::NotFound);
        }
        Ok(())
    }

    fn release_lease(&self, id: &ProfileId, node: &str) -> Result<(), CatalogError> {
        let mut c = self.client.lock().unwrap();
        c.execute(
            "UPDATE hyperion_profiles SET owner_node = NULL, lease_until = NULL WHERE id = $1 AND owner_node = $2",
            &[&id.as_str(), &node],
        )
        .map_err(|e| CatalogError::Postgres(e.to_string()))?;
        Ok(())
    }

    fn tombstone(&self, id: &ProfileId) -> Result<(), CatalogError> {
        let mut c = self.client.lock().unwrap();
        c.execute(
            "UPDATE hyperion_profiles SET deleted = TRUE, owner_node = NULL WHERE id = $1",
            &[&id.as_str()],
        )
        .map_err(|e| CatalogError::Postgres(e.to_string()))?;
        Ok(())
    }

    fn heartbeat(&self, node: &str) -> Result<(), CatalogError> {
        let mut c = self.client.lock().unwrap();
        c.execute(
            r#"INSERT INTO hyperion_nodes (id, heartbeat, drain) VALUES ($1, NOW(), FALSE)
               ON CONFLICT (id) DO UPDATE SET heartbeat = NOW()"#,
            &[&node],
        )
        .map_err(|e| CatalogError::Postgres(e.to_string()))?;
        Ok(())
    }

    fn nodes(&self) -> Result<Vec<String>, CatalogError> {
        let mut c = self.client.lock().unwrap();
        let rows = c
            .query(
                "SELECT id FROM hyperion_nodes WHERE heartbeat > NOW() - INTERVAL '30 seconds'",
                &[],
            )
            .map_err(|e| CatalogError::Postgres(e.to_string()))?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }
}

fn kind_str(k: ProfileKind) -> &'static str {
    match k {
        ProfileKind::Persistent => "persistent",
        ProfileKind::Ephemeral => "ephemeral",
    }
}

fn parse_kind(s: &str) -> ProfileKind {
    if s.eq_ignore_ascii_case("ephemeral") {
        ProfileKind::Ephemeral
    } else {
        ProfileKind::Persistent
    }
}

fn row_to_rec(r: &postgres::Row) -> ProfileRecord {
    let id: String = r.get(0);
    let kind: String = r.get(2);
    ProfileRecord {
        id: ProfileId(id),
        name: r.get(1),
        kind: parse_kind(&kind),
        owner_node: r.get(3),
        storage_key: r.get(4),
        deleted: r.get(5),
        version: r.get::<_, i64>(6) as u64,
    }
}
