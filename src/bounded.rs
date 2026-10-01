//! JSON response bodies read with a size cap, shared by the outbound HTTP clients (Ollama and
//! Discord OAuth2), so an upstream cannot make the 256 MB container buffer without limit.

use serde::de::DeserializeOwned;

pub enum BodyError {
    /// The connection failed while the body was read.
    Transport(reqwest::Error),
    /// Larger than the limit, or not the expected JSON.
    Invalid,
}

/// Reads the whole body and parses it, giving up as soon as it exceeds `limit` bytes.
pub async fn json<T: DeserializeOwned>(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<T, BodyError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(BodyError::Transport)? {
        if bytes.len() + chunk.len() > limit {
            return Err(BodyError::Invalid);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| BodyError::Invalid)
}
