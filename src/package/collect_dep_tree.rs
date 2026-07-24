use node_semver::{Range, Version};
use parking_lot::Mutex;
use serde::{self, Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use tokio::task::JoinHandle;
use tracing::{error, warn};

use crate::{
    app_error::ServerError,
    npm_replicator::{database::NpmDatabase, types::document::MinimalPackageData},
};

use super::cached::CachedPackageProcessor;

#[derive(Clone, Debug)]
pub enum VersionRange {
    Range(Range),
    Alias(String),
}

#[derive(Clone, Debug)]
pub struct DependencyRequest {
    name: String,
    version_range: VersionRange,
    /// Original range/tag string, kept for accurate PackageVersionNotFound messages.
    range_str: String,
    depth: u32,
}

impl DependencyRequest {
    pub fn new(name: &str, version_range_str: &str, depth: u32) -> Result<Self, ServerError> {
        // TODO: Handle aliases, "react": "npm:preact@^7.0.0"
        let version_range = match Range::parse(version_range_str) {
            Ok(req) => VersionRange::Range(req),
            Err(_) => VersionRange::Alias(String::from(version_range_str)),
        };

        Ok(DependencyRequest {
            name: String::from(name),
            version_range,
            range_str: String::from(version_range_str),
            depth,
        })
    }

    pub fn resolve_version(&self, manifest: &MinimalPackageData) -> Option<String> {
        match &self.version_range {
            VersionRange::Alias(alias_str) => manifest.dist_tags.get(alias_str).cloned(),
            VersionRange::Range(range) => {
                // The versions map is keyed by string, so its order is lexicographic
                // ("1.9.0" > "1.10.0", "9.0.0" > "10.0.0"); pick the semver-maximum
                // matching version instead of trusting that order. Malformed version
                // keys are skipped rather than failing the whole resolution.
                let highest_version: Option<Version> = manifest
                    .versions
                    .keys()
                    .filter_map(|version| match Version::parse(version) {
                        Ok(parsed) => Some(parsed),
                        Err(_) => {
                            warn!(
                                "Skipping malformed version '{}' of package {}",
                                version, self.name
                            );
                            None
                        }
                    })
                    .filter(|parsed| range.satisfies(parsed))
                    .max();

                highest_version.map(|v| v.to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    #[serde(rename = "n")]
    name: String,
    #[serde(rename = "v")]
    version: String,
    #[serde(rename = "d")]
    depth: u32,
}

impl Dependency {
    pub fn new(name: String, version: String, depth: u32) -> Self {
        Dependency {
            name,
            version,
            depth,
        }
    }
}

pub type DependencyList = Vec<Dependency>;

pub fn process_dep_map(
    dep_map: HashMap<String, String>,
    depth: u32,
) -> Result<Vec<DependencyRequest>, ServerError> {
    let mut deps: Vec<DependencyRequest> = Vec::with_capacity(dep_map.len());
    for (key, val) in dep_map.iter() {
        match DependencyRequest::new(key.as_str(), val.as_str(), depth) {
            Ok(dep) => {
                deps.push(dep);
            }
            Err(err) => {
                error!(
                    "Failed to parse dep range {} for {}. {:?}",
                    val.as_str(),
                    key.as_str(),
                    err
                )
            }
        }
    }
    Ok(deps)
}

type ResolveDepResult = Result<(Dependency, Vec<DependencyRequest>), ServerError>;

#[tracing::instrument(name = "resolve_dep", skip(pkg_processor, npm_db))]
async fn resolve_dep(
    req: DependencyRequest,
    npm_db: &NpmDatabase,
    pkg_processor: &CachedPackageProcessor,
) -> ResolveDepResult {
    npm_db.ensure_package(&req.name).await?;
    let manifest = npm_db.get_package(&req.name)?;
    let Some(resolved_version) = req.resolve_version(&manifest) else {
        return Err(ServerError::PackageVersionNotFound(
            req.name,
            req.range_str,
        ));
    };

    let dependencies = pkg_processor
        .get(req.name.as_str(), resolved_version.as_str())
        .await?;
    let mut transient_deps: Vec<DependencyRequest> = Vec::with_capacity(dependencies.1.len());
    for (dep_name, dep_meta) in dependencies.1.iter() {
        if dep_meta.is_used {
            let dep_req_res = DependencyRequest::new(
                dep_name.as_str(),
                dep_meta.version.as_str(),
                req.depth + 1,
            );
            if let Ok(dep_req) = dep_req_res {
                transient_deps.push(dep_req);
            }
        }
    }

    Ok((
        Dependency::new(req.name, resolved_version, req.depth),
        transient_deps,
    ))
}

#[derive(Debug, Clone)]
struct DepTreeCollector {
    npm_db: NpmDatabase,
    dependencies: Arc<Mutex<DependencyList>>,
    futures: Arc<Mutex<VecDeque<JoinHandle<()>>>>,
    in_progress: Arc<Mutex<Vec<DependencyRequest>>>,
    /// First resolution error wins; surfaced after all spawned work joins.
    first_error: Arc<Mutex<Option<ServerError>>>,
    pkg_processor: CachedPackageProcessor,
}

impl DepTreeCollector {
    pub fn new(npm_db: NpmDatabase, pkg_processor: CachedPackageProcessor) -> Self {
        DepTreeCollector {
            npm_db,
            pkg_processor,
            dependencies: Arc::new(Mutex::new(Vec::new())),
            futures: Arc::new(Mutex::new(VecDeque::new())),
            in_progress: Arc::new(Mutex::new(Vec::new())),
            first_error: Arc::new(Mutex::new(None)),
        }
    }

    fn get_dependencies(&self) -> DependencyList {
        self.dependencies.lock().clone()
    }

    fn add_dependency(&self, dep: Dependency) {
        if !self
            .dependencies
            .lock()
            .iter()
            .any(|d| d.name.eq(&dep.name))
        {
            self.dependencies.lock().push(dep);
        }
    }

    fn record_error(&self, err: ServerError) {
        let mut slot = self.first_error.lock();
        if slot.is_none() {
            *slot = Some(err);
        }
    }

    fn add_future(&self, dep_req: DependencyRequest) {
        let dep_collector = self.clone();
        let pkg_processor = self.pkg_processor.clone();
        let future = tokio::spawn(async move {
            let npm_db = dep_collector.npm_db.clone();
            match resolve_dep(dep_req, &npm_db, &pkg_processor).await {
                Ok((dependency, transient_deps)) => {
                    dep_collector.add_dependency(dependency);
                    dep_collector.add_dep_requests(transient_deps);
                }
                Err(err) => {
                    dep_collector.record_error(err);
                }
            }
        });
        self.futures.lock().push_back(future);
    }

    fn has_dep_request(&self, dep_request: DependencyRequest) -> bool {
        if self
            .dependencies
            .lock()
            .iter()
            .any(|d| d.name.eq(&dep_request.name))
        {
            return true;
        }

        if self
            .in_progress
            .lock()
            .iter()
            .any(|d| d.name.eq(&dep_request.name))
        {
            return true;
        }

        false
    }

    fn total_dep_count(&self) -> u64 {
        (self.dependencies.lock().len() + self.in_progress.lock().len()) as u64
    }

    fn should_skip_dep_request(&self, dep_request: DependencyRequest) -> bool {
        // Skip packages that start with @types/ as they don't contain any useful code, just typings...
        if dep_request.name.as_str().starts_with("@types/") {
            return true;
        }

        // Stale cache / bad edges: only well-formed package names are resolvable.
        if !super::npm_specifier::is_valid_package_name(dep_request.name.as_str()) {
            warn!(
                "Skipping non-npm dependency specifier {:?}",
                dep_request.name
            );
            return true;
        }

        // Add a limit to the total amount of deps
        if self.total_dep_count() > 500 {
            return true;
        }

        self.has_dep_request(dep_request)
    }

    fn add_dep_request(&self, dep_request: DependencyRequest) {
        self.in_progress.lock().push(dep_request);
    }

    fn add_dep_requests(&self, dep_requests: Vec<DependencyRequest>) {
        for dep_req in dep_requests {
            if !self.should_skip_dep_request(dep_req.clone()) {
                self.add_future(dep_req.clone());
                self.add_dep_request(dep_req.clone());
            }
        }
    }

    fn get_next_join(&self) -> Option<JoinHandle<()>> {
        self.futures.lock().pop_front()
    }

    pub async fn try_collect(
        dep_requests: Vec<DependencyRequest>,
        npm_db: NpmDatabase,
        pkg_processor: CachedPackageProcessor,
    ) -> Result<DependencyList, ServerError> {
        let collector = DepTreeCollector::new(npm_db, pkg_processor);
        collector.add_dep_requests(dep_requests);

        while let Some(handle) = collector.get_next_join() {
            if let Err(err) = handle.await {
                error!("Dependency collection error {:?}", err);
            }
        }

        if let Some(err) = collector.first_error.lock().take() {
            return Err(err);
        }

        Ok(collector.get_dependencies())
    }
}

pub async fn collect_dep_tree(
    dep_requests: Vec<DependencyRequest>,
    npm_db: &NpmDatabase,
    pkg_processor: &CachedPackageProcessor,
) -> Result<DependencyList, ServerError> {
    DepTreeCollector::try_collect(dep_requests, npm_db.clone(), pkg_processor.clone()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::npm_replicator::types::document::{
        MinimalPackageData, MinimalPackageVersionData,
    };
    use std::collections::{BTreeMap, HashMap};

    fn pkg_with_versions(name: &str, versions: &[&str]) -> MinimalPackageData {
        let mut map = BTreeMap::new();
        for version in versions {
            map.insert(
                (*version).to_string(),
                MinimalPackageVersionData {
                    tarball: format!("https://example.com/{name}/{version}.tgz"),
                    dependencies: HashMap::new(),
                },
            );
        }
        MinimalPackageData {
            name: name.to_string(),
            dist_tags: HashMap::new(),
            versions: map,
        }
    }

    #[test]
    fn caret_range_resolves_semver_max() {
        // "1.10.0" < "1.9.0" lexicographically; must still pick semver-highest.
        let pkg = pkg_with_versions("pkg", &["1.9.0", "1.10.0"]);
        let req = DependencyRequest::new("pkg", "^1.0.0", 0).unwrap();
        assert_eq!(req.resolve_version(&pkg).as_deref(), Some("1.10.0"));
    }

    #[test]
    fn star_range_resolves_semver_max() {
        // "9.0.0" > "10.0.0" lexicographically; wildcard must still pick 10.x.
        let pkg = pkg_with_versions("pkg", &["9.0.0", "10.0.0"]);
        let req = DependencyRequest::new("pkg", "*", 0).unwrap();
        assert_eq!(req.resolve_version(&pkg).as_deref(), Some("10.0.0"));
    }

    #[test]
    fn malformed_version_key_is_skipped() {
        let pkg = pkg_with_versions("pkg", &["not-semver", "1.2.3"]);
        let req = DependencyRequest::new("pkg", "^1.0.0", 0).unwrap();
        assert_eq!(req.resolve_version(&pkg).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn missing_exact_version_returns_none() {
        let pkg = pkg_with_versions("@mui/icons-material", &["9.1.1"]);
        let req = DependencyRequest::new("@mui/icons-material", "9.1.2", 0).unwrap();
        assert_eq!(req.resolve_version(&pkg), None);
    }

    #[test]
    fn existing_exact_version_resolves() {
        let pkg = pkg_with_versions("@mui/material", &["9.1.2"]);
        let req = DependencyRequest::new("@mui/material", "9.1.2", 0).unwrap();
        assert_eq!(req.resolve_version(&pkg).as_deref(), Some("9.1.2"));
    }

    #[test]
    fn dist_tag_resolves() {
        let mut pkg = pkg_with_versions("pkg", &["1.9.0", "1.10.0"]);
        pkg.dist_tags
            .insert("latest".to_string(), "1.10.0".to_string());
        let req = DependencyRequest::new("pkg", "latest", 0).unwrap();
        assert_eq!(req.resolve_version(&pkg).as_deref(), Some("1.10.0"));
    }
}
