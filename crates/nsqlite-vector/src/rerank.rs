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
/// The parser has no idea how many documents the caller sent, so it cannot
/// tell a legitimate index from one that points past the end of the caller's
/// slice. [`parse_rerank_response_bounded`] can: it takes the document count
/// and rejects both an out-of-range and a duplicated `index`, which is what
/// makes a hit safe to use as `&documents[result.original_index]`.
/// [`RerankClient::rerank`] always goes through it.
pub fn parse_rerank_response(body: &str) -> Result<Vec<RerankResult>, VectorError> {
    parse_rerank_response_bounded(body, None)
}

/// [`parse_rerank_response`] with a document count to validate against.
///
/// `documents_len` is the length of the `documents` slice the caller sent, and
/// is `None` only when the body came from somewhere other than a request this
/// process made — a stored fixture, say — and no such bound is known.
///
/// A response that breaches the bound is rejected as [`VectorError::Decode`]
/// rather than clamped, because either fault means the response cannot be
/// mapped back to the caller's documents: an out-of-range index would have the
/// caller index past the end of its own slice, and a duplicate means two
/// entries claim one document, so taking the first would silently drop the
/// other.
pub fn parse_rerank_response_bounded(
    body: &str,
    documents_len: Option<usize>,
) -> Result<Vec<RerankResult>, VectorError> {
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
        if let Some(documents_len) = documents_len {
            if result.index >= documents_len {
                return Err(VectorError::Decode(format!(
                    "rerank response holds index {}, outside the {documents_len} documents sent",
                    result.index
                )));
            }
        }
        results.push(RerankResult {
            original_index: result.index,
            score: result.relevance_score,
        });
    }

    // Sorting by score also groups equal indices next to each other, so the
    // duplicate check is a single scan of adjacent pairs.
    results.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.original_index.cmp(&b.original_index))
    });
    for pair in results.windows(2) {
        if pair[0].original_index == pair[1].original_index {
            return Err(VectorError::Decode(format!(
                "rerank response repeats index {}",
                pair[0].original_index
            )));
        }
    }

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
    /// `top_n` is both sent in the request and enforced on the response: the
    /// result is never longer than `top_n`, and a server that answers with more
    /// documents than were sent, or with an index outside them, is reported as a
    /// [`VectorError::Decode`] rather than passed on. That is what makes it safe
    /// to index the caller's own `documents` slice with
    /// [`RerankResult::original_index`]. Pass `None` to get every document back
    /// and let the server choose how many to return.
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
        let parsed = parse_rerank_response_bounded(&response, Some(documents.len()))
            .map_err(|e| e.redact(self.config.api_key()))?;
        enforce_top_n(parsed.len(), documents.len(), top_n)?;
        Ok(parsed)
    }
}

