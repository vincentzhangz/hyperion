use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("not found")]
    NotFound,
    #[error("path escape")]
    PathEscape,
    #[error("s3: {0}")]
    S3(String),
}

pub trait ProfileStore: Send + Sync {
    fn save(&self, key: &str, blob: &[u8]) -> Result<(), StoreError>;
    fn load(&self, key: &str) -> Result<Vec<u8>, StoreError>;
    fn delete(&self, key: &str) -> Result<(), StoreError>;
}

#[derive(Clone, Default)]
pub struct MemoryStore {
    inner: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ProfileStore for MemoryStore {
    fn save(&self, key: &str, blob: &[u8]) -> Result<(), StoreError> {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_owned(), blob.to_vec());
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        self.inner
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or(StoreError::NotFound)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.inner.lock().unwrap().remove(key);
        Ok(())
    }
}

pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        }
        Ok(Self { root })
    }

    fn path_for(&self, key: &str) -> Result<PathBuf, StoreError> {
        if key.contains("..") || key.contains('/') || key.contains('\\') {
            return Err(StoreError::PathEscape);
        }
        let p = self.root.join(format!("{key}.bin"));
        let canon_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        if let Some(parent) = p.parent()
            && let Ok(c) = parent.canonicalize()
            && !c.starts_with(&canon_root)
        {
            return Err(StoreError::PathEscape);
        }
        Ok(p)
    }
}

impl ProfileStore for LocalStore {
    fn save(&self, key: &str, blob: &[u8]) -> Result<(), StoreError> {
        let dest = self.path_for(key)?;
        let tmp = dest.with_extension("bin.tmp");
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(blob)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &dest)?;
        if let Ok(dir) = fs::File::open(&self.root) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let dest = self.path_for(key)?;
        fs::read(&dest).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                StoreError::NotFound
            } else {
                StoreError::Io(e)
            }
        })
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let dest = self.path_for(key)?;
        match fs::remove_file(&dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(e)),
        }
    }
}

/// Minimal S3-compatible store using blocking HTTPS (SigV4).
pub struct S3Store {
    endpoint: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
    cache: MemoryStore,
}

impl S3Store {
    pub fn from_config(cfg: &hyperion_config::S3Config) -> Result<Self, StoreError> {
        Ok(Self {
            endpoint: cfg
                .endpoint
                .clone()
                .unwrap_or_else(|| format!("https://s3.{}.amazonaws.com", cfg.region)),
            bucket: cfg
                .bucket
                .clone()
                .ok_or_else(|| StoreError::S3("missing bucket".into()))?,
            region: cfg.region.clone(),
            access_key: cfg
                .access_key
                .clone()
                .ok_or_else(|| StoreError::S3("missing access key".into()))?,
            secret_key: cfg.secret_key.clone().unwrap_or_default(),
            cache: MemoryStore::new(),
        })
    }

    fn object_url(&self, key: &str) -> String {
        format!(
            "{}/{}/profiles/{key}.bin",
            self.endpoint.trim_end_matches('/'),
            self.bucket
        )
    }
}

impl ProfileStore for S3Store {
    fn save(&self, key: &str, blob: &[u8]) -> Result<(), StoreError> {
        self.cache.save(key, blob)?;
        let url = self.object_url(key);
        match signed_put(
            &url,
            blob,
            &self.access_key,
            &self.secret_key,
            &self.region,
            &self.bucket,
            key,
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Cache remains; surface the remote error so operators can alert.
                Err(StoreError::S3(e))
            }
        }
    }

    fn load(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        if let Ok(v) = self.cache.load(key) {
            return Ok(v);
        }
        let url = self.object_url(key);
        let body = signed_get(
            &url,
            &self.access_key,
            &self.secret_key,
            &self.region,
            &self.bucket,
            key,
        )?;
        self.cache.save(key, &body)?;
        Ok(body)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let _ = self.cache.delete(key);
        let url = self.object_url(key);
        signed_delete(
            &url,
            &self.access_key,
            &self.secret_key,
            &self.region,
            &self.bucket,
            key,
        )
    }
}

fn signed_put(
    url: &str,
    body: &[u8],
    access: &str,
    secret: &str,
    region: &str,
    bucket: &str,
    key: &str,
) -> Result<(), String> {
    let object = format!("profiles/{key}.bin");
    let host = host_from_url(url)?;
    let canonical_uri = format!("/{bucket}/{object}");
    let (auth, amz_date, sha) = sigv4("PUT", &canonical_uri, body, access, secret, region, &host)?;
    let resp = ureq::put(url)
        .set("Authorization", &auth)
        .set("x-amz-date", &amz_date)
        .set("x-amz-content-sha256", &sha)
        .set("Content-Type", "application/octet-stream")
        .send_bytes(body)
        .map_err(|e| e.to_string())?;
    if resp.status() >= 300 {
        return Err(format!("s3 put status {}", resp.status()));
    }
    Ok(())
}

