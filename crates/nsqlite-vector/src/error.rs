//! Error type shared by the embeddings and rerank clients.
//!
//! Every variant is built from data that never contains the API key: transport
//! failures come from `ureq`, HTTP failures are assembled from the status code
//! and the server's own error envelope, and [`VectorError::redact`] scrubs the
//! key out of any server-supplied string before it reaches a log or a caller.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use serde::Deserialize;

/// The error envelope OpenRouter returns alongside a non-2xx status.
///
/// Every field is optional: the shape is stable in the published spec, but a
/// proxy or a gateway can sit in front of the API and return something else
/// entirely, so a missing or mistyped member must not mask the status code.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiErrorEnvelope {
    /// The `error` member of the response body.
    #[serde(default)]
    pub error: Option<ApiErrorDetail>,
}

/// The inner `error` object of an [`ApiErrorEnvelope`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiErrorDetail {
    /// OpenRouter's own numeric error code, when present.
    #[serde(default)]
    pub code: Option<serde_json::Value>,
    /// A human-readable message, when present.
    #[serde(default)]
    pub message: Option<String>,
    /// Free-form metadata, of which `error_type` is the member this crate reads.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

impl ApiErrorDetail {
    /// `metadata.error_type`, e.g. `rate_limit_exceeded`.
    pub fn error_type(&self) -> Option<&str> {
        self.metadata.as_ref()?.get("error_type")?.as_str()
    }
}

/// Anything that can go wrong while talking to the OpenRouter vector APIs.
#[derive(Debug)]
pub enum VectorError {
    /// The client was handed a configuration it cannot use, such as an empty
    /// API key, a `base_url` without a scheme, or empty input text.
    Config(String),

    /// The request never produced a usable HTTP response: DNS failure, refused
    /// connection, TLS problem, timeout, or a malformed request.
    Transport(String),

    /// The server answered with a non-2xx status that is not a 429.
    Http {
        /// The HTTP status code.
        status: u16,
        /// `error.code` from the body, when the server sent a well-formed envelope.
        code: Option<i64>,
        /// `error.message` from the body, when present.
        message: Option<String>,
        /// `error.metadata.error_type` from the body, when present.
        error_type: Option<String>,
    },

    /// A 2xx response whose body could not be parsed into the documented shape,
    /// or whose contents were internally inconsistent (duplicate or out-of-range
    /// indices, a wrong number of vectors, an undecodable base64 payload).
    Decode(String),

    /// A 429, which is a normal, expected outcome under load and is worth
    /// distinguishing from other failures so callers can back off.
    RateLimited {
        /// The parsed `Retry-After` header, when it was present and numeric.
        /// A `Retry-After` holding an HTTP-date is reported as `None`; the
        /// header is still available on the raw response if a caller needs it.
        retry_after: Option<Duration>,
        /// `error.code` from the body, when the server sent a well-formed envelope.
        code: Option<i64>,
        /// `error.message` from the body, when present.
        message: Option<String>,
        /// `error.metadata.error_type` from the body, when present.
        error_type: Option<String>,
    },
}

impl VectorError {
    /// Builds a [`VectorError::RateLimited`] from a `Retry-After` header value.
    ///
    /// The header is either a non-negative number of seconds or an HTTP-date.
    /// Only the numeric form is turned into a [`Duration`]; an HTTP-date needs a
    /// date parser, which this crate deliberately does not take a dependency for,
    /// so it is reported as `None` rather than guessed at.
    pub fn parse_retry_after(header: &str) -> Option<Duration> {
        let seconds: u64 = header.trim().parse().ok()?;
        Some(Duration::from_secs(seconds))
    }

    /// The HTTP status behind this error, when the request got far enough to
    /// have one. `None` for configuration, transport, and decode failures.
    pub fn status(&self) -> Option<u16> {
        match self {
            VectorError::Http { status, .. } => Some(*status),
            VectorError::RateLimited { .. } => Some(429),
            _ => None,
        }
    }

