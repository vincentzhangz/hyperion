//! Isolated browser profiles: cookies, storage, settings, and persistence.

mod catalog;
mod crypto;
mod manager;
#[cfg(feature = "postgres")]
mod postgres;
mod snapshot;
mod store;

pub use catalog::{Catalog, CatalogError, MemoryCatalog, ProfileRecord};
pub use crypto::{Key, decrypt, encrypt};
pub use manager::{Profile, ProfileError, ProfileId, ProfileKind, ProfileManager};
#[cfg(feature = "postgres")]
pub use postgres::PostgresCatalog;
pub use snapshot::{ProfileSettings, ProfileSnapshot, SNAPSHOT_VERSION};
pub use store::{LocalStore, MemoryStore, ProfileStore, S3Store, StoreError};
