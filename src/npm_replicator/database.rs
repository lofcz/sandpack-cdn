use std::num::NonZeroUsize;
use std::sync::{Arc, LazyLock};

use lru::LruCache;
use parking_lot::Mutex;
use rusqlite::{named_params, Connection, OpenFlags, OptionalExtension};

use crate::app_error::{AppResult, ServerError};

use super::types::document::{MinimalPackageData, RegistryDocument};

/// Shared client for on-demand npm registry lookups. We fetch package
/// manifests lazily (only the packages users actually import) instead of
/// replicating the entire npm registry.
static NPM_HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("sandpack-cdn")
        .build()
        .expect("failed to build npm registry http client")
});

const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org";

#[derive(Clone, Debug)]
pub struct NpmDatabase {
    db: Arc<Mutex<Connection>>,
    cache: Arc<Mutex<LruCache<String, MinimalPackageData>>>,
}

impl NpmDatabase {
    pub fn new(db_path: &str) -> AppResult<Self> {
        let connection = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        let cache = LruCache::new(NonZeroUsize::new(500).unwrap());

        Ok(Self {
            db: Arc::new(Mutex::new(connection)),
            cache: Arc::new(Mutex::new(cache)),
        })
    }

    pub fn init(&self) -> AppResult<()> {
        let connection = self.db.lock();

        connection.execute(
            "CREATE TABLE IF NOT EXISTS package (
                id    TEXT PRIMARY KEY,
                content  TEXT NOT NULL
            );",
            (),
        )?;

        Ok(())
    }

    pub fn delete_package(&self, name: &str) -> AppResult<usize> {
        let connection = self.db.lock();
        let mut stmt = connection.prepare("DELETE FROM package WHERE id = (:id);")?;
        let res = stmt.execute(named_params! { ":id": name })?;
        let mut cache = self.cache.lock();
        cache.pop(name);
        Ok(res)
    }

    pub fn write_package(&self, pkg: MinimalPackageData) -> AppResult<usize> {
        if pkg.versions.is_empty() {
            println!("Tried to write pkg {}, but has no versions", pkg.name);
            return self.delete_package(&pkg.name);
        }

        let pkg_name = pkg.name.clone();
        let content = serde_json::to_string(&pkg)?;
        let res = {
            let connection = self.db.lock();
            let mut stmt = connection
                .prepare("INSERT OR REPLACE INTO package (id, content) VALUES (:id, :content);")?;
            stmt.execute(named_params! { ":id": pkg.name, ":content": content })
        }?;

        let mut cache = self.cache.lock();
        cache.pop(&pkg_name);

        Ok(res)
    }

    pub fn get_package(&self, name: &str) -> AppResult<MinimalPackageData> {
        {
            let mut cache = self.cache.lock();
            let cached_value = cache.get(name);
            if let Some(pkg_data) = cached_value {
                return Ok(pkg_data.clone());
            }
        };

        let content_val: Option<String> = {
            let connection = self.db.lock();
            let mut stmt = connection.prepare("SELECT content FROM package where id = (:id);")?;
            stmt.query_row(named_params! { ":id": name }, |row| row.get(0))
                .optional()?
        };

        if let Some(pkg_content) = content_val {
            let found_pkg: MinimalPackageData = serde_json::from_str(&pkg_content)?;
            let mut cache = self.cache.lock();
            cache.put(name.to_string(), found_pkg.clone());
            Ok(found_pkg)
        } else {
            Err(crate::app_error::ServerError::PackageNotFound(
                name.to_string(),
            ))
        }
    }

    /// Returns true if the package manifest is already known locally (cache or
    /// SQLite). Cheap, synchronous, holds no lock across an await.
    fn has_package(&self, name: &str) -> bool {
        {
            let mut cache = self.cache.lock();
            if cache.get(name).is_some() {
                return true;
            }
        }

        let connection = self.db.lock();
        let exists = match connection.prepare("SELECT 1 FROM package WHERE id = (:id) LIMIT 1;") {
            Ok(mut stmt) => stmt
                .exists(named_params! { ":id": name })
                .unwrap_or(false),
            Err(_) => false,
        };
        exists
    }

    /// Download a single package manifest from the npm registry and persist it.
    /// Reuses `MinimalPackageData::from_doc`, which parses exactly the packument
    /// shape that `registry.npmjs.org/<pkg>` returns.
    async fn fetch_and_store(&self, name: &str) -> AppResult<()> {
        let url = format!("{}/{}", NPM_REGISTRY_URL, name.replace('/', "%2f"));
        let response = NPM_HTTP
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(ServerError::NpmManifestDownloadError {
                status_code: response.status().as_u16(),
                package_name: name.to_string(),
            });
        }

        let body = response.text().await?;
        let doc: RegistryDocument = serde_json::from_str(&body)?;
        self.write_package(MinimalPackageData::from_doc(doc))?;
        Ok(())
    }

    /// Ensure a package manifest exists locally, fetching it on demand from the
    /// npm registry if missing. This replaces the old full-registry replication
    /// thread: we only ever store the packages that get imported.
    pub async fn ensure_package(&self, name: &str) -> AppResult<()> {
        if self.has_package(name) {
            return Ok(());
        }
        self.fetch_and_store(name).await
    }

    /// Cheap liveness probe used by `/health`. An empty database is healthy;
    /// an unreadable one is not.
    pub fn ping(&self) -> AppResult<()> {
        let connection = self.db.lock();
        connection.query_row("SELECT 1", [], |_| Ok(()))?;
        Ok(())
    }
}
