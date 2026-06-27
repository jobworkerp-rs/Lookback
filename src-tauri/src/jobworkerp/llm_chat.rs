//! Pure helpers for decoding `LlmChatResult` per-token chunks emitted by
//! `memories-llm` (llama-cpp-plugin). The chat command turns them into
//! `chat://step` events.

use prost::Message as _;

use crate::grpc::proto::jobworkerp::runner::llm::{
    LlmChatResult, PendingToolCalls, ToolCallRequest, ToolExecutionResult, ToolExecutionStarted,
    llm_chat_result::message_content::Content as MessageContentInner,
};

/// `tool_execution_started` boundary — surfaced as the `searching` phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedToolStarted {
    pub call_id: String,
    pub fn_name: String,
    pub job_id: i64,
    pub fn_arguments: String,
}

/// Tool-call request normalised across `pending_tool_calls` and the
/// `MessageContent::ToolCalls` assistant variant so the agent loop treats
/// both paths uniformly.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtractedToolCall {
    pub call_id: String,
    pub fn_name: String,
    pub fn_arguments: String,
}

/// `result` is the raw tool JSON — parsing into typed `ChatSource`s is
/// the chat command's responsibility (keeps this module tool-agnostic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedToolResult {
    pub call_id: String,
    pub fn_name: String,
    pub result: String,
    pub error: Option<String>,
    pub success: bool,
    pub job_id: Option<i64>,
}

/// One-pass projection of a `LlmChatResult` chunk. The streaming loop
/// touches every field per token, so decoding once and projecting beats
/// re-decoding for each phase.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtractedChunk {
    pub text: Option<String>,
    pub started: Option<ExtractedToolStarted>,
    pub results: Vec<ExtractedToolResult>,
    pub done: bool,
    pub pending_tool_calls: Vec<ExtractedToolCall>,
    pub requires_tool_execution: bool,
}

/// Decode `bytes` as `LlmChatResult` and project it onto the fields the
/// chat command cares about. Returns `None` only when prost decoding
/// itself fails; a successfully-decoded but semantically empty message
/// yields a default-valued `ExtractedChunk` (all-none, done=false).
///
/// `MessageContent::ToolCalls` (the streaming partial channel) is
/// deliberately ignored — llama-cpp-plugin's accumulator re-finalizes
/// those deltas into the canonical `pending_tool_calls` on the
/// terminal `done=true` chunk (see docs/client-tool-calling.ja.md
/// §"最終 chunk" / §"collect_stream"), and that channel is the only
/// one the agent loop consumes.
pub fn decode_chunk(bytes: &[u8]) -> Option<ExtractedChunk> {
    let chunk = LlmChatResult::decode(bytes).ok()?;
    let text = chunk.content.and_then(text_from_content);
    // proto3 int64 defaults to 0 when unset; treat `job_id == 0` as
    // "no notification" so a default-constructed message isn't mistaken
    // for a real tool start.
    let started = chunk
        .tool_execution_started
        .filter(|s| s.job_id > 0)
        .map(from_proto_started);
    let results = chunk
        .tool_execution_results
        .into_iter()
        .map(from_proto_result)
        .collect();
    let pending_tool_calls = chunk
        .pending_tool_calls
        .map(from_proto_pending)
        .unwrap_or_default();
    Some(ExtractedChunk {
        text,
        started,
        results,
        done: chunk.done,
        pending_tool_calls,
        requires_tool_execution: chunk.requires_tool_execution.unwrap_or(false),
    })
}

fn text_from_content(
    c: crate::grpc::proto::jobworkerp::runner::llm::llm_chat_result::MessageContent,
) -> Option<String> {
    match c.content {
        Some(MessageContentInner::Text(t)) if !t.is_empty() => Some(t),
        // `ToolCalls` partials are accumulated server-side and surface
        // on the final chunk's `pending_tool_calls`; the chat command
        // never needs the partial stream.
        _ => None,
    }
}

