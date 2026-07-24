use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use crate::{
    app_error::ServerError, cached::Cached, npm::package_content::PackageContentFetcher,
    npm_replicator::database::NpmDatabase,
};
use moka::future::Cache;
use tracing::warn;

use super::disk_cache;
use super::process::{process_npm_package, MinimalCachedModule, ModuleDependenciesMap};

pub type Content = Arc<(MinimalCachedModule, ModuleDependenciesMap)>;

#[tracing::instrument(
    name = "get_processed_pkg",
    skip(temp_dir, package_cache_dir, cached, npm_db, content_fetcher)
)]
async fn get_processed_pkg(
    package_name: &str,
    package_version: &str,
    temp_dir: &str,
    package_cache_dir: PathBuf,
    cached: Cached<Content>,
    npm_db: NpmDatabase,
    content_fetcher: PackageContentFetcher,
) -> Result<Content, ServerError> {
    let package_name = String::from(package_name);
    let package_version = String::from(package_version);
    let temp_dir = String::from(temp_dir);
    let res = cached
        .get_cached(|_last_val| {
            Box::pin(async move {
                // Exact versions are immutable — prefer the on-disk transform
                // cache (survives CDN process restarts / prewarm).
                if let Some((module, deps)) =
                    disk_cache::load(&package_cache_dir, &package_name, &package_version)?
                {
                    return Ok::<_, ServerError>(Arc::new((module, deps)));
                }

                let content = process_npm_package(
                    &package_name,
                    &package_version,
                    &temp_dir,
                    &npm_db,
                    &content_fetcher,
                )
                .await?;

                if let Err(err) = disk_cache::store(
                    &package_cache_dir,
                    &package_name,
                    &package_version,
                    &content.0,
                    &content.1,
                ) {
                    // Non-fatal: still serve the freshly transformed package.
                    warn!(
                        "Failed to persist package cache for {}@{}: {:?}",
                        package_name, package_version, err
                    );
                }

                Ok::<_, ServerError>(Arc::new(content))
            })
        })
        .await?;

    Ok(res)
}

#[derive(Clone)]
pub struct CachedPackageProcessor {
    cache: Cache<String, Cached<Content>>,
    refresh_interval: Duration,
    npm_db: NpmDatabase,
    content_fetcher: PackageContentFetcher,
    temp_dir: String,
    package_cache_dir: PathBuf,
}

impl CachedPackageProcessor {
    pub fn new(
        npm_db: NpmDatabase,
        content_fetcher: PackageContentFetcher,
        temp_dir: &str,
        package_cache_dir: PathBuf,
    ) -> CachedPackageProcessor {
        if let Err(err) = disk_cache::ensure_dir(&package_cache_dir) {
            warn!(
                "Could not create package cache dir {}: {:?}",
                package_cache_dir.display(),
                err
            );
        }

        let ttl = Duration::from_secs(86400);
        let max_capacity = 250;
        CachedPackageProcessor {
            cache: Cache::builder()
                .max_capacity(max_capacity)
                .time_to_idle(ttl)
                .build(),
            // Long enough that warm hits stay in memory; disk covers restarts.
            refresh_interval: Duration::from_secs(604800),
            npm_db,
            content_fetcher,
            temp_dir: String::from(temp_dir),
            package_cache_dir,
        }
    }

    #[tracing::instrument(name = "processed_pkg_get", skip(self))]
    pub async fn get(
        &self,
        package_name: &str,
        package_version: &str,
    ) -> Result<Content, ServerError> {
        let key = format!("{}@{}", package_name, package_version);
        if let Some(found_value) = self.cache.get(&key).await {
            get_processed_pkg(
                package_name,
                package_version,
                &self.temp_dir,
                self.package_cache_dir.clone(),
                found_value,
                self.npm_db.clone(),
                self.content_fetcher.clone(),
            )
            .await
        } else {
            let cached: Cached<Content> = Cached::new(self.refresh_interval);
            self.cache.insert(key, cached.clone()).await;
            get_processed_pkg(
                package_name,
                package_version,
                &self.temp_dir,
                self.package_cache_dir.clone(),
                cached,
                self.npm_db.clone(),
                self.content_fetcher.clone(),
            )
            .await
        }
    }
}

impl fmt::Debug for CachedPackageProcessor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CachedPackageProcessor")
    }
}
