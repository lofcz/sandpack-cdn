use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::routes::AppState;

pub async fn health_handler(State(state): State<AppState>) -> Response {
    match state.npm_db.ping() {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("database unavailable: {err}"),
        )
            .into_response(),
    }
}
