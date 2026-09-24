//! # nsqlite-vector
//!
//! A blocking HTTP client for OpenRouter's embeddings and rerank endpoints.
//!
//! This crate is the network layer only. It has no database, no SQL, and no
//! virtual table; it turns text into vectors and reorders documents, and leaves
//! storage to the caller.
//!
//! ```no_run
//! use nsqlite_vector::{ClientConfig, EmbedClient};
//!
//! // The key comes from the caller — an env var, a secret store, a prompt.
//! // Nothing in this crate reads one from disk or hardcodes one.
//! let api_key = "sk-or-...".to_string();
//!
//! let client = EmbedClient::new(ClientConfig::new(api_key))?;
//! let vectors = client.embed_batch(&["first doc".to_string(), "second doc".to_string()])?;
//! assert_eq!(vectors.len(), 2);
//! # Ok::<(), nsqlite_vector::VectorError>(())
//! ```
//!
//! The example is `no_run`: it compiles as part of the test suite but issues no
//! request, so `cargo test` never touches the network.
//!
//! ## Two things to know before you use it
//!
//! **Embeddings come back out of order.** The `data` array is not ordered to
//! match the `input` array; each entry's `index` says which input it belongs
//! to. This crate sorts on it, so the vectors you get back line up with the
//! texts you sent in — but if you call the API directly, sort yourself.
//!
//! **`relevance_score` has no absolute meaning.** OpenRouter documents the
//! field only as "Relevance score of the document to the query". Range, units,
//! and whether it is a logit or a probability are all unstated, and different
//! models are not on a shared scale. It is exposed here as a bare `f64` for
//! ranking *within one response*. Do not threshold it against a constant; see
//! the [`rerank`] module docs.
//!
//! ## Dependencies
//!
//! [`ureq`] for the HTTP, [`serde`]/[`serde_json`] for the JSON. All
//! MIT/Apache-2.0. No async runtime, no TLS choice to make, no retry policy to
//! reason about — a 429 comes back as [`VectorError::RateLimited`] with the
//! `Retry-After` delay, and deciding what to do about it stays with the caller.

mod config;
mod embed;
mod error;
mod rerank;

pub use config::{
    ClientConfig, DEFAULT_BASE_URL, DEFAULT_EMBED_MODEL, DEFAULT_RERANK_MODEL, DEFAULT_TIMEOUT,
};
pub use embed::{
    parse_embed_response, EmbedClient, EmbedRequest, EmbedResponse, EmbeddingInput, EncodingFormat,
    MAX_BATCH_SIZE,
};
pub use error::{ApiErrorDetail, ApiErrorEnvelope, VectorError};
pub use rerank::{
    parse_rerank_response, parse_rerank_response_bounded, RerankClient, RerankRequest, RerankResult,
};

use std::time::Duration;

/// Largest response body this client will read, in bytes.
///
/// `ureq`'s own default is 10 MiB, which a 64-input batch of large float
/// vectors can exceed. The limit is raised rather than removed: an unbounded
/// read on a hostile or misconfigured endpoint is a way to run out of memory.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// Builds the connection-pooling agent both clients share.
///
/// `http_status_as_error` is turned off so that a non-2xx arrives as a
/// `Response` with its body still attached; the 429 and the `error` envelope
/// are only readable from the body, so letting `ureq` collapse the response
/// into a bare status would throw away exactly the information callers need.
///
/// `max_redirects` is zero for the same reason. The `Authorization` header is
/// set per request, and an endpoint that answers with a redirect is not one
/// this client should chase — the 3xx is surfaced as
/// [`VectorError::Http`] instead.
fn build_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

/// POSTs a JSON body and returns the response body as a string.
///
/// Shared by both clients so the auth header, the status mapping, and the
/// redaction rule are written once. Every error path passes through
/// [`VectorError::redact`] first, so a server that echoes the request back
/// cannot walk the key into a log.
fn post_json(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    body: &str,
) -> Result<String, VectorError> {
    let response = agent
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|e| VectorError::Transport(e.to_string()).redact(api_key))?;

    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let text = response
        .into_body()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .lossy_utf8(true)
        .read_to_string()
        .map_err(|e| VectorError::Transport(e.to_string()).redact(api_key))?;

    if (200..300).contains(&status) {
        return Ok(text);
    }
    Err(error::error_from_status(status, retry_after.as_deref(), &text).redact(api_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_are_send_and_sync() {
        // They hold a pooled agent, so sharing one across threads must work.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EmbedClient>();
        assert_send_sync::<RerankClient>();
        assert_send_sync::<ClientConfig>();
    }

    #[test]
    fn transport_errors_are_redacted_before_they_escape() {
        // Exercises the redaction rule that `post_json` applies to every
        // transport failure, without needing a socket to fail.
        let key = "sk-or-v1-EXAMPLEKEY-NOT-REAL-0000";
        let err = VectorError::Transport(format!("connection failed with {key}")).redact(key);
        assert!(!format!("{err:?} {err}").contains(key));
        assert!(err.to_string().contains("<redacted>"));
    }

    #[test]
    fn decode_errors_are_redacted_before_they_escape() {
        let key = "sk-or-v1-EXAMPLEKEY-NOT-REAL-0000";
        let err = VectorError::Decode(format!("bad body echoing {key}")).redact(key);
        assert!(!format!("{err:?} {err}").contains(key));
    }

    #[test]
    fn error_from_status_is_reachable_for_callers_mapping_their_own_transports() {
        let err = error::error_from_status(429, Some("7"), r#"{"error":{"code":429}}"#);
        assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
    }
}
