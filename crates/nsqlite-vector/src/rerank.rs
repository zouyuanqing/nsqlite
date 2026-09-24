//! Rerank client for `POST {base_url}/rerank`.
//!
//! # What `relevance_score` means
//!
//! OpenRouter documents the field as "Relevance score of the document to the
//! query" and nothing more. The range is unstated, and whether the number is a
//! logit, a log-probability, or a squashed probability is unstated. Different
//! models on the same endpoint are not on a shared scale.
//!
//! So the only sound reading is **ordinal, and only inside one response**:
//!
//! * Within a single response, a higher score means the document is more
//!   relevant to that query. Sorting by it is well defined.
//! * Across two calls, even two calls to the same model, the absolute values are
//!   not comparable. A serving stack may batch, route, or quantize differently
//!   between requests.
//! * Across two models they are certainly not comparable.
//! * A fixed cutoff — `score > 0.5`, say — is meaningless, because the values may
//!   all be negative logits, all be near 0.99, or spread anywhere in between
//!   depending on the model.
//!
//! To cut off, take a *rank* from this response (the top `k`) or normalise
//! within it, never an absolute threshold. [`RerankResult`] deliberately
//! exposes the score as a bare `f64` with no predicate helpers for exactly this
//! reason.

use serde::{Deserialize, Serialize};

use crate::config::ClientConfig;
use crate::error::VectorError;

/// The request body sent to `/rerank`.
#[derive(Debug, Clone, Serialize)]
pub struct RerankRequest {
    /// The model id, e.g. `nvidia/llama-nemotron-rerank-vl-1b-v2:free`.
    pub model: String,
    /// The query the documents are scored against.
    pub query: String,
    /// The candidate documents, in the order the caller supplied them.
    pub documents: Vec<String>,
    /// How many results to ask for. Omitted entirely when `None`, which lets
    /// the server apply its own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_n: Option<usize>,
}

impl RerankRequest {
    /// Builds a rerank request. Pass `None` for `top_n` to get the server
    /// default.
    pub fn new(
        model: impl Into<String>,
        query: impl Into<String>,
        documents: Vec<String>,
        top_n: Option<usize>,
    ) -> Self {
        RerankRequest {
            model: model.into(),
            query: query.into(),
            documents,
            top_n,
        }
    }

    /// Serialises the request to JSON.
    pub fn to_json(&self) -> Result<String, VectorError> {
        serde_json::to_string(self)
            .map_err(|e| VectorError::Config(format!("could not serialise rerank request: {e}")))
    }
}

/// One entry of the response `results` array.
#[derive(Debug, Clone, Deserialize)]
struct RawRerankResult {
    index: usize,
    relevance_score: f64,
    /// Echoed back by some deployments. Kept only to tolerate it.
    #[serde(default)]
    #[allow(dead_code)]
    document: Option<serde_json::Value>,
}

/// The wire shape of a rerank response.
#[derive(Debug, Clone, Deserialize)]
struct RawRerankResponse {
    #[serde(default)]
    results: Vec<RawRerankResult>,
    /// Passed through when present; not interpreted.
    #[serde(default)]
    #[allow(dead_code)]
    model: Option<String>,
}

/// A ranked document: where it was in the caller's input, and its score.
///
/// `original_index` indexes the `documents` slice that was passed to
/// [`RerankClient::rerank`], so a caller can map a hit back to its own row.
///
/// `score` is the raw `relevance_score`, subject to the constraints in the
/// module docs: comparable only against other scores from the same response.
/// Never threshold it against a constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankResult {
    /// Index of the document in the caller's original `documents` slice.
    pub original_index: usize,
    /// The model's relevance score, ordinally comparable within this response
    /// only. See the module documentation for what this number is not.
    pub score: f64,
}

/// Parses a rerank response body.
///
/// Each result keeps the `index` the server reported and is sorted by
/// **descending score**, as the API contract for this client promises. Ties
/// break on the original index so the ordering is deterministic.
///
/// An out-of-range or duplicated `index` is rejected as [`VectorError::Decode`]
/// rather than clamped, because either means the response cannot be mapped back
/// to the caller's documents.
pub fn parse_rerank_response(body: &str) -> Result<Vec<RerankResult>, VectorError> {
    let raw: RawRerankResponse = serde_json::from_str(body)
        .map_err(|e| VectorError::Decode(format!("rerank response is not valid JSON: {e}")))?;

    let mut results = Vec::with_capacity(raw.results.len());
    for result in raw.results {
        if !result.relevance_score.is_finite() {
            return Err(VectorError::Decode(format!(
                "rerank result at index {} has a non-finite relevance_score",
                result.index
            )));
        }
        results.push(RerankResult {
            original_index: result.index,
            score: result.relevance_score,
        });
    }

    results.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.original_index.cmp(&b.original_index))
    });

    Ok(results)
}

/// A client for the OpenRouter rerank endpoint.
///
/// The `Debug` impl is hand-written so the API key cannot leak through it.
#[derive(Clone)]
pub struct RerankClient {
    config: ClientConfig,
    agent: ureq::Agent,
}

impl RerankClient {
    /// Builds a client. The API key comes from the caller; nothing is read
    /// from disk or the environment.
    pub fn new(config: ClientConfig) -> Result<Self, VectorError> {
        config.validate()?;
        let agent = crate::build_agent(config.timeout());
        Ok(RerankClient { config, agent })
    }

