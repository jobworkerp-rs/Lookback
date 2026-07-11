//! Client-side query embedding via the jobworkerp embedding worker.
//!
//! `MemoryVectorService.HybridSearch` requires the caller to supply a
//! `query_vectors` value (the server does NOT embed the text for hybrid,
//! unlike `SearchSemantic`). We obtain that vector by dispatching the
//! `embed_text` method of the multimodal embedding runner registered on
//! jobworkerp — the same path the chat-workflow-app BFF uses.
//!
//! The runner returns one embedding per text chunk; for a short search
//! query that is normally a single chunk. Callers that need exactly one
//! vector (hybrid search) take `values` of the first chunk.

use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::jobworkerp::JobworkerpHandle;

/// jobworkerp worker that runs the multimodal embedding runner. Overridable
/// so a memories-side rename is an env change, not a code change.
pub fn embed_worker_name() -> String {
    std::env::var("LOOKBACK_EMBED_WORKER").unwrap_or_else(|_| "memories-mm-embedding".to_string())
}

/// Method selector passed as the `using` argument. The multimodal runner
/// has no single default method, so `embed_text` MUST be specified — calling
/// the bare worker resolves to a non-embed path and fails opaquely.
pub fn embed_method() -> String {
    std::env::var("LOOKBACK_EMBED_METHOD").unwrap_or_else(|_| "embed_text".to_string())
}

/// One embedding chunk in the runner's JSON result. Only `values` is
/// load-bearing for query embedding; offsets/content are ignored.
#[derive(Debug, Deserialize)]
struct EmbeddingChunk {
    #[serde(default)]
    values: Vec<f32>,
}

/// The runner result shape. `model_info` and other fields are intentionally
/// not modeled — serde ignores unknown keys by default.
#[derive(Debug, Deserialize)]
struct EmbeddingResult {
    #[serde(default)]
    embeddings: Vec<EmbeddingChunk>,
}

/// Parse the embedding runner's JSON output and return the first chunk's
/// vector. Pure so the parsing contract is unit-tested without a live
/// worker.
///
/// Errors when the JSON doesn't parse, carries no chunks, or the first
/// chunk's `values` is empty (a query below `min_chunk_tokens` comes back
/// with no usable vector).
pub fn parse_embedding_values(json: serde_json::Value) -> AppResult<Vec<f32>> {
    let parsed: EmbeddingResult = serde_json::from_value(json)
        .map_err(|e| AppError::Jobworkerp(format!("parse embedding result: {e}")))?;
    let first = parsed
        .embeddings
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Jobworkerp("embedding result has no chunks".to_string()))?;
    if first.values.is_empty() {
        return Err(AppError::Jobworkerp(
            "embedding result chunk has empty values".to_string(),
        ));
    }
    Ok(first.values)
}

fn embed_text_arguments(text: &str, prefix: Option<&str>) -> serde_json::Value {
    match prefix.filter(|value| !value.is_empty()) {
        Some(prefix) => serde_json::json!({ "text": text, "prefix": prefix }),
        None => serde_json::json!({ "text": text }),
    }
}

/// Embed a search query into a single vector via the jobworkerp embedding
/// worker. Empty/whitespace input is rejected before dispatch so the
/// failure is observable rather than a 5xx from the runner.
pub async fn embed_query(handle: &JobworkerpHandle, text: &str) -> AppResult<Vec<f32>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(AppError::Config("embedding text must be non-empty".into()));
    }
    let worker = embed_worker_name();
    let method = embed_method();
    // MultimodalEmbeddingRunner is registered with response_type=DIRECT and
    // rejects the streaming enqueue path ("runner does not support
    // streaming"), so query embedding must use the unary dispatch.
    let result = handle
        .dispatch_unary(
            &worker,
            embed_text_arguments(
                trimmed,
                std::env::var("MEMORY_EMBEDDING_QUERY_PREFIX")
                    .ok()
                    .as_deref(),
            ),
            Some(&method),
        )
        .await?;
    parse_embedding_values(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_embedding_values_reads_first_chunk() {
        let json = serde_json::json!({
            "embeddings": [{"values": [0.1, 0.2, 0.3]}],
            "model_info": {"model_name": "x"}
        });
        assert_eq!(parse_embedding_values(json).unwrap(), vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn parse_embedding_values_takes_first_of_multiple_chunks() {
        let json = serde_json::json!({"embeddings": [{"values": [1.0]}, {"values": [2.0]}]});
        assert_eq!(parse_embedding_values(json).unwrap(), vec![1.0]);
    }

    #[test]
    fn parse_embedding_values_ignores_unknown_fields() {
        // model_info / begin_position etc. must not break parsing.
        let json = serde_json::json!({
            "embeddings": [{"values": [0.5], "begin_position": 0, "end_position": 4, "content": "hi"}],
            "extra": 42
        });
        assert_eq!(parse_embedding_values(json).unwrap(), vec![0.5]);
    }

    #[test]
    fn parse_embedding_values_errors_on_empty_chunks() {
        let err = parse_embedding_values(serde_json::json!({"embeddings": []})).unwrap_err();
        assert!(matches!(err, AppError::Jobworkerp(_)));
    }

    #[test]
    fn parse_embedding_values_errors_on_empty_values() {
        let err = parse_embedding_values(serde_json::json!({"embeddings": [{"values": []}]}))
            .unwrap_err();
        assert!(matches!(err, AppError::Jobworkerp(_)));
    }

    #[test]
    fn parse_embedding_values_errors_on_malformed_shape() {
        // A non-object payload can't deserialize into EmbeddingResult.
        let err = parse_embedding_values(serde_json::json!("not an object")).unwrap_err();
        assert!(matches!(err, AppError::Jobworkerp(_)));
    }

    #[test]
    fn embed_text_arguments_keep_prefix_out_of_source_text() {
        assert_eq!(
            embed_text_arguments("本文", Some("検索クエリ: ")),
            serde_json::json!({ "text": "本文", "prefix": "検索クエリ: " })
        );
        assert_eq!(
            embed_text_arguments("plain", None),
            serde_json::json!({ "text": "plain" })
        );
    }

    #[test]
    fn embed_worker_name_defaults_and_overrides() {
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::remove_var("LOOKBACK_EMBED_WORKER") };
        assert_eq!(embed_worker_name(), "memories-mm-embedding");
        unsafe { std::env::set_var("LOOKBACK_EMBED_WORKER", "custom-embed") };
        assert_eq!(embed_worker_name(), "custom-embed");
        unsafe { std::env::remove_var("LOOKBACK_EMBED_WORKER") };
    }

    #[test]
    fn embed_method_defaults_and_overrides() {
        unsafe { std::env::remove_var("LOOKBACK_EMBED_METHOD") };
        assert_eq!(embed_method(), "embed_text");
        unsafe { std::env::set_var("LOOKBACK_EMBED_METHOD", "embed_v2") };
        assert_eq!(embed_method(), "embed_v2");
        unsafe { std::env::remove_var("LOOKBACK_EMBED_METHOD") };
    }
}