    /// The `Retry-After` delay, for a 429 that carried a numeric one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            VectorError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The server's message, when the error carried one.
    pub fn message(&self) -> Option<&str> {
        match self {
            VectorError::Http { message, .. } | VectorError::RateLimited { message, .. } => {
                message.as_deref()
            }
            _ => None,
        }
    }

    /// Replaces every occurrence of `secret` in a server-supplied string with
    /// `<redacted>`.
    ///
    /// The API key is only ever sent in a request header, so in practice this is
    /// belt-and-braces. It exists because `error.message` is attacker-adjacent
    /// data: a gateway that echoes a request header back to us must not be able
    /// to walk the key into a log file through this type. An empty `secret` is
    /// ignored, since matching against the empty string would redact everything.
    pub fn redact(mut self, secret: &str) -> Self {
        if secret.is_empty() {
            return self;
        }
        match &mut self {
            VectorError::Config(message) => scrub(message, secret),
            VectorError::Transport(message) => scrub(message, secret),
            VectorError::Decode(message) => scrub(message, secret),
            VectorError::Http {
                message,
                error_type,
                ..
            }
            | VectorError::RateLimited {
                message,
                error_type,
                ..
            } => {
                scrub_opt(message, secret);
                scrub_opt(error_type, secret);
            }
        }
        self
    }
}

fn scrub(value: &mut String, secret: &str) {
    if value.contains(secret) {
        *value = value.replace(secret, "<redacted>");
    }
}

fn scrub_opt(value: &mut Option<String>, secret: &str) {
    if let Some(inner) = value {
        scrub(inner, secret);
    }
}

/// Coerces a JSON value to `i64`, accepting the stringified integers that some
/// gateways emit where the spec documents a number.
fn code_to_i64(value: Option<&serde_json::Value>) -> Option<i64> {
    match value? {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Turns a non-2xx response into the right [`VectorError`] variant.
///
/// A 429 always becomes [`VectorError::RateLimited`], carrying the `Retry-After`
/// delay when the header was present and numeric. Everything else becomes
/// [`VectorError::Http`], keeping the status even when the body is not the
/// documented envelope — a 502 from a proxy is still a 502, and losing that
/// because the body was HTML would be the wrong trade.
pub fn error_from_status(status: u16, retry_after: Option<&str>, body: &str) -> VectorError {
    let detail = serde_json::from_str::<ApiErrorEnvelope>(body)
        .ok()
        .and_then(|envelope| envelope.error);

    let code = detail.as_ref().and_then(|d| code_to_i64(d.code.as_ref()));
    let message = detail.as_ref().and_then(|d| d.message.clone());
    let error_type = detail
        .as_ref()
        .and_then(|d| d.error_type().map(str::to_owned));

    if status == 429 {
        return VectorError::RateLimited {
            retry_after: retry_after.and_then(VectorError::parse_retry_after),
            code,
            message,
            error_type,
        };
    }

    VectorError::Http {
        status,
        code,
        message,
        error_type,
    }
}

impl fmt::Display for VectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VectorError::Config(message) => write!(f, "configuration error: {message}"),
            VectorError::Transport(message) => write!(f, "transport error: {message}"),
            VectorError::Decode(message) => write!(f, "response decode error: {message}"),
            VectorError::Http {
                status,
                code,
                message,
                error_type,
            } => write!(f, "http {status}: {}", describe(code, message, error_type)),
            VectorError::RateLimited {
                retry_after,
                code,
                message,
                error_type,
            } => {
                let retry = match retry_after {
                    Some(d) => format!("; retry after {}s", d.as_secs()),
                    None => String::new(),
                };
                write!(
                    f,
                    "rate limited (429): {}{retry}",
                    describe(code, message, error_type)
                )
            }
        }
    }
}

