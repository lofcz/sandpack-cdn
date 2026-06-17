use std::sync::LazyLock;

use regex::Regex;

use crate::app_error::ServerError;

static VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^(\\d+)\\((.*)\\)$").unwrap());
const LATEST_VERSION: u64 = 5;

pub fn decode_base64(part: &str) -> Result<String, ServerError> {
    let decoded = base64_simd::STANDARD
        .decode_to_vec(part.as_bytes())
        .map_err(|_e| ServerError::Base64DecodingError())?;
    let val =
        String::from_utf8(decoded).map_err(|_e| ServerError::Base64DecodingError())?;
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
