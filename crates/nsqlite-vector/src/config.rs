//! Client configuration.
//!
//! The API key is supplied by the caller at construction time. It is never read
//! from a file or an environment variable by this crate, and it is redacted from
//! every `Debug` rendering — see [`ClientConfig`]'s hand-written `Debug` impl and
//! [`crate::VectorError::redact`].

use std::fmt;
use std::time::Duration;

use crate::error::VectorError;

/// The OpenRouter API root used when the caller does not override it.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// The free embedding model used when the caller does not override it.
pub const DEFAULT_EMBED_MODEL: &str = "nvidia/llama-nemotron-embed-vl-1b-v2:free";

/// The free reranking model used when the caller does not override it.
pub const DEFAULT_RERANK_MODEL: &str = "nvidia/llama-nemotron-rerank-vl-1b-v2:free";

/// The per-request timeout used when the caller does not override it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Everything both clients need to reach OpenRouter.
///
/// Build one with [`ClientConfig::new`] and adjust the defaults from there:
///
/// ```
/// use nsqlite_vector::ClientConfig;
///
/// let config = ClientConfig::new("sk-or-...")          // key comes from the caller
///     .with_base_url("https://openrouter.ai/api/v1")  // optional
///     .with_timeout(std::time::Duration::from_secs(30));
/// # let _ = config;
/// ```
#[derive(Clone)]
pub struct ClientConfig {
    api_key: String,
    base_url: String,
    embed_model: String,
    rerank_model: String,
    timeout: Duration,
}

impl ClientConfig {
    /// Builds a config from an API key, filling in OpenRouter's defaults for
    /// the base URL, the two models, and the timeout.
    ///
    /// The key is not validated here beyond rejecting an empty one; a key that
    /// is well-formed but wrong only shows up as a 401 from the server, which
    /// [`crate::EmbedClient::embed_one`] reports as
    /// [`VectorError::Http`].
    pub fn new(api_key: impl Into<String>) -> Self {
        ClientConfig {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            embed_model: DEFAULT_EMBED_MODEL.to_string(),
            rerank_model: DEFAULT_RERANK_MODEL.to_string(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Overrides the API root. A trailing `/` is trimmed so that joining a
    /// path onto it never produces a double slash.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// Overrides the embedding model id.
    pub fn with_embed_model(mut self, model: impl Into<String>) -> Self {
        self.embed_model = model.into();
        self
    }

    /// Overrides the reranking model id.
    pub fn with_rerank_model(mut self, model: impl Into<String>) -> Self {
        self.rerank_model = model.into();
        self
    }

    /// Overrides the per-request timeout. `Duration::ZERO` is rejected by
    /// [`ClientConfig::validate`], since it would make every request time out
    /// instantly.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The API key, for the `Authorization` header.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// The API root, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The configured embedding model id.
    pub fn embed_model(&self) -> &str {
        &self.embed_model
    }

    /// The configured reranking model id.
    pub fn rerank_model(&self) -> &str {
        &self.rerank_model
    }

    /// The configured per-request timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Absolute URL of an endpoint such as `embeddings` or `rerank`.
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }

    /// Checks the config is usable before any request is attempted, so a
    /// malformed `base_url` surfaces as [`VectorError::Config`] instead of a
    /// transport error halfway through a batch.
    pub fn validate(&self) -> Result<(), VectorError> {
        if self.api_key.trim().is_empty() {
            return Err(VectorError::Config("api_key must not be empty".to_string()));
        }
        if self.base_url.is_empty() {
            return Err(VectorError::Config(
                "base_url must not be empty".to_string(),
            ));
        }
        let Some((scheme, rest)) = self.base_url.split_once("://") else {
            return Err(VectorError::Config(format!(
                "base_url must start with http:// or https://, got {:?}",
                self.base_url
            )));
        };
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(VectorError::Config(format!(
                "base_url must use http or https, got {scheme:?}"
            )));
        }
        if rest.is_empty() {
            return Err(VectorError::Config(
                "base_url must include a host".to_string(),
            ));
        }
        if self.timeout.is_zero() {
            return Err(VectorError::Config(
                "timeout must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// Hand-written so the key can never reach a log line, a panic message, or a
/// `{:?}` in a test failure — all of which format through `Debug`.
impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("embed_model", &self.embed_model)
            .field("rerank_model", &self.rerank_model)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-or-v1-abcdef0123456789";

    #[test]
    fn debug_output_does_not_contain_the_key() {
        let config = ClientConfig::new(SECRET);
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains(SECRET),
            "key leaked in Debug: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "no redaction marker: {rendered}"
        );
        // The non-secret fields stay visible, so a Debug dump is still useful.
        assert!(rendered.contains(DEFAULT_BASE_URL));
        assert!(rendered.contains(DEFAULT_EMBED_MODEL));
        assert!(rendered.contains(DEFAULT_RERANK_MODEL));
    }

    #[test]
    fn defaults_match_the_documented_models() {
        let config = ClientConfig::new(SECRET);
        assert_eq!(
            config.embed_model(),
            "nvidia/llama-nemotron-embed-vl-1b-v2:free"
        );
        assert_eq!(
            config.rerank_model(),
            "nvidia/llama-nemotron-rerank-vl-1b-v2:free"
        );
        assert_eq!(config.base_url(), "https://openrouter.ai/api/v1");
        assert_eq!(config.timeout(), Duration::from_secs(60));
        assert_eq!(config.api_key(), SECRET);
    }

    #[test]
    fn trailing_slash_is_trimmed_from_base_url() {
        let config = ClientConfig::new(SECRET).with_base_url("https://example.test/api/v1///");
        assert_eq!(config.base_url(), "https://example.test/api/v1");
        assert_eq!(
            config.endpoint("embeddings"),
            "https://example.test/api/v1/embeddings"
        );
    }

    #[test]
    fn validate_accepts_a_good_config() {
        assert!(ClientConfig::new(SECRET).validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_key_and_bad_urls() {
        let cases: [(&str, ClientConfig); 4] = [
            ("empty key", ClientConfig::new("   ")),
            (
                "no scheme",
                ClientConfig::new(SECRET).with_base_url("openrouter.ai"),
            ),
            (
                "bad scheme",
                ClientConfig::new(SECRET).with_base_url("ftp://example.test"),
            ),
            (
                "no host",
                ClientConfig::new(SECRET).with_base_url("https://"),
            ),
        ];
        for (name, config) in cases {
            match config.validate() {
                Err(VectorError::Config(message)) => {
                    assert!(!message.contains(SECRET), "key leaked in {name}: {message}");
                }
                other => panic!("{name}: expected Config error, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_a_zero_timeout() {
        let config = ClientConfig::new(SECRET).with_timeout(Duration::ZERO);
        assert!(matches!(config.validate(), Err(VectorError::Config(_))));
    }

    #[test]
    fn model_overrides_take_effect() {
        let config = ClientConfig::new(SECRET)
            .with_embed_model("vendor/embed")
            .with_rerank_model("vendor/rerank");
        assert_eq!(config.embed_model(), "vendor/embed");
        assert_eq!(config.rerank_model(), "vendor/rerank");
    }
}
