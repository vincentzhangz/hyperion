use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manager::ProfileId;
use hyperion_net::cookie::CookieEntry;

pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    pub version: u32,
    pub id: ProfileId,
    pub name: String,
    pub settings: ProfileSettings,
    pub cookies: Vec<CookieEntry>,
    pub local_storage: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    #[serde(default)]
    pub checksum: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileSettings {
    pub user_agent: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    pub locale: String,
    pub timezone: String,
    pub viewport: (f64, f64, f64),
    pub proxy: Option<String>,
    pub allow_private_network: bool,
}

impl Default for ProfileSettings {
    fn default() -> Self {
        Self {
            user_agent: None,
            extra_headers: Vec::new(),
            locale: "en-US".into(),
            timezone: "UTC".into(),
            viewport: (980.0, 1024.0, 1.0),
            proxy: None,
            allow_private_network: false,
        }
    }
}

impl ProfileSnapshot {
    pub fn with_checksum(mut self) -> Self {
        self.checksum.clear();
        let body = serde_json::to_vec(&self).unwrap_or_default();
        self.checksum = hex::encode(Sha256::digest(&body));
        self
    }

    pub fn verify(&self) -> Result<(), &'static str> {
        if self.version > SNAPSHOT_VERSION {
            return Err("unsupported snapshot version");
        }
        if self.checksum.is_empty() {
            return Ok(());
        }
        let mut clone = self.clone();
        clone.checksum.clear();
        let body = serde_json::to_vec(&clone).unwrap_or_default();
        let expect = hex::encode(Sha256::digest(&body));
        if expect != self.checksum {
            return Err("checksum mismatch");
        }
        Ok(())
    }
}
