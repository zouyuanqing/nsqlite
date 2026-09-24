//! Embeddings client for `POST {base_url}/embeddings`.
//!
//! The one subtlety worth reading before using this module: OpenRouter does
//! **not** promise that the `data` array comes back in input order. Every entry
//! carries an `index`, and that index — not the array position — is what
//! identifies which input a vector belongs to. [`parse_embed_response`] sorts
//! by it, and every public method here relies on that, so `embed_batch(&texts)`
//! returns vectors in exactly the order of `texts`.

use serde::{Deserialize, Serialize};

use crate::config::ClientConfig;
use crate::error::VectorError;

/// Largest number of inputs sent in a single embeddings request.
///
/// OpenRouter does not document a hard cap, but long arrays are the easy way to
/// trip a gateway's body limit, and 64 is a batch size that keeps a single
/// chunked request comfortably inside every provider's limits.
pub const MAX_BATCH_SIZE: usize = 64;

/// How the vectors are encoded in the response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EncodingFormat {
    /// A JSON array of numbers per vector. Readable, and the default.
    #[default]
    Float,
    /// One base64 string per vector holding little-endian `f32`s. Roughly a
    /// third of the bytes over the wire, which matters for large batches.
    Base64,
}

/// The `input` member of an embeddings request: one string or a batch of them.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    /// A single text.
    Text(String),
    /// A batch of texts.
    Texts(Vec<String>),
}

impl From<&str> for EmbeddingInput {
    fn from(text: &str) -> Self {
        EmbeddingInput::Text(text.to_string())
    }
}

impl From<&[String]> for EmbeddingInput {
    fn from(texts: &[String]) -> Self {
        EmbeddingInput::Texts(texts.to_vec())
    }
}

/// The request body sent to `/embeddings`.
#[derive(Debug, Clone, Serialize)]
pub struct EmbedRequest {
    /// The model id, e.g. `nvidia/llama-nemotron-embed-vl-1b-v2:free`.
    pub model: String,
    /// The text or texts to embed.
    pub input: EmbeddingInput,
    /// How the response vectors are encoded.
    pub encoding_format: EncodingFormat,
    /// Requested output dimensionality, for models that support truncation.
    /// Omitted entirely when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<usize>,
}

impl EmbedRequest {
    /// Builds a request for `input` using `model`, with the given encoding.
    pub fn new(
        model: impl Into<String>,
        input: impl Into<EmbeddingInput>,
        encoding_format: EncodingFormat,
    ) -> Self {
        EmbedRequest {
            model: model.into(),
            input: input.into(),
            encoding_format,
            dimensions: None,
        }
    }

    /// Sets the `dimensions` member of the request.
    pub fn with_dimensions(mut self, dimensions: usize) -> Self {
        self.dimensions = Some(dimensions);
        self
    }

    /// Serialises the request to JSON.
    pub fn to_json(&self) -> Result<String, VectorError> {
        serde_json::to_string(self).map_err(|e| {
            VectorError::Config(format!("could not serialise embeddings request: {e}"))
        })
    }
}

/// The raw `embedding` member of a response entry.
///
/// OpenRouter documents this as either an array of floats or, when
/// `encoding_format` is `base64`, a string. Both shapes are accepted regardless
/// of what was requested, so a server that ignores the parameter still parses.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawEmbedding {
    Floats(Vec<f64>),
    Base64(String),
}

/// One entry of the response `data` array.
#[derive(Debug, Clone, Deserialize)]
struct RawEmbeddingEntry {
    index: usize,
    embedding: RawEmbedding,
}

/// A parsed, order-corrected response.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedResponse {
    /// The model that actually served the request, which may differ from the
    /// requested one if OpenRouter routed it to a fallback.
    pub model: String,
    /// Vectors sorted by their `index`, ascending.
    pub embeddings: Vec<Vec<f64>>,
    /// Raw `usage` object, passed through unparsed because the shape is
    /// provider-specific and this crate does not interpret it.
    pub usage: Option<serde_json::Value>,
}

