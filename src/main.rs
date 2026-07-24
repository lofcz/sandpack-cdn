use crate::npm_replicator::database::NpmDatabase;
use dotenvy::dotenv;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

mod app_error;
mod cached;
mod npm;
mod npm_replicator;
mod package;
mod router;
mod setup_tracing;
mod transform;
mod utils;

#[derive(Clone)]
pub struct AppConfig {
    temp_dir: String,
    /// Durable transform cache for exact `name@version` (survives process restarts).
    package_cache_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    dotenv().ok();

    let port = match env::var("PORT") {
        Ok(var) => var,
        Err(_) => String::from("8080"),
    }
    .parse::<u16>()
    .unwrap();

    let npm_db_path = env::var("NPM_SQLITE_DB").expect("NPM_SQLITE_DB env variable should be set");

    setup_tracing::setup_tracing();

    let temp_dir_path = env::current_dir()?.join("temp_files");
    let temp_dir = temp_dir_path.as_os_str().to_str().unwrap();

    // Prefer PACKAGE_CACHE_DIR; otherwise sibling of the sqlite db
    // (`…/data/npm.sqlite` → `…/data/package_cache`), then `./package_cache`.
    let package_cache_dir = match env::var("PACKAGE_CACHE_DIR") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(&npm_db_path)
            .parent()
            .map(|p| p.join("package_cache"))
            .unwrap_or_else(|| PathBuf::from("package_cache")),
    };

    let app_data = AppConfig {
        temp_dir: String::from(temp_dir),
        package_cache_dir: package_cache_dir.clone(),
    };

    // create data directories
    tokio::fs::create_dir_all(String::from(temp_dir)).await?;
    tokio::fs::create_dir_all(&package_cache_dir).await?;
    println!("Package transform cache: {}", package_cache_dir.display());

    // Setup the npm package store. Packages are fetched on demand from the npm
    // registry (see NpmDatabase::ensure_package) instead of replicating the
    // whole registry, so no background sync thread is needed.
    let npm_db = NpmDatabase::new(&npm_db_path).unwrap();
    npm_db.init().unwrap();

    // Layers are applied outermost-last in axum: requests flow
    // Trace -> Cors -> Compression -> handler, responses flow back out.
    let app = router::routes::routes(npm_db, app_data)
        // Negotiated gzip compression (replaces warp::compression::gzip()).
        .layer(CompressionLayer::new())
        // Allow any origin/method/header and auto-handle OPTIONS preflight
        // (replaces the manual Access-Control-* header filter).
        .layer(CorsLayer::permissive())
        // Per-request tracing spans (replaces warp::trace::request()).
        .layer(TraceLayer::new_for_http());

    // Bind to loopback only: the CDN is a private sidecar reached exclusively by
    // the host app over http://localhost (health check + reverse proxy). Loopback
    // traffic is exempt from Windows Firewall, so this avoids the "allow this app
    // through the firewall" prompt (which needs admin) and keeps the CDN off the
    // network entirely.
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    println!("Server running on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Resolves on SIGINT or (on Unix) SIGTERM so in-flight requests can drain
/// before the process exits.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    println!("Shutdown signal received, draining connections...");
}
