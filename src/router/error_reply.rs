use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{self, Deserialize, Serialize};

use crate::app_error::ServerError;

use super::custom_reply::CustomReply;

#[derive(Clone, Serialize, Deserialize)]
pub struct ErrorReply {
    status: u16,
    message: String,
    details: String,
}

impl ErrorReply {
    pub fn new(status: u16, message: String, details: String) -> Self {
        ErrorReply {
            status,
            message,
            details,
        }
    }

    pub fn as_reply(&self, cache_ttl: u32) -> Result<CustomReply, ServerError> {
        let mut reply = CustomReply::json(self)?;
        reply.set_status(StatusCode::from_u16(self.status)?);
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

    /// Render this error as an HTTP response with the given cache TTL.
    /// Serializing a plain error struct cannot realistically fail; if it does we
    /// fall back to a bare 500 rather than panicking inside the response path.
    pub fn respond(self, cache_ttl: u32) -> Response {
        match self.as_reply(cache_ttl) {
            Ok(reply) => reply.into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

impl From<ServerError> for ErrorReply {
    fn from(err: ServerError) -> Self {
        ErrorReply::new(500, format!("{}", err), format!("{:?}", err))
    }
}