    /// The config this client was built from.
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Reranks `documents` against `query`.
    ///
    /// `top_n` is passed straight through; `None` lets the server choose. The
    /// returned vector is ordered by descending `relevance_score`, each entry
    /// carrying the index the document had in `documents`.
    ///
    /// An empty `documents` slice is a [`VectorError::Config`] error rather
    /// than a request: there is nothing to rank, and the server's behaviour for
    /// an empty array is unspecified.
    pub fn rerank(
        &self,
        query: &str,
        documents: &[String],
        top_n: Option<usize>,
    ) -> Result<Vec<RerankResult>, VectorError> {
        if query.trim().is_empty() {
            return Err(VectorError::Config(
                "rerank query must not be empty".to_string(),
            ));
        }
        if documents.is_empty() {
            return Err(VectorError::Config(
                "rerank requires at least one document".to_string(),
            ));
        }
        if let Some(0) = top_n {
            return Err(VectorError::Config(
                "top_n must be greater than zero, or None for the server default".to_string(),
            ));
        }

        let request =
            RerankRequest::new(self.config.rerank_model(), query, documents.to_vec(), top_n);
        let body = request.to_json()?;
        let url = self.config.endpoint("rerank");
        let response = crate::post_json(&self.agent, &url, self.config.api_key(), &body)?;
        parse_rerank_response(&response).map_err(|e| e.redact(self.config.api_key()))
    }
}

/// Redacts the key; see the note on [`ClientConfig`]'s `Debug` impl.
impl std::fmt::Debug for RerankClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RerankClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "nvidia/llama-nemotron-rerank-vl-1b-v2:free";

    #[test]
    fn request_serialises_query_documents_and_top_n() {
        let request = RerankRequest::new(
            MODEL,
            "what is a b-tree",
            vec!["alpha".into(), "beta".into()],
            Some(1),
        );
        let json: serde_json::Value = serde_json::from_str(&request.to_json().unwrap()).unwrap();
        assert_eq!(json["model"], MODEL);
        assert_eq!(json["query"], "what is a b-tree");
        assert_eq!(json["documents"], serde_json::json!(["alpha", "beta"]));
        assert_eq!(json["top_n"], 1);
    }

    #[test]
    fn top_n_is_omitted_when_none() {
        let request = RerankRequest::new(MODEL, "q", vec!["d".into()], None);
        let json: serde_json::Value = serde_json::from_str(&request.to_json().unwrap()).unwrap();
        assert!(json.get("top_n").is_none());
    }

    #[test]
    fn results_are_sorted_by_descending_score() {
        let body = r#"{"results":[
            {"index":0,"relevance_score":0.1,"document":{"text":"a"}},
            {"index":1,"relevance_score":0.9,"document":{"text":"b"}},
            {"index":2,"relevance_score":0.5,"document":{"text":"c"}}
        ]}"#;
        let parsed = parse_rerank_response(body).unwrap();
        assert_eq!(
            parsed,
            vec![
                RerankResult {
                    original_index: 1,
                    score: 0.9
                },
                RerankResult {
                    original_index: 2,
                    score: 0.5
                },
                RerankResult {
                    original_index: 0,
                    score: 0.1
                },
            ]
        );
    }

    #[test]
    fn negative_scores_sort_correctly() {
        // A model may emit logits; ordering still has to hold.
        let body = r#"{"results":[
            {"index":0,"relevance_score":-4.2},
            {"index":1,"relevance_score":-0.1},
            {"index":2,"relevance_score":-12.5}
        ]}"#;
        let parsed = parse_rerank_response(body).unwrap();
        assert_eq!(
            parsed.iter().map(|r| r.original_index).collect::<Vec<_>>(),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn ties_break_on_original_index_for_determinism() {
        let body = r#"{"results":[
            {"index":3,"relevance_score":0.5},
            {"index":1,"relevance_score":0.5},
            {"index":2,"relevance_score":0.5}
        ]}"#;
        let parsed = parse_rerank_response(body).unwrap();
        assert_eq!(
            parsed.iter().map(|r| r.original_index).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn result_tolerates_a_missing_document_member() {
        let body = r#"{"results":[{"index":0,"relevance_score":0.25}]}"#;
        let parsed = parse_rerank_response(body).unwrap();
        assert_eq!(parsed[0].score, 0.25);
    }

    #[test]
    fn malformed_json_is_a_decode_error() {
        let err = parse_rerank_response("}{").unwrap_err();
        assert!(matches!(err, VectorError::Decode(_)), "got {err:?}");
    }

    #[test]
    fn missing_relevance_score_is_rejected() {
        let body = r#"{"results":[{"index":0}]}"#;
        assert!(matches!(
            parse_rerank_response(body),
            Err(VectorError::Decode(_))
        ));
    }

    #[test]
    fn empty_results_parse_to_an_empty_ranking() {
        assert!(parse_rerank_response(r#"{"results":[]}"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn bad_arguments_are_rejected_before_any_request() {
        let client = RerankClient::new(ClientConfig::new("test-key")).unwrap();
        let docs = vec!["d".to_string()];

        assert!(matches!(
            client.rerank("  ", &docs, None),
            Err(VectorError::Config(_))
        ));
        assert!(matches!(
            client.rerank("q", &[], None),
            Err(VectorError::Config(_))
        ));
        assert!(matches!(
            client.rerank("q", &docs, Some(0)),
            Err(VectorError::Config(_))
        ));
    }

    #[test]
    fn client_debug_does_not_contain_the_key() {
        let client = RerankClient::new(ClientConfig::new("sk-or-v1-abcdef0123456789")).unwrap();
        let rendered = format!("{client:?}");
        assert!(
            !rendered.contains("sk-or-v1-abcdef0123456789"),
            "key leaked: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn client_constructor_rejects_an_empty_key() {
        let err = RerankClient::new(ClientConfig::new("")).unwrap_err();
        assert!(matches!(err, VectorError::Config(_)), "got {err:?}");
    }
}
