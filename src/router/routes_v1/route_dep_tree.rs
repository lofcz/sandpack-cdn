use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};

use crate::app_error::ServerError;
use crate::npm_replicator::database::NpmDatabase;
use crate::package::cached::CachedPackageProcessor;
use crate::package::collect_dep_tree::{collect_dep_tree, process_dep_map, DependencyList};
use crate::router::routes::AppState;

use super::super::custom_reply::CustomReply;
use super::super::error_reply::ErrorReply;
use super::super::utils::decode_req_part;

async fn process_dep_tree(
    decoded_deps_str: &str,
    npm_db: &NpmDatabase,
    pkg_processor: &CachedPackageProcessor,
) -> Result<DependencyList, ServerError> {
    let dep_map: HashMap<String, String> = serde_json::from_str(decoded_deps_str)?;
    let dep_requests = process_dep_map(dep_map, 0)?;
    collect_dep_tree(dep_requests, npm_db, pkg_processor).await
}

pub async fn get_dep_tree_reply(
    path: String,
    npm_db: NpmDatabase,
    pkg_processor: CachedPackageProcessor,
) -> Result<CustomReply, ServerError> {
    let (version, decoded_deps_str) = decode_req_part(&path)?;

    let tree = process_dep_tree(&decoded_deps_str, &npm_db, &pkg_processor).await?;

    let mut reply = match version {
        0..=4 => CustomReply::json(&tree),
        _ => CustomReply::msgpack(&tree),
    }?;
    let cache_ttl = 15 * 60;
    reply.add_header(
        "Cache-Control",
        format!("public, max-age={}", cache_ttl).as_str(),
    );
    reply.add_header(
        "CDN-Cache-Control",
        format!("max-age={}", cache_ttl).as_str(),
    );
    Ok(reply)
}

pub async fn dep_tree_handler(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    match get_dep_tree_reply(path, state.npm_db, state.pkg_processor).await {
        Ok(reply) => reply.into_response(),
        Err(err) => ErrorReply::from(err).respond(),
    }
}
