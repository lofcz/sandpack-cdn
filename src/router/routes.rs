use axum::response::Response;
use axum::routing::get;
use axum::Router;

use crate::npm::package_content::PackageContentFetcher;
use crate::npm_replicator::database::NpmDatabase;
use crate::package::cached::CachedPackageProcessor;
use crate::AppConfig;

use super::error_reply::ErrorReply;
use super::health::health_handler;
use super::routes_v1::route_dep_tree::dep_tree_handler;
use super::routes_v1::route_package_data::package_data_handler;

/// Shared state handed to every route handler. Cheap to clone (everything inside
/// is `Arc`-backed), so axum can clone it per request.
#[derive(Clone)]
pub struct AppState {
    pub npm_db: NpmDatabase,
    pub pkg_processor: CachedPackageProcessor,
}

pub fn routes(npm_db: NpmDatabase, app_data: AppConfig) -> Router {
    let pkg_content_fetcher = PackageContentFetcher::new();
    let pkg_processor = CachedPackageProcessor::new(
        npm_db.clone(),
        pkg_content_fetcher,
        &app_data.temp_dir,
        app_data.package_cache_dir,
    );

    let state = AppState {
        npm_db,
        pkg_processor,
    };

    Router::new()
        .route("/package/{path}", get(package_data_handler))
        .route("/dep_tree/{path}", get(dep_tree_handler))
        .route("/health", get(health_handler))
        .fallback(not_found_handler)
        .with_state(state)
}

async fn not_found_handler() -> Response {
    ErrorReply::new(404, "Not found".to_string(), "Not found".to_string()).respond()
}