fn from_proto_pending(p: PendingToolCalls) -> Vec<ExtractedToolCall> {
    p.calls.into_iter().map(from_proto_request).collect()
}

fn from_proto_request(p: ToolCallRequest) -> ExtractedToolCall {
    ExtractedToolCall {
        call_id: p.call_id,
        fn_name: p.fn_name,
        fn_arguments: p.fn_arguments,
    }
}

fn from_proto_started(p: ToolExecutionStarted) -> ExtractedToolStarted {
    ExtractedToolStarted {
        call_id: p.call_id,
        fn_name: p.fn_name,
        job_id: p.job_id,
        fn_arguments: p.fn_arguments,
    }
}

fn from_proto_result(p: ToolExecutionResult) -> ExtractedToolResult {
    ExtractedToolResult {
        call_id: p.call_id,
        fn_name: p.fn_name,
        result: p.result,
        error: p.error,
        success: p.success,
        job_id: p.job_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::proto::jobworkerp::runner::llm::{
        llm_chat_result::{
            MessageContent,
            message_content::{Content, ToolCall, ToolCalls},
        },
        *,
    };

    fn text_chunk(text: &str, done: bool) -> Vec<u8> {
        LlmChatResult {
            content: Some(MessageContent {
                content: Some(Content::Text(text.to_string())),
            }),
            done,
            ..Default::default()
        }
        .encode_to_vec()
    }

    fn started_chunk(call_id: &str, fn_name: &str, job_id: i64) -> Vec<u8> {
        LlmChatResult {
            tool_execution_started: Some(ToolExecutionStarted {
                call_id: call_id.into(),
                fn_name: fn_name.into(),
                job_id,
                fn_arguments: "{}".into(),
            }),
            ..Default::default()
        }
        .encode_to_vec()
    }

    fn result_chunk(call_id: &str, fn_name: &str, result: &str, success: bool) -> Vec<u8> {
        LlmChatResult {
            tool_execution_results: vec![ToolExecutionResult {
                call_id: call_id.into(),
                fn_name: fn_name.into(),
                result: result.into(),
                error: None,
                success,
                job_id: Some(42),
            }],
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn decode_chunk_text_none_for_tool_call_content() {
        // ToolCalls oneof excludes Text; no renderable token surfaces.
        let bytes = LlmChatResult {
            content: Some(MessageContent {
                content: Some(Content::ToolCalls(ToolCalls {
                    calls: vec![ToolCall {
                        call_id: "c1".into(),
                        fn_name: "lookback_recall".into(),
                        fn_arguments: "{}".into(),
                        delta_index: None,
                    }],
                })),
            }),
            ..Default::default()
        }
        .encode_to_vec();
        let chunk = decode_chunk(&bytes).expect("decode");
        assert!(chunk.text.is_none());
    }

    #[test]
    fn decode_chunk_tool_started_rejects_zero_job_id() {
        // proto3 defaults int64 to 0; a default-constructed message must
        // not be mistaken for a real tool-start notification.
        let bytes = LlmChatResult {
            tool_execution_started: Some(ToolExecutionStarted::default()),
            ..Default::default()
        }
        .encode_to_vec();
        let chunk = decode_chunk(&bytes).expect("decode");
        assert!(chunk.started.is_none());
    }

    #[test]
    fn decode_chunk_extracts_tool_results() {
        let bytes = result_chunk("call-1", "lookback_recall", "[{\"x\":1}]", true);
        let chunk = decode_chunk(&bytes).expect("decode");
        assert_eq!(chunk.results.len(), 1);
        assert_eq!(chunk.results[0].call_id, "call-1");
        assert_eq!(chunk.results[0].result, "[{\"x\":1}]");
        assert!(chunk.results[0].success);
        assert_eq!(chunk.results[0].job_id, Some(42));
    }

    #[test]
    fn decode_chunk_projects_text_token() {
        let chunk = decode_chunk(&text_chunk("hello", false)).expect("decode");
        assert_eq!(chunk.text.as_deref(), Some("hello"));
        assert!(chunk.started.is_none());
        assert!(chunk.results.is_empty());
        assert!(!chunk.done);
    }

    #[test]
    fn decode_chunk_projects_tool_started() {
        let chunk = decode_chunk(&started_chunk("c", "f", 7)).expect("decode");
        let started = chunk.started.expect("started");
        assert_eq!(started.job_id, 7);
        assert!(chunk.text.is_none());
    }

    #[test]
    fn decode_chunk_projects_done_flag() {
        // Final chunk: empty text + done=true. The struct must report
        // done=true even though text is None.
        let chunk = decode_chunk(&text_chunk("", true)).expect("decode");
        assert!(chunk.text.is_none());
        assert!(chunk.done);
    }

    #[test]
    fn decode_chunk_none_for_invalid_bytes() {
        // prost decode failure → None (lets caller drop the chunk).
        assert!(decode_chunk(b"\x01\x02junk").is_none());
    }

    fn pending_chunk(calls: Vec<(&str, &str, &str)>, requires: bool, done: bool) -> Vec<u8> {
        LlmChatResult {
            pending_tool_calls: Some(PendingToolCalls {
                calls: calls
                    .into_iter()
                    .map(|(id, name, args)| ToolCallRequest {
                        call_id: id.into(),
                        fn_name: name.into(),
                        fn_arguments: args.into(),
                    })
                    .collect(),
            }),
            requires_tool_execution: Some(requires),
            done,
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn decode_chunk_extracts_pending_tool_calls() {
        // Client-side tool-calling mode: the plugin returns pending calls
        // alongside requires_tool_execution=true and expects the client to
        // invoke them before continuing the chat.
        let bytes = pending_chunk(
            vec![("call-1", "lookback_recall", "{\"query\":\"hi\"}")],
            true,
            true,
        );
        let chunk = decode_chunk(&bytes).expect("decode");
        assert_eq!(chunk.pending_tool_calls.len(), 1);
        assert_eq!(chunk.pending_tool_calls[0].call_id, "call-1");
        assert_eq!(chunk.pending_tool_calls[0].fn_name, "lookback_recall");
        assert_eq!(
            chunk.pending_tool_calls[0].fn_arguments,
            "{\"query\":\"hi\"}"
        );
        assert!(chunk.requires_tool_execution);
        assert!(chunk.done);
        assert!(chunk.text.is_none());
    }

    #[test]
    fn decode_chunk_ignores_message_content_tool_calls() {
        // `MessageContent::ToolCalls` is the partial / preview channel
        // the plugin emits as deltas. The chat command relies entirely
        // on the canonical `pending_tool_calls` re-finalized on the
        // terminal chunk, so the decoder collapses this variant to a
        // no-op (empty text, empty pending) — no separate assistant
        // tool-call surface.
        let bytes = LlmChatResult {
            content: Some(MessageContent {
                content: Some(Content::ToolCalls(ToolCalls {
                    calls: vec![ToolCall {
                        call_id: "call-2".into(),
                        fn_name: "lookback_recall".into(),
                        fn_arguments: "{}".into(),
                        delta_index: None,
                    }],
                })),
            }),
            done: false,
            ..Default::default()
        }
        .encode_to_vec();
        let chunk = decode_chunk(&bytes).expect("decode");
        assert!(chunk.text.is_none());
        assert!(chunk.pending_tool_calls.is_empty());
        assert!(!chunk.requires_tool_execution);
    }

    #[test]
    fn decode_chunk_defaults_when_no_tool_fields() {
        // A text-only chunk has empty tool-call collections and a false
        // requires-tool-execution flag (proto3 default is None → false).
        let chunk = decode_chunk(&text_chunk("hi", false)).expect("decode");
        assert!(chunk.pending_tool_calls.is_empty());
        assert!(!chunk.requires_tool_execution);
    }
}