fn signed_get(
    url: &str,
    access: &str,
    secret: &str,
    region: &str,
    bucket: &str,
    key: &str,
) -> Result<Vec<u8>, StoreError> {
    let object = format!("profiles/{key}.bin");
    let host = host_from_url(url).map_err(StoreError::S3)?;
    let canonical_uri = format!("/{bucket}/{object}");
    let (auth, amz_date, sha) =
        sigv4("GET", &canonical_uri, b"", access, secret, region, &host).map_err(StoreError::S3)?;
    let resp = ureq::get(url)
        .set("Authorization", &auth)
        .set("x-amz-date", &amz_date)
        .set("x-amz-content-sha256", &sha)
        .call()
        .map_err(|e| StoreError::S3(e.to_string()))?;
    if resp.status() == 404 {
        return Err(StoreError::NotFound);
    }
    if resp.status() >= 300 {
        return Err(StoreError::S3(format!("s3 get status {}", resp.status())));
    }
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(StoreError::Io)?;
    Ok(buf)
}

fn signed_delete(
    url: &str,
    access: &str,
    secret: &str,
    region: &str,
    bucket: &str,
    key: &str,
) -> Result<(), StoreError> {
    let object = format!("profiles/{key}.bin");
    let host = host_from_url(url).map_err(StoreError::S3)?;
    let canonical_uri = format!("/{bucket}/{object}");
    let (auth, amz_date, sha) = sigv4("DELETE", &canonical_uri, b"", access, secret, region, &host)
        .map_err(StoreError::S3)?;
    match ureq::delete(url)
        .set("Authorization", &auth)
        .set("x-amz-date", &amz_date)
        .set("x-amz-content-sha256", &sha)
        .call()
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(404, _)) => Ok(()),
        Err(e) => Err(StoreError::S3(e.to_string())),
    }
}

fn host_from_url(url: &str) -> Result<String, String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or(rest);
    if host.is_empty() {
        return Err("s3 url missing host".into());
    }
    Ok(host.to_owned())
}

fn sigv4(
    method: &str,
    canonical_uri: &str,
    body: &[u8],
    access: &str,
    secret: &str,
    region: &str,
    host: &str,
) -> Result<(String, String, String), String> {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    type HmacSha256 = Hmac<Sha256>;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // YYYYMMDDTHHMMSSZ — good enough without chrono.
    let tm = time_parts(secs);
    let amz_date = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        tm.0, tm.1, tm.2, tm.3, tm.4, tm.5
    );
    let date_stamp = format!("{:04}{:02}{:02}", tm.0, tm.1, tm.2);
    let payload_hash = hex::encode(Sha256::digest(body));
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let canonical_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{canonical_hash}");

    let mut k_date = HmacSha256::new_from_slice(format!("AWS4{secret}").as_bytes())
        .map_err(|e| e.to_string())?;
    k_date.update(date_stamp.as_bytes());
    let mut k_region =
        HmacSha256::new_from_slice(&k_date.finalize().into_bytes()).map_err(|e| e.to_string())?;
    k_region.update(region.as_bytes());
    let mut k_service =
        HmacSha256::new_from_slice(&k_region.finalize().into_bytes()).map_err(|e| e.to_string())?;
    k_service.update(b"s3");
    let mut k_signing = HmacSha256::new_from_slice(&k_service.finalize().into_bytes())
        .map_err(|e| e.to_string())?;
    k_signing.update(b"aws4_request");
    let mut sig = HmacSha256::new_from_slice(&k_signing.finalize().into_bytes())
        .map_err(|e| e.to_string())?;
    sig.update(string_to_sign.as_bytes());
    let signature = hex::encode(sig.finalize().into_bytes());
    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );
    Ok((auth, amz_date, payload_hash))
}

fn time_parts(unix: u64) -> (u32, u32, u32, u32, u32, u32) {
    // Civil date from unix seconds (UTC), Howard Hinnant algorithm.
    let z = (unix / 86400) as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let rem = unix % 86400;
    let h = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;
    (y as u32, m as u32, d as u32, h, min, s)
}

#[allow(dead_code)]
pub fn ensure_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}