/// The wire shape of an embeddings response.
#[derive(Debug, Clone, Deserialize)]
struct RawEmbedResponse {
    #[serde(default)]
    data: Vec<RawEmbeddingEntry>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
    #[serde(default)]
    model: String,
}

/// Parses an embeddings response body.
///
/// Sorts `data` by `index`, decodes any base64 vectors, and rejects a body
/// whose indices are duplicated — a duplicate means two entries claim the same
/// input, and silently dropping one would put the wrong vector at that
/// position. Pass `expected` to also require exactly that many vectors, which
/// is what catches a server that quietly returns fewer than it was given.
/// Returns [`VectorError::Decode`] for anything malformed.
pub fn parse_embed_response(
    body: &str,
    expected: Option<usize>,
) -> Result<EmbedResponse, VectorError> {
    let raw: RawEmbedResponse = serde_json::from_str(body)
        .map_err(|e| VectorError::Decode(format!("embeddings response is not valid JSON: {e}")))?;

    let mut indexed = Vec::with_capacity(raw.data.len());
    for entry in raw.data {
        let vector = decode_embedding(&entry.embedding).ok_or_else(|| {
            VectorError::Decode(format!(
                "embedding at index {} could not be decoded",
                entry.index
            ))
        })?;
        indexed.push((entry.index, vector));
    }

    // The whole reason this function exists: array order is not guaranteed.
    indexed.sort_by_key(|(index, _)| *index);
    for pair in indexed.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(VectorError::Decode(format!(
                "embeddings response repeats index {}",
                pair[0].0
            )));
        }
    }
    if let Some(expected) = expected {
        if indexed.len() != expected {
            return Err(VectorError::Decode(format!(
                "embeddings response holds {} vectors, expected {expected}",
                indexed.len()
            )));
        }
        // Sorted above, so the last entry carries the highest index.
        if let Some((index, _)) = indexed.last() {
            if *index >= expected {
                return Err(VectorError::Decode(format!(
                    "embeddings response holds index {index}, outside the {expected} inputs sent"
                )));
            }
        }
    }

    Ok(EmbedResponse {
        model: raw.model,
        embeddings: indexed.into_iter().map(|(_, vector)| vector).collect(),
        usage: raw.usage,
    })
}

/// Decodes one `embedding` member into a `Vec<f64>`.
///
/// `Err` means the value is neither a float array nor a base64 string of
/// whole little-endian `f32`s.
fn decode_embedding(raw: &RawEmbedding) -> Option<Vec<f64>> {
    match raw {
        RawEmbedding::Floats(values) => {
            if values.iter().any(|v| !v.is_finite()) {
                return None;
            }
            Some(values.clone())
        }
        RawEmbedding::Base64(encoded) => {
            let bytes = base64_decode(encoded)?;
            if bytes.len() % 4 != 0 || bytes.is_empty() {
                return None;
            }
            Some(
                bytes
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64)
                    .collect(),
            )
        }
    }
}

