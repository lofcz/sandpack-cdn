use std::sync::LazyLock;

use regex::Regex;

use crate::app_error::ServerError;

static VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^(\\d+)\\((.*)\\)$").unwrap());
const LATEST_VERSION: u64 = 5;

/// Generous upper bound for an encoded specifier list; legitimate queries
/// (even ~1000 dependencies) stay well under it.
const MAX_ENCODED_LEN: usize = 64 * 1024;

pub fn decode_base64(part: &str) -> Result<String, ServerError> {
    if part.len() > MAX_ENCODED_LEN {
        return Err(ServerError::InvalidQuery);
    }
    let decoded = base64_simd::STANDARD
        .decode_to_vec(part.as_bytes())
        .map_err(|_e| ServerError::Base64DecodingError())?;
    let val = String::from_utf8(decoded).map_err(|_e| ServerError::Base64DecodingError())?;
    Ok(val)
}

pub fn decode_req_part(part: &str) -> Result<(u64, String), ServerError> {
    let decoded = decode_base64(part)?;

    if let Some(parts) = VERSION_RE.captures(&decoded) {
        if let Some(version_match) = parts.get(1) {
            let version = version_match.as_str().parse::<u64>()?;
            if version > LATEST_VERSION {
                return Err(ServerError::InvalidCDNVersion);
            }

            if let Some(content_match) = parts.get(2) {
                return Ok((version, String::from(content_match.as_str())));
            }
        }
    }

    // Fallback to no version
    Ok((1, decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_decodes() {
        // "react@^18.0.0" in standard base64
        assert_eq!(
            decode_base64("cmVhY3RAXjE4LjAuMA==").unwrap(),
            "react@^18.0.0"
        );
    }

    #[test]
    fn oversized_input_is_rejected() {
        let oversized = "A".repeat(MAX_ENCODED_LEN + 1);
        assert!(matches!(
            decode_base64(&oversized),
            Err(ServerError::InvalidQuery)
        ));
    }

    #[test]
    fn invalid_base64_is_rejected() {
        assert!(matches!(
            decode_base64("!!!"),
            Err(ServerError::Base64DecodingError())
        ));
    }
}