/// Rejects a response that returned more results than the caller asked for.
///
/// A `top_n` larger than the document count is not a fault — there are only
/// `documents_len` documents to return — so the effective limit is the smaller
/// of the two. `top_n: None` means every document, which the index bounds in
/// the parser have already guaranteed.
fn enforce_top_n(
    results_len: usize,
    documents_len: usize,
    top_n: Option<usize>,
) -> Result<(), VectorError> {
    let limit = top_n.unwrap_or(documents_len).min(documents_len);
    if results_len > limit {
        return Err(VectorError::Decode(format!(
            "rerank response holds {results_len} results, more than the {limit} asked for"
        )));
    }
    Ok(())
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
    fn an_index_outside_the_documents_sent_is_rejected() {
        // Four documents were sent; index 9999 does not name one of them, and
        // the caller pattern `&documents[result.original_index]` would panic.
        let body = r#"{"results":[
            {"index":0,"relevance_score":0.9},
            {"index":9999,"relevance_score":0.1}
        ]}"#;
        let err = parse_rerank_response_bounded(body, Some(4)).unwrap_err();
        assert!(matches!(err, VectorError::Decode(_)), "got {err:?}");
        assert!(err.to_string().contains("outside the 4 documents"), "{err}");
    }

    #[test]
    fn a_duplicated_index_is_rejected() {
        let body = r#"{"results":[
            {"index":0,"relevance_score":0.9},
            {"index":0,"relevance_score":0.1}
        ]}"#;
        let err = parse_rerank_response_bounded(body, Some(4)).unwrap_err();
        assert!(matches!(err, VectorError::Decode(_)), "got {err:?}");
        assert!(err.to_string().contains("repeats index 0"), "{err}");
    }

    #[test]
    fn a_bounded_parse_accepts_indices_inside_the_documents_sent() {
        let body = r#"{"results":[
            {"index":0,"relevance_score":0.1},
            {"index":3,"relevance_score":0.9}
        ]}"#;
        let parsed = parse_rerank_response_bounded(body, Some(4)).unwrap();
        assert_eq!(
            parsed.iter().map(|r| r.original_index).collect::<Vec<_>>(),
            vec![3, 0]
        );
    }

    #[test]
    fn the_unbounded_parser_keeps_its_original_permissive_behaviour() {
        // It has no document count, so the checks above cannot apply. What it
        // still guarantees — sorting, tie order, and the non-finite rejection —
        // is unchanged.
        let body = r#"{"results":[
            {"index":0,"relevance_score":0.1},
            {"index":9999,"relevance_score":0.9}
        ]}"#;
        let parsed = parse_rerank_response(body).unwrap();
        assert_eq!(parsed[0].original_index, 9999);

        let err = parse_rerank_response(r#"{"results":[{"index":0,"relevance_score":1e400}]}"#)
            .unwrap_err();
        assert!(matches!(err, VectorError::Decode(_)), "got {err:?}");
    }

    #[test]
    fn a_duplicate_is_rejected_whatever_order_it_arrives_in() {
        // Sorting groups equal indices together whatever the scores are, so
        // neither mode can be tricked by putting the repeat out of order, and
        // a duplicate is refused even without a bound to check against.
        let body = r#"{"results":[
            {"index":1,"relevance_score":0.1},
            {"index":1,"relevance_score":0.9}
        ]}"#;
        assert!(matches!(
            parse_rerank_response_bounded(body, Some(4)),
            Err(VectorError::Decode(_))
        ));
        assert!(matches!(
            parse_rerank_response(body),
            Err(VectorError::Decode(_))
        ));
    }

    #[test]
    fn every_hit_indexes_one_of_the_callers_documents() {
        // The whole point of the bound: a hit is safe to use as
        // `&documents[result.original_index]`.
        let documents = ["a".to_string(), "b".to_string(), "c".to_string()];
        let body = r#"{"results":[
            {"index":0,"relevance_score":0.9},
            {"index":3,"relevance_score":0.8}
        ]}"#;
        let err = parse_rerank_response_bounded(body, Some(documents.len())).unwrap_err();
        assert!(matches!(err, VectorError::Decode(_)), "got {err:?}");

        // And the result the caller is promised is always indexable.
        let body = r#"{"results":[
            {"index":0,"relevance_score":0.1},
            {"index":2,"relevance_score":0.9}
        ]}"#;
        let parsed = parse_rerank_response_bounded(body, Some(documents.len())).unwrap();
        for hit in &parsed {
            assert!(
                documents.get(hit.original_index).is_some(),
                "hit {} is not a document",
                hit.original_index
            );
        }
    }

    #[test]
    fn more_results_than_top_n_is_a_decode_error() {
        // A server that ignored `top_n` and returned every document used to
        // hand back a longer vector than the caller asked for, with no error.
        let err = enforce_top_n(4, 4, Some(2)).unwrap_err();
        assert!(matches!(err, VectorError::Decode(_)), "got {err:?}");
        assert!(err.to_string().contains("4 results"), "{err}");

        // Asking for exactly what came back is fine, and a `top_n` above the
        // document count is not a fault: there are only four documents to
        // return, so four results is the answer.
        assert!(enforce_top_n(2, 4, Some(2)).is_ok());
        assert!(enforce_top_n(4, 4, None).is_ok());
        assert!(enforce_top_n(4, 4, Some(99)).is_ok());
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
