//! On-disk cache of transformed packages (`MinimalCachedModule` + dependency map).
//!
//! Exact `name@version` artifacts are immutable on npm, so entries never expire.
//! Layout: `{cache_dir}/<safe-name>@<version>.v1.msgpack` under the CDN data dir
//! (Priprava: `Tools/SandpackCdn/data/package_cache/`).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::app_error::ServerError;

use super::process::{MinimalCachedModule, ModuleDependenciesMap};

/// Bump when the on-disk envelope / module shape changes (triggers rebuild).
/// v2: inject_helpers before common_js → bare `_interop_require_*` (broken).
/// v3: dual common_js + external @swc/helpers (worked, but wrong model).
/// v4: inline helpers via `ecma_helpers_inline` (common_js → inject_helpers).
const DISK_FORMAT: u32 = 4;

#[derive(Debug, Serialize, Deserialize)]
struct DiskEnvelope {
    #[serde(rename = "v")]
    format: u32,
    #[serde(rename = "m")]
    module: MinimalCachedModule,
    #[serde(rename = "d")]
    dependencies: ModuleDependenciesMap,
}

/// Safe file name for `name@version` (scoped packages use `/`).
fn cache_file_name(package_name: &str, package_version: &str) -> String {
    let safe_name = package_name.replace('/', "__");
    format!("{safe_name}@{package_version}.v{DISK_FORMAT}.msgpack")
}

fn cache_path(cache_dir: &Path, package_name: &str, package_version: &str) -> PathBuf {
    cache_dir.join(cache_file_name(package_name, package_version))
}

/// Ensure the cache directory exists.
pub fn ensure_dir(cache_dir: &Path) -> Result<(), ServerError> {
    fs::create_dir_all(cache_dir)?;
    Ok(())
}

/// Load a previously transformed package from disk, if present and valid.
pub fn load(
    cache_dir: &Path,
    package_name: &str,
    package_version: &str,
) -> Result<Option<(MinimalCachedModule, ModuleDependenciesMap)>, ServerError> {
    let path = cache_path(cache_dir, package_name, package_version);
    if !path.is_file() {
        return Ok(None);
    }

    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(err) => {
            warn!(
                "Failed to read package cache {}: {} — will reprocess",
                path.display(),
                err
            );
            return Ok(None);
        }
    };

    match crate::utils::msgpack::deserialize_msgpack::<DiskEnvelope>(&bytes) {
        Ok(env) if env.format == DISK_FORMAT => {
            debug!(
                "package cache hit {}@{}",
                package_name, package_version
            );
            Ok(Some((env.module, env.dependencies)))
        }
        Ok(env) => {
            warn!(
                "package cache format {} != {} for {}@{} — will reprocess",
                env.format, DISK_FORMAT, package_name, package_version
            );
            let _ = fs::remove_file(&path);
            Ok(None)
        }
        Err(err) => {
            warn!(
                "Corrupt package cache {} ({}): will reprocess",
                path.display(),
                err
            );
            let _ = fs::remove_file(&path);
            Ok(None)
        }
    }
}

/// Persist a transformed package for the next CDN process / prewarm.
pub fn store(
    cache_dir: &Path,
    package_name: &str,
    package_version: &str,
    module: &MinimalCachedModule,
    dependencies: &ModuleDependenciesMap,
) -> Result<(), ServerError> {
    ensure_dir(cache_dir)?;
    let path = cache_path(cache_dir, package_name, package_version);
    let envelope = DiskEnvelope {
        format: DISK_FORMAT,
        module: module.clone(),
        dependencies: dependencies.clone(),
    };
    let bytes = crate::utils::msgpack::serialize_msgpack(&envelope)?;

    // Write via temp + rename so readers never see a partial file.
    let tmp = path.with_extension("msgpack.tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, &path)?;
    debug!(
        "package cache store {}@{} ({} bytes)",
        package_name,
        package_version,
        bytes.len()
    );
    Ok(())
}