/// Decodes standard base64, with or without padding, ignoring ASCII whitespace.
///
/// This is deliberately a dozen lines rather than a dependency: the format is
/// fixed by the OpenAI-compatible schema OpenRouter implements, and the crate's
/// dependency list stays at the three the HTTP client actually needs.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    let mut padding = 0usize;

    for byte in input.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padding += 1;
            continue;
        }
        if padding > 0 {
            // Data after padding is malformed.
            return None;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    // Leftover bits must be zero, otherwise the encoding is corrupt.
    if bits >= 6 || (buffer & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

/// A client for the OpenRouter embeddings endpoint.
///
/// The `Debug` impl is hand-written: the API key lives inside the config and
/// must not appear in a log line, a panic message, or a test failure.
#[derive(Clone)]
pub struct EmbedClient {
    config: ClientConfig,
    agent: ureq::Agent,
}

impl EmbedClient {
    /// Builds a client. The API key comes from the caller; nothing is read
    /// from disk or the environment.
    ///
    /// The config is validated here, so a bad URL or an empty key is reported
    /// immediately rather than on the first call.
    pub fn new(config: ClientConfig) -> Result<Self, VectorError> {
        config.validate()?;
        let agent = crate::build_agent(config.timeout());
        Ok(EmbedClient { config, agent })
    }

    /// The config this client was built from.
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Embeds a single text and returns its vector.
    pub fn embed_one(&self, text: &str) -> Result<Vec<f64>, VectorError> {
        self.embed_one_with(text, EncodingFormat::Float, None)
    }

    /// Embeds a single text with an explicit encoding and dimensionality.
    pub fn embed_one_with(
        &self,
        text: &str,
        encoding_format: EncodingFormat,
        dimensions: Option<usize>,
    ) -> Result<Vec<f64>, VectorError> {
        if text.trim().is_empty() {
            return Err(VectorError::Config("cannot embed empty text".to_string()));
        }
        let mut request = EmbedRequest::new(
            self.config.embed_model(),
            EmbeddingInput::Text(text.to_string()),
            encoding_format,
        );
        request.dimensions = dimensions;
        let response = self.send(&request, Some(1))?;
        response.embeddings.into_iter().next().ok_or_else(|| {
            VectorError::Decode("embeddings response contained no vectors".to_string())
        })
    }

    /// Embeds a batch of texts, returning one vector per input **in input
    /// order**.
    ///
    /// The batch is split into chunks of at most [`MAX_BATCH_SIZE`], each chunk
    /// is a separate request, and each response is re-sorted by `index` before
    /// the results are concatenated. An empty input yields an empty vector
    /// without touching the network.
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, VectorError> {
        self.embed_batch_with(texts, EncodingFormat::Float, None)
    }

    /// Like [`EmbedClient::embed_batch`], with an explicit encoding and
    /// dimensionality applied to every chunk.
    pub fn embed_batch_with(
        &self,
        texts: &[String],
        encoding_format: EncodingFormat,
        dimensions: Option<usize>,
    ) -> Result<Vec<Vec<f64>>, VectorError> {
        if let Some(position) = texts.iter().position(|t| t.trim().is_empty()) {
            return Err(VectorError::Config(format!(
                "cannot embed empty text at position {position}"
            )));
        }

        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(MAX_BATCH_SIZE) {
            let mut request = EmbedRequest::new(
                self.config.embed_model(),
                EmbeddingInput::Texts(chunk.to_vec()),
                encoding_format,
            );
            request.dimensions = dimensions;
            // The count check lives in the parser so a short or over-long
            // response is one code path, and so it is directly testable.
            let response = self.send(&request, Some(chunk.len()))?;
            out.extend(response.embeddings);
        }
        Ok(out)
    }

    /// Sends one request and parses it, mapping every failure mode onto
    /// [`VectorError`].
    ///
    /// `expected_inputs` is the number of inputs in `request`; `None` skips the
    /// count check, which is only right for a caller parsing a body it did not
    /// just build.
    fn send(
        &self,
        request: &EmbedRequest,
        expected_inputs: Option<usize>,
    ) -> Result<EmbedResponse, VectorError> {
        let body = request.to_json()?;
        let url = self.config.endpoint("embeddings");
        let response = crate::post_json(&self.agent, &url, self.config.api_key(), &body)?;
        parse_embed_response(&response, expected_inputs)
            .map_err(|e| e.redact(self.config.api_key()))
    }
}

/// Redacts the key; see the note on [`ClientConfig`]'s `Debug` impl.
impl std::fmt::Debug for EmbedClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "nvidia/llama-nemotron-embed-vl-1b-v2:free";

    #[test]
    fn request_serialises_a_single_string_input() {
        let request = EmbedRequest::new(MODEL, "hello", EncodingFormat::Float);
        let json: serde_json::Value = serde_json::from_str(&request.to_json().unwrap()).unwrap();
        assert_eq!(json["model"], MODEL);
        assert_eq!(json["input"], "hello");
        assert_eq!(json["encoding_format"], "float");
        // Omitted rather than sent as null.
        assert!(json.get("dimensions").is_none());
    }

    #[test]
    fn request_serialises_a_batch_input_and_dimensions() {
        let request = EmbedRequest::new(
            MODEL,
            EmbeddingInput::Texts(vec!["a".into(), "b".into()]),
            EncodingFormat::Base64,
        )
        .with_dimensions(768);
        let json: serde_json::Value = serde_json::from_str(&request.to_json().unwrap()).unwrap();
        assert_eq!(json["input"], serde_json::json!(["a", "b"]));
        assert_eq!(json["encoding_format"], "base64");
        assert_eq!(json["dimensions"], 768);
    }

    #[test]
    fn out_of_order_indices_are_sorted() {
        // Vector 1 comes back first; without sorting this would be reversed.
        let body = r#"{
            "model": "nvidia/llama-nemotron-embed-vl-1b-v2:free",
            "data": [
                {"index": 2, "embedding": [0.3]},
                {"index": 0, "embedding": [0.1]},
                {"index": 1, "embedding": [0.2]}
            ],
            "usage": {"prompt_tokens": 7, "total_tokens": 7}
        }"#;
        let parsed = parse_embed_response(body, Some(3)).unwrap();
        assert_eq!(parsed.embeddings, vec![vec![0.1], vec![0.2], vec![0.3]]);
        assert_eq!(parsed.model, MODEL);
        assert_eq!(parsed.usage.unwrap()["prompt_tokens"], 7);
    }

    #[test]
    fn base64_encoding_format_is_decoded() {
        // Little-endian f32 for [1.0, -2.0, 0.5].
        let encoded = base64_encode(
            &[1.0f32, -2.0, 0.5]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        let body =
            format!(r#"{{"model":"{MODEL}","data":[{{"index":0,"embedding":"{encoded}"}}]}}"#);
        let parsed = parse_embed_response(&body, Some(1)).unwrap();
        assert_eq!(parsed.embeddings.len(), 1);
        assert_eq!(parsed.embeddings[0], vec![1.0, -2.0, 0.5]);
    }

    #[test]
    fn base64_is_decoded_regardless_of_array_order() {
        let encoded = base64_encode(&[3.5f32.to_le_bytes()].concat());
        let body = format!(
            r#"{{"model":"m","data":[
                {{"index":1,"embedding":"{encoded}"}},
                {{"index":0,"embedding":[9.0]}}
            ]}}"#
        );
        let parsed = parse_embed_response(&body, Some(2)).unwrap();
        assert_eq!(parsed.embeddings, vec![vec![9.0], vec![3.5]]);
    }

    #[test]
    fn duplicate_indices_are_rejected() {
        let body = r#"{"model":"m","data":[
            {"index":0,"embedding":[1.0]},
            {"index":0,"embedding":[2.0]}
        ]}"#;
        let err = parse_embed_response(body, None).unwrap_err();
        assert!(matches!(err, VectorError::Decode(_)), "got {err:?}");
        assert!(err.to_string().contains("repeats index 0"));
    }

    #[test]
    fn a_wrong_vector_count_is_rejected() {
        // Three inputs sent, two vectors back: without this check the caller
        // would silently get an embedding for the wrong document.
        let body = r#"{"model":"m","data":[
            {"index":0,"embedding":[1.0]},
            {"index":1,"embedding":[2.0]}
        ]}"#;
        let err = parse_embed_response(body, Some(3)).unwrap_err();
        assert!(err.to_string().contains("2 vectors, expected 3"), "{err}");
    }

    #[test]
    fn an_index_outside_the_input_range_is_rejected() {
        // Right count, wrong identities: a gap where an index should have been.
        let body = r#"{"model":"m","data":[
            {"index":0,"embedding":[1.0]},
            {"index":5,"embedding":[2.0]}
        ]}"#;
        let err = parse_embed_response(body, Some(2)).unwrap_err();
        assert!(err.to_string().contains("index 5"), "{err}");
    }

    #[test]
    fn a_shorter_response_parses_when_no_count_is_expected() {
        let body = r#"{"model":"m","data":[{"index":0,"embedding":[1.0]}]}"#;
        let parsed = parse_embed_response(body, None).unwrap();
        assert_eq!(parsed.embeddings, vec![vec![1.0]]);
    }

    #[test]
    fn malformed_json_is_a_decode_error() {
        let err = parse_embed_response("not json at all", None).unwrap_err();
        assert!(matches!(err, VectorError::Decode(_)), "got {err:?}");
    }

    #[test]
    fn base64_that_is_not_whole_f32s_is_rejected() {
        // 3 bytes: not a multiple of 4.
        let encoded = base64_encode(&[1, 2, 3]);
        let body = format!(r#"{{"model":"m","data":[{{"index":0,"embedding":"{encoded}"}}]}}"#);
        assert!(matches!(
            parse_embed_response(&body, None),
            Err(VectorError::Decode(_))
        ));
    }

    #[test]
    fn non_finite_floats_are_rejected() {
        let body = r#"{"model":"m","data":[{"index":0,"embedding":[1.0,null]}]}"#;
        assert!(matches!(
            parse_embed_response(body, None),
            Err(VectorError::Decode(_))
        ));
    }

    #[test]
    fn empty_data_parses_to_no_vectors() {
        let parsed = parse_embed_response(r#"{"model":"m","data":[]}"#, None).unwrap();
        assert!(parsed.embeddings.is_empty());
    }

    #[test]
    fn empty_data_fails_a_count_that_expected_vectors() {
        let err = parse_embed_response(r#"{"model":"m","data":[]}"#, Some(1)).unwrap_err();
        assert!(err.to_string().contains("0 vectors, expected 1"), "{err}");
    }

    #[test]
    fn base64_decode_rejects_invalid_input() {
        assert!(base64_decode("****").is_none());
        assert!(base64_decode("A=BC").is_none());
        assert!(base64_decode("AB").is_none(), "6 leftover bits");
    }

    #[test]
    fn base64_round_trips_through_the_test_encoder() {
        for case in [
            vec![],
            vec![0u8],
            vec![1, 2, 3],
            (0..=255u8).collect::<Vec<u8>>(),
        ] {
            let encoded = base64_encode(&case);
            assert_eq!(base64_decode(&encoded).unwrap(), case);
        }
    }

    /// Test-only encoder, the inverse of `base64_decode`.
    fn base64_encode(input: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let bits = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            out.push(ALPHABET[(bits >> 18 & 63) as usize] as char);
            out.push(ALPHABET[(bits >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(bits >> 6 & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[(bits & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn chunking_covers_a_batch_larger_than_one_request() {
        // The public guarantee is chunk size, so assert on the constant and on
        // the chunk arithmetic rather than on network behaviour.
        assert_eq!(MAX_BATCH_SIZE, 64);
        let inputs: Vec<String> = (0..130).map(|i| i.to_string()).collect();
        let chunks: Vec<_> = inputs.chunks(MAX_BATCH_SIZE).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 64);
        assert_eq!(chunks[2].len(), 2);
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), inputs.len());
    }

    #[test]
    fn empty_text_is_a_config_error_before_any_request() {
        let client = EmbedClient::new(ClientConfig::new("test-key")).unwrap();
        let err = client.embed_one("   ").unwrap_err();
        assert!(matches!(err, VectorError::Config(_)), "got {err:?}");

        let err = client
            .embed_batch(&["ok".to_string(), String::new()])
            .unwrap_err();
        match err {
            VectorError::Config(message) => assert!(message.contains("position 1")),
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn empty_batch_short_circuits() {
        let client = EmbedClient::new(ClientConfig::new("test-key")).unwrap();
        assert!(client.embed_batch(&[]).unwrap().is_empty());
    }

    #[test]
    fn client_debug_does_not_contain_the_key() {
        let client = EmbedClient::new(ClientConfig::new("sk-or-v1-abcdef0123456789")).unwrap();
        let rendered = format!("{client:?}");
        assert!(
            !rendered.contains("sk-or-v1-abcdef0123456789"),
            "key leaked: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn client_constructor_rejects_an_empty_key() {
        let err = EmbedClient::new(ClientConfig::new("")).unwrap_err();
        assert!(matches!(err, VectorError::Config(_)), "got {err:?}");
    }
}
