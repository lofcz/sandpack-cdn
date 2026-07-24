use nanoid::nanoid;
use node_semver::Version;
use serde::{self, Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{error, info, span, Level};
use transform::transformer::transform_file;

use crate::app_error::ServerError;
use crate::npm::package_content::{download_package_content, PackageContentFetcher};
use crate::npm_replicator::database::NpmDatabase;
use crate::transform;
use crate::utils::tar;

use super::package_json::PackageJSON;
use super::{package_json, resolver};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MinimalFile {
    // This file got used so we transform it or return it
    File {
        // content
        #[serde(rename = "c")]
        content: String,
        // dependencies
        #[serde(rename = "d")]
        dependencies: Vec<String>,
        // is transpiled?
        #[serde(rename = "t")]
        is_transpiled: bool,
    },
    // We didn't compile or detected this file being used, so we return the size in bytes instead
    Ignored(u64),
    // Something went wrong with this file
    Failed(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinimalCachedModule {
    // name, it's part of the request so leaving it out for now...
    // n: String,
    // version, it's part of the request so leaving it out for now...
    // v: String,
    // files
    #[serde(rename = "f")]
    files: HashMap<String, MinimalFile>,
    // used modules, this is different from dependencies as this only includes a
    // list of node_modules that are used in the code, used for the resolve endpoint
    // to eagerly fetch these modules
    #[serde(rename = "m")]
    modules: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleDependency {
    #[serde(rename = "v")]
    pub version: String,
    #[serde(rename = "i")]
    pub is_used: bool,
}

pub type ModuleDependenciesMap = HashMap<String, ModuleDependency>;

fn collect_file_paths(
    dir_path: PathBuf,
    root_dir: PathBuf,
    files_map: &mut HashMap<String, u64>,
) -> Result<(), ServerError> {
    for entry in fs::read_dir(dir_path)? {
        let entry = entry?;
        let entry_path = entry.path();

        let metadata = fs::metadata(&entry_path)?;
        if metadata.is_dir() {
            collect_file_paths(entry_path, root_dir.clone(), files_map)?;
        } else if metadata.is_file() {
            // Module specifiers always use `/`, but `as_os_str` yields the
            // platform separator (`\` on Windows). Normalize so the resolver
            // can match nested files (e.g. `cjs/react.development.js`).
            let rel_path = entry_path
                .strip_prefix(root_dir.clone())
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\\', "/");
            files_map.insert(rel_path, metadata.len());
        }
    }

    Ok(())
}

fn deps_to_files_and_modules(deps: &[String]) -> (HashSet<String>, HashSet<String>) {
    use super::npm_specifier::{parse_module_specifier, ModuleSpecifier};

    let mut used_modules: HashSet<String> = HashSet::new();
    let mut file_specifiers: HashSet<String> = HashSet::new();

    for dep in deps {
        match parse_module_specifier(dep) {
            Some(ModuleSpecifier::Relative(path)) => {
                file_specifiers.insert(path);
            }
            Some(ModuleSpecifier::Package { name, .. }) => {
                used_modules.insert(name);
            }
            None => {
                // URI / absolute / invalid — not a package-graph edge.
            }
        }
    }

    (file_specifiers, used_modules)
}

fn transform_files(
    specifiers: Vec<String>,
    curr_file: &str,
    result_map: &mut HashMap<String, MinimalFile>,
    files_map: &HashMap<String, u64>,
    pkg_root: PathBuf,
    used_modules: &mut HashSet<String>,
) {
    let curr_dir = resolver::file_path_to_dirname(curr_file);
    let curr_extension = resolver::extract_file_extension(curr_file);
    for specifier in specifiers {
        let abs_specifier =
            resolver::make_mod_specifier_absolute(curr_dir.as_str(), specifier.as_str());
        let found_files =
            resolver::collect_files(abs_specifier.as_str(), files_map, curr_extension);
        for found_file in found_files {
            if !result_map.contains_key(found_file.as_str()) {
                let file_path = pkg_root.clone().join(found_file.as_str());
                match fs::read_to_string(file_path) {
                    Ok(content) => match transform_file(found_file.as_str(), content.as_str()) {
                        Ok(transformed_file) => {
                            let deps: Vec<String> =
                                transformed_file.dependencies.into_iter().collect();
                            let (file_deps, module_deps) = deps_to_files_and_modules(&deps);

                            for module_dep in module_deps {
                                used_modules.insert(module_dep);
                            }

                            // Per-file `d` is what Sandpack walks with addDependency.
                            // Only keep edges the specifier parser accepts.
                            let graph_deps: Vec<String> = deps
                                .into_iter()
                                .filter(|d| {
                                    super::npm_specifier::parse_module_specifier(d).is_some()
                                })
                                .collect();

                            result_map.insert(
                                found_file.clone(),
                                MinimalFile::File {
                                    content: transformed_file.content,
                                    dependencies: graph_deps,
                                    is_transpiled: true,
                                },
                            );

                            // Always keep this last, to prevent infinite loops
                            transform_files(
                                file_deps.into_iter().collect(),
                                found_file.as_str(),
                                result_map,
                                files_map,
                                pkg_root.clone(),
                                used_modules,
                            );
                        }
                        Err(err) => {
                            error!("{:?}", err);

                            result_map.insert(
                                found_file.clone(),
                                MinimalFile::File {
                                    content: content.clone(),
                                    dependencies: vec![],
                                    is_transpiled: false,
                                },
                            );
                        }
                    },
                    // TODO: Return an error in this case?
                    Err(err) => {
                        error!("Error reading file: {:?}", err);
                        result_map.insert(found_file.clone(), MinimalFile::Failed(false));
                    }
                }
            }
        }
    }
}

#[tracing::instrument(name = "transform_package", skip(pkg_output_path))]
fn transform_package(
    pkg_output_path: PathBuf,
    package_name: &str,
    package_version: &str,
) -> Result<(MinimalCachedModule, ModuleDependenciesMap), ServerError> {
    let mut file_paths: HashMap<String, u64> = HashMap::new();
    {
        let collect_files_span = span!(
            Level::INFO,
            "pkg_collect_file_paths",
            package_name = package_name,
            package_version = package_version
        )
        .entered();
        collect_file_paths(
            pkg_output_path.clone(),
            pkg_output_path.clone(),
            &mut file_paths,
        )?;
        collect_files_span.exit();
    }

    let mut module_files: HashMap<String, MinimalFile> = HashMap::new();
    let mut used_modules: HashSet<String> = HashSet::new();

    // Read and process pkg.json
    let read_pkg_json = span!(
        Level::INFO,
        "read_pkg_json",
        package_name = package_name,
        package_version = package_version
    )
    .entered();
    let pkg_json_content = fs::read_to_string(Path::new(&pkg_output_path).join("package.json"))?;
    let parsed_pkg_json: PackageJSON = package_json::parse_pkg_json(pkg_json_content.clone())?;

    // add package.json content to the files
    module_files.insert(
        String::from("package.json"),
        MinimalFile::File {
            content: pkg_json_content,
            dependencies: vec![],
            is_transpiled: false,
        },
    );
    read_pkg_json.exit();

    // transform entries
    {
        let transform_files_span = span!(
            Level::INFO,
            "pkg_transform_files",
            package_name = package_name,
            package_version = package_version
        )
        .entered();
        transform_files(
            package_json::collect_pkg_entries(parsed_pkg_json.clone())?,
            ".",
            &mut module_files,
            &file_paths,
            pkg_output_path,
            &mut used_modules,
        );
        transform_files_span.exit();
    }

    // add remaining files as ignored files
    for (key, value) in &file_paths {
        if !module_files.contains_key(key) {
            module_files.insert(String::from(key), MinimalFile::Ignored(*value));
        }
    }

    // collect dependencies for `/dep_tree` (only `is_used` edges are followed):
    //  1) declared `dependencies` (used or not — unused stay is_used=false)
    //  2) used `peerDependencies` (version from peer range)
    //  3) used bare imports with no declared range (e.g. `@swc/helpers` pulled
    //     in by `@tailwindcss/browser` without a peer entry) → dist-tag "latest"
    let mut dependencies: ModuleDependenciesMap = HashMap::new();
    if let Some(deps) = parsed_pkg_json.dependencies {
        for (key, value) in deps.iter() {
            dependencies.insert(
                key.clone(),
                ModuleDependency {
                    version: value.clone(),
                    is_used: used_modules.contains(key),
                },
            );
        }
    }
    if let Some(peers) = parsed_pkg_json.peer_dependencies {
        for (key, value) in peers.iter() {
            if dependencies.contains_key(key) {
                continue;
            }
            if !used_modules.contains(key) {
                continue;
            }
            dependencies.insert(
                key.clone(),
                ModuleDependency {
                    version: value.clone(),
                    is_used: true,
                },
            );
        }
    }
    for key in used_modules.iter() {
        if key.eq(&package_name) || dependencies.contains_key(key) {
            continue;
        }
        // Keys here are already bare package names from parse_module_specifier.
        if !super::npm_specifier::is_valid_package_name(key) {
            continue;
        }
        dependencies.insert(
            key.clone(),
            ModuleDependency {
                // Alias path in collect_dep_tree resolves npm dist-tags.
                version: String::from("latest"),
                is_used: true,
            },
        );
    }

    let used_modules: Vec<String> = used_modules
        .into_iter()
        .filter(|v| !v.eq(&package_name) && super::npm_specifier::is_valid_package_name(v))
        .collect::<Vec<String>>();
    let module_spec = MinimalCachedModule {
        files: module_files,
        modules: used_modules,
    };

    Ok((module_spec, dependencies))
}

#[tracing::instrument(name = "process_npm_package", skip(temp_dir, npm_db))]
pub async fn process_npm_package(
    package_name: &str,
    package_version: &str,
    temp_dir: &str,
    npm_db: &NpmDatabase,
    content_fetcher: &PackageContentFetcher,
) -> Result<(MinimalCachedModule, ModuleDependenciesMap), ServerError> {
    info!(
        "Started processing package: {}@{}",
        package_name, package_version
    );

    let tarball_content =
        download_package_content(package_name, package_version, npm_db, content_fetcher).await?;
    let mut pkg_output_path = Path::new(temp_dir)
        .join(nanoid!())
        .join(format!("{}-{}", package_name, package_version));
    tar::store_tarball(tarball_content.as_ref().clone(), pkg_output_path.as_path())?;

    // TODO: Go to first folder, folder is not always named `package`
    // for @types, it's the name of the package, like `acorn`, `react`, ...
    pkg_output_path = pkg_output_path.join("package");

    // Transform module in new thread
    let package_name_string = String::from(package_name);
    let package_version_string = String::from(package_version);
    let cloned_pkg_output_path = pkg_output_path.clone();
    let task = tokio::task::spawn_blocking(move || {
        transform_package(
            cloned_pkg_output_path,
            package_name_string.as_str(),
            package_version_string.as_str(),
        )
    });
    let transform_result = task.await?;

    // Cleanup package directory
    tokio::fs::remove_dir_all(pkg_output_path.as_path()).await?;

    transform_result
}

pub fn parse_package_specifier_no_validation(
    package_specifier: &str,
) -> Result<(String, String), ServerError> {
    if package_specifier.contains(char::is_whitespace) {
        return Err(ServerError::InvalidPackageSpecifier);
    }

    let mut parts: Vec<&str> = package_specifier.split('@').collect();
    let package_version_opt = parts.pop();
    if let Some(package_version) = package_version_opt {
        if parts.len() > 2 {
            return Err(ServerError::InvalidPackageSpecifier);
        }

        let package_name = parts.join("@");
        Ok((package_name, String::from(package_version)))
    } else {
        Err(ServerError::InvalidPackageSpecifier)
    }
}

pub fn parse_package_specifier(package_specifier: &str) -> Result<(String, String), ServerError> {
    let (name, version) = parse_package_specifier_no_validation(package_specifier)?;
    Version::parse(&version)?;
    Ok((name, version))
}