fn describe(code: &Option<i64>, message: &Option<String>, error_type: &Option<String>) -> String {
    let mut parts = Vec::new();
    if let Some(code) = code {
        parts.push(format!("code {code}"));
    }
    if let Some(error_type) = error_type {
        parts.push(format!("type {error_type}"));
    }
    if let Some(message) = message {
        parts.push(message.clone());
    }
    if parts.is_empty() {
        "the server sent no error details".to_string()
    } else {
        parts.join(": ")
    }
}

impl Error for VectorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_maps_to_http_variant() {
        let body = r#"{"error":{"code":400,"message":"model not found",
                       "metadata":{"error_type":"invalid_request_error"}}}"#;
        let err = error_from_status(400, None, body);
        match &err {
            VectorError::Http {
                status,
                code,
                message,
                error_type,
            } => {
                assert_eq!(*status, 400);
                assert_eq!(*code, Some(400));
                assert_eq!(message.as_deref(), Some("model not found"));
                assert_eq!(error_type.as_deref(), Some("invalid_request_error"));
            }
            other => panic!("expected Http, got {other:?}"),
        }
        assert!(err.to_string().contains("invalid_request_error"));
    }

    #[test]
    fn error_body_maps_to_rate_limited() {
        let body = r#"{"error":{"code":429,"message":"Rate limit exceeded",
                       "metadata":{"error_type":"rate_limit_exceeded"}}}"#;
        let err = error_from_status(429, Some("30"), body);
        match &err {
            VectorError::RateLimited {
                retry_after,
                code,
                message,
                error_type,
            } => {
                assert_eq!(*retry_after, Some(Duration::from_secs(30)));
                assert_eq!(*code, Some(429));
                assert_eq!(message.as_deref(), Some("Rate limit exceeded"));
                assert_eq!(error_type.as_deref(), Some("rate_limit_exceeded"));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert_eq!(err.status(), Some(429));
        assert_eq!(err.retry_after(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn rate_limited_without_retry_after_header() {
        let err = error_from_status(429, None, r#"{"error":{"code":429,"message":"slow down"}}"#);
        assert!(matches!(err, VectorError::RateLimited { .. }));
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn http_date_retry_after_is_reported_as_unknown() {
        assert_eq!(
            VectorError::parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            None
        );
        assert_eq!(
            VectorError::parse_retry_after(" 12 "),
            Some(Duration::from_secs(12))
        );
        assert_eq!(VectorError::parse_retry_after("-1"), None);
    }

    #[test]
    fn unparseable_body_still_keeps_the_status() {
        let err = error_from_status(502, None, "<html>bad gateway</html>");
        assert_eq!(
            err.to_string(),
            "http 502: the server sent no error details"
        );
        match err {
            VectorError::Http {
                status,
                code,
                message,
                error_type,
            } => {
                assert_eq!(status, 502);
                assert_eq!(code, None);
                assert_eq!(message, None);
                assert_eq!(error_type, None);
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn stringified_code_is_still_read_as_a_number() {
        let err = error_from_status(500, None, r#"{"error":{"code":"500","message":"boom"}}"#);
        assert_eq!(err.status(), Some(500));
        assert!(err.to_string().contains("code 500"));
    }

    #[test]
    fn redact_scrubs_the_key_from_every_string() {
        let secret = "sk-or-v1-supersecret";
        let body = format!(
            r#"{{"error":{{"code":401,"message":"bad key {secret}",
                "metadata":{{"error_type":"{secret}"}}}}}}"#
        );
        let err = error_from_status(401, None, &body).redact(secret);
        let rendered = format!("{err:?} {err}");
        assert!(!rendered.contains(secret), "key leaked: {rendered}");
    }

    #[test]
    fn redact_ignores_an_empty_secret() {
        let err = VectorError::Config("something".to_string()).redact("");
        assert_eq!(err.to_string(), "configuration error: something");
    }
}
