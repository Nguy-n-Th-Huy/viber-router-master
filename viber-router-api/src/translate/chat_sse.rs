//! Anthropic Messages SSE stream -> OpenAI Chat Completions SSE stream.
//!
//! Mirrors the buffering and event-framing shape of `SseUsageParser`: bytes are
//! fed in as they arrive, complete `event\ndata: ...\n\n` blocks are parsed out
//! of an internal buffer, and each translated chunk is returned immediately
//! rather than held until the stream ends.

use serde_json::{Value, json};
use uuid::Uuid;

use super::chat::map_stop_reason;

/// One in-flight Anthropic `tool_use` content block being streamed.
struct ToolCallState {
    /// Anthropic content-block index this call was started at. Kept only for
    /// lookup; the OpenAI-visible index is `openai_index`.
    content_index: u64,
    /// OpenAI's `tool_calls[].index`, assigned consecutively among tool calls
    /// only — text blocks between tool blocks do not consume an index.
    openai_index: usize,
}

/// Translates one Anthropic Messages SSE stream into Chat Completions chunks.
pub struct ChatSseTranslator {
    buffer: Vec<u8>,
    completion_id: String,
    /// The model reported on every chunk: the client's requested name when one
    /// was supplied, otherwise filled from the upstream's own
    /// `message_start.message.model`.
    model: String,
    /// When true, `model` is the client's requested name and `message_start`
    /// must not overwrite it with the upstream's (possibly mapped) name.
    client_model_pinned: bool,
    include_usage: bool,
    /// Tool calls started so far, most recent last; searched from the back
    /// since deltas almost always target the most recently opened block.
    tool_calls: Vec<ToolCallState>,
    input_tokens: i32,
    cache_read_tokens: Option<i32>,
    /// Whether the role-announcing first chunk has been emitted yet.
    started: bool,
    finished: bool,
}

impl ChatSseTranslator {
    /// `client_model`, when set, is reported as the `model` on every chunk in
    /// place of the upstream's own name, matching the non-streaming translator
    /// so a client sees one consistent model name either way.
    pub fn new(include_usage: bool, client_model: Option<&str>) -> Self {
        Self {
            buffer: Vec::new(),
            completion_id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
            model: client_model.unwrap_or_default().to_string(),
            client_model_pinned: client_model.is_some(),
            include_usage,
            tool_calls: Vec::new(),
            input_tokens: 0,
            cache_read_tokens: None,
            started: false,
            finished: false,
        }
    }

    /// Feed a chunk of raw upstream SSE bytes, returning the translated bytes
    /// to forward to the client (possibly empty, possibly several events).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(chunk);
        self.drain_events()
    }

    /// Flush any buffered partial event and terminate the stream.
    ///
    /// Anthropic streams always end with `message_stop`, which itself emits
    /// `[DONE]`; this only matters if the upstream connection closed without
    /// one, in which case the client still gets a clean terminator.
    pub fn finish(mut self) -> Vec<u8> {
        let mut out = self.drain_events();
        if !self.finished {
            out.extend_from_slice(b"data: [DONE]\n\n");
        }
        out
    }

    fn drain_events(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(pos) = find_double_newline(&self.buffer) {
            let event_bytes = self.buffer[..pos].to_vec();
            self.handle_event(&event_bytes, &mut out);
            self.buffer.drain(..pos + 2);
        }
        out
    }

    fn handle_event(&mut self, event_bytes: &[u8], out: &mut Vec<u8>) {
        let Ok(event_str) = std::str::from_utf8(event_bytes) else {
            return;
        };
        for line in event_str.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            self.handle_data(&json, out);
        }
    }

    fn handle_data(&mut self, json: &Value, out: &mut Vec<u8>) {
        let event_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match event_type {
            "message_start" => self.on_message_start(json, out),
            "content_block_start" => self.on_content_block_start(json, out),
            "content_block_delta" => self.on_content_block_delta(json, out),
            "message_delta" => self.on_message_delta(json, out),
            "message_stop" => self.on_message_stop(out),
            "error" => self.on_error(json, out),
            _ => {}
        }
    }

    fn push_chunk(&self, delta: Value, finish_reason: Option<&str>, usage: Option<Value>, out: &mut Vec<u8>) {
        let mut chunk = json!({
            "id": self.completion_id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason
            }]
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(serde_json::to_string(&chunk).unwrap_or_default().as_bytes());
        out.extend_from_slice(b"\n\n");
    }

    fn on_message_start(&mut self, json: &Value, out: &mut Vec<u8>) {
        let message = json.get("message");
        if !self.client_model_pinned
            && let Some(model) = message.and_then(|m| m.get("model")).and_then(|m| m.as_str())
        {
            self.model = model.to_string();
        }
        if let Some(usage) = message.and_then(|m| m.get("usage")) {
            self.input_tokens = usage
                .get("input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            self.cache_read_tokens = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32);
        }
        if !self.started {
            self.started = true;
            self.push_chunk(json!({"role": "assistant", "content": ""}), None, None, out);
        }
    }

    fn on_content_block_start(&mut self, json: &Value, out: &mut Vec<u8>) {
        let block = json.get("content_block").cloned().unwrap_or_default();
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            return; // Text blocks have no OpenAI start signal.
        }
        let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        let openai_index = self.tool_calls.len();
        self.tool_calls.push(ToolCallState { content_index, openai_index });

        let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
        self.push_chunk(
            json!({"tool_calls": [{
                "index": openai_index,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": ""}
            }]}),
            None,
            None,
            out,
        );
    }

    fn on_content_block_delta(&mut self, json: &Value, out: &mut Vec<u8>) {
        let delta = json.get("delta").cloned().unwrap_or_default();
        match delta.get("type").and_then(|t| t.as_str()) {
            Some("text_delta") => {
                let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                self.push_chunk(json!({"content": text}), None, None, out);
            }
            Some("input_json_delta") => {
                let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let Some(call) = self
                    .tool_calls
                    .iter()
                    .rev()
                    .find(|c| c.content_index == content_index)
                else {
                    return;
                };
                let partial = delta.get("partial_json").and_then(|p| p.as_str()).unwrap_or("");
                self.push_chunk(
                    json!({"tool_calls": [{"index": call.openai_index, "function": {"arguments": partial}}]}),
                    None,
                    None,
                    out,
                );
            }
            _ => {}
        }
    }

    fn on_message_delta(&mut self, json: &Value, out: &mut Vec<u8>) {
        let stop_reason = json
            .pointer("/delta/stop_reason")
            .and_then(|s| s.as_str());
        let finish_reason = map_stop_reason(stop_reason);
        self.push_chunk(json!({}), Some(finish_reason), None, out);

        if self.include_usage {
            let output_tokens = json
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let mut usage = json!({
                "prompt_tokens": self.input_tokens,
                "completion_tokens": output_tokens,
                "total_tokens": self.input_tokens as i64 + output_tokens
            });
            if let Some(cached) = self.cache_read_tokens {
                usage["prompt_tokens_details"] = json!({"cached_tokens": cached});
            }
            // A trailing usage-only chunk: empty choices, matching OpenAI's own
            // `stream_options.include_usage` behaviour.
            let chunk = json!({
                "id": self.completion_id,
                "object": "chat.completion.chunk",
                "model": self.model,
                "choices": [],
                "usage": usage
            });
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(serde_json::to_string(&chunk).unwrap_or_default().as_bytes());
            out.extend_from_slice(b"\n\n");
        }
    }

    fn on_message_stop(&mut self, out: &mut Vec<u8>) {
        self.finished = true;
        out.extend_from_slice(b"data: [DONE]\n\n");
    }

    fn on_error(&mut self, json: &Value, out: &mut Vec<u8>) {
        let message = json
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error");
        let error_type = json
            .pointer("/error/type")
            .and_then(|t| t.as_str())
            .unwrap_or("api_error");
        let envelope = super::error_envelope(super::ClientProtocol::ChatCompletions, error_type, message);
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(serde_json::to_string(&envelope).unwrap_or_default().as_bytes());
        out.extend_from_slice(b"\n\n");
        self.finished = true;
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
}

/// Find the byte offset of the first `\n\n` (double-newline event delimiter).
fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(event_type: &str, data: Value) -> Vec<u8> {
        format!("event: {event_type}\ndata: {data}\n\n").into_bytes()
    }

    /// Collect every `data: ...` JSON payload out of translated SSE bytes, in
    /// order, skipping the literal `[DONE]` line.
    fn parsed_chunks(bytes: &[u8]) -> Vec<Value> {
        let text = std::str::from_utf8(bytes).unwrap();
        text.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap())
            .collect()
    }

    #[test]
    fn text_only_stream_translates_role_content_and_finish() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
        )));
        all.extend(t.feed(&sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        )));
        for word in ["Hel", "lo", "!"] {
            all.extend(t.feed(&sse(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": word}}),
            )));
        }
        all.extend(t.feed(&sse(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        )));
        all.extend(t.feed(&sse(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
        )));
        all.extend(t.feed(&sse("message_stop", json!({"type": "message_stop"}))));
        all.extend(t.finish());

        let chunks = parsed_chunks(&all);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let text: String = chunks[1..4]
            .iter()
            .map(|c| c["choices"][0]["delta"]["content"].as_str().unwrap())
            .collect();
        assert_eq!(text, "Hello!");
        assert_eq!(chunks.last().unwrap()["choices"][0]["finish_reason"], "stop");
        assert!(std::str::from_utf8(&all).unwrap().ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn streaming_tool_call_announces_then_streams_arguments() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather"}}),
        )));
        all.extend(t.feed(&sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"}}),
        )));
        all.extend(t.feed(&sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"Hanoi\"}"}}),
        )));
        all.extend(t.feed(&sse(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {}}),
        )));

        let chunks = parsed_chunks(&all);
        let announce = &chunks[1]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(announce["id"], "toolu_1");
        assert_eq!(announce["function"]["name"], "get_weather");
        assert_eq!(announce["function"]["arguments"], "");

        let frag1 = &chunks[2]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(frag1["index"], 0);
        assert_eq!(frag1["function"]["arguments"], "{\"city\":");

        assert_eq!(chunks.last().unwrap()["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn two_tool_calls_around_a_text_block_get_consecutive_openai_indices() {
        // Anthropic content indices: 0 = tool_use, 1 = text, 2 = tool_use.
        // OpenAI must number the two tool calls 0 and 1 — indices count tool
        // calls only, not raw Anthropic content-block positions.
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "c1", "name": "f1"}}),
        )));
        all.extend(t.feed(&sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        )));
        all.extend(t.feed(&sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "c2", "name": "f2"}}),
        )));
        all.extend(t.feed(&sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
        )));

        let chunks = parsed_chunks(&all);
        // chunks[0] = role, [1] = announce c1 (index 0), [2] = announce c2 (index 1), [3] = fragment for c2.
        assert_eq!(chunks[1]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(chunks[2]["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
        assert_eq!(chunks[3]["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
    }

    #[test]
    fn include_usage_true_emits_final_usage_chunk() {
        let mut t = ChatSseTranslator::new(true, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
        )));
        all.extend(t.feed(&sse(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        )));

        let chunks = parsed_chunks(&all);
        let usage_chunk = chunks.iter().find(|c| c.get("usage").is_some()).unwrap();
        assert_eq!(usage_chunk["usage"]["prompt_tokens"], 10);
        assert_eq!(usage_chunk["usage"]["completion_tokens"], 5);
        assert_eq!(usage_chunk["usage"]["total_tokens"], 15);
    }

    #[test]
    fn include_usage_false_emits_no_usage_field() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
        )));
        all.extend(t.feed(&sse(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        )));

        for chunk in parsed_chunks(&all) {
            assert!(chunk.get("usage").is_none());
        }
    }

    #[test]
    fn mid_stream_error_preserves_prior_content_then_done() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}),
        )));
        all.extend(t.feed(&sse(
            "error",
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "upstream overloaded"}}),
        )));

        let text = std::str::from_utf8(&all).unwrap();
        assert!(text.contains("partial"));
        let chunks = parsed_chunks(&all);
        let error_chunk = chunks.last().unwrap();
        assert_eq!(error_chunk["error"]["message"], "upstream overloaded");
        assert!(text.trim_end().ends_with("data: [DONE]"));
    }

    /// Golden master over a realistic captured Anthropic stream: text, then two
    /// tool calls, then usage and stop.
    ///
    /// Feeds the fixture one byte at a time, which is the harshest framing the
    /// translator can face — every event boundary is crossed mid-chunk. The
    /// output is compared to a frozen baseline, so any change to the emitted
    /// chunk shape (not just the fields these tests assert on individually) has
    /// to be looked at deliberately.
    #[test]
    fn golden_master_full_stream_byte_by_byte() {
        let fixture = include_str!("testdata/anthropic_stream.sse");

        let mut t = ChatSseTranslator::new(true, None);
        let mut actual = Vec::new();
        for byte in fixture.as_bytes() {
            actual.extend(t.feed(&[*byte]));
        }
        actual.extend(t.finish());

        let actual = String::from_utf8(actual).expect("translated stream is utf-8");
        // The completion id is random per response; normalise it so the baseline
        // is stable while still proving every chunk carries the same one.
        let normalised = normalise_completion_id(&actual);

        let expected = include_str!("testdata/chat_completions_stream.golden");
        assert_eq!(normalised, expected);
    }

    /// Replace the generated `chatcmpl-<uuid>` with a fixed placeholder, and
    /// assert every occurrence was the same id.
    fn normalise_completion_id(stream: &str) -> String {
        let mut seen: Option<String> = None;
        let mut out = String::with_capacity(stream.len());
        let mut rest = stream;
        while let Some(start) = rest.find("\"chatcmpl-") {
            let id_start = start + 1; // past the opening quote
            let id_end = id_start
                + rest[id_start..]
                    .find('"')
                    .expect("completion id is quoted");
            let id = &rest[id_start..id_end];
            match &seen {
                None => seen = Some(id.to_string()),
                Some(first) => assert_eq!(first, id, "all chunks must share one completion id"),
            }
            out.push_str(&rest[..id_start]);
            out.push_str("chatcmpl-GOLDEN");
            rest = &rest[id_end..];
        }
        out.push_str(rest);
        assert!(seen.is_some(), "stream contained no completion id");
        out
    }

    /// Chat Completions has no reasoning field, so `thinking` blocks contribute
    /// nothing. Checked because the Responses translator had the opposite bug:
    /// it opened an empty item for them.
    #[test]
    fn thinking_blocks_emit_no_chunks() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {}}}))));
        let before = parsed_chunks(&all).len();
        all.extend(t.feed(&sse("content_block_start", json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}))));
        all.extend(t.feed(&sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "secret reasoning"}}))));
        all.extend(t.feed(&sse("content_block_stop", json!({"type": "content_block_stop", "index": 0}))));

        assert_eq!(parsed_chunks(&all).len(), before, "thinking must add no chunks");
        assert!(
            !String::from_utf8(all).unwrap().contains("secret reasoning"),
            "reasoning text must not leak into content"
        );
    }

    /// The Chat Completions analogue of the item_id bug: a tool-call argument
    /// fragment must never name a `tool_calls[].index` that was not announced
    /// by a preceding chunk carrying that index with its id and name.
    #[test]
    fn tool_argument_delta_without_a_start_emits_nothing() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {}}}))));
        let before = parsed_chunks(&all).len();
        // No content_block_start for index 0 — the announcement never happened.
        all.extend(t.feed(&sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{}"}}))));

        assert_eq!(
            parsed_chunks(&all).len(),
            before,
            "an unannounced tool index must not be referenced"
        );
    }

    /// Every chunk reports `choices[0].index == 0`, which is correct rather than
    /// a hardcoding bug: Anthropic Messages returns one completion, and the
    /// inbound seam rejects `n > 1`, so a second choice cannot exist.
    #[test]
    fn choice_index_is_always_zero_because_n_gt_1_is_rejected() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {}}}))));
        all.extend(t.feed(&sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}))));
        all.extend(t.feed(&sse("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}))));

        let chunks = parsed_chunks(&all);
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            if let Some(choices) = chunk["choices"].as_array()
                && !choices.is_empty()
            {
                assert_eq!(choices.len(), 1);
                assert_eq!(choices[0]["index"], 0);
            }
        }
    }

    #[test]
    fn client_model_is_echoed_on_every_chunk_over_the_upstream_name() {
        // The client asked for gpt-4o; a per-server mapping sent
        // claude-sonnet-4-6 upstream, and message_start reports that name.
        let mut t = ChatSseTranslator::new(false, Some("gpt-4o"));
        let mut all = Vec::new();
        all.extend(t.feed(&sse(
            "message_start",
            json!({"type": "message_start", "message": {"model": "claude-sonnet-4-6", "usage": {"input_tokens": 1}}}),
        )));
        all.extend(t.feed(&sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
        )));

        let chunks = parsed_chunks(&all);
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert_eq!(chunk["model"], "gpt-4o");
        }
    }

    #[test]
    fn upstream_model_is_used_when_client_model_is_absent() {
        let mut t = ChatSseTranslator::new(false, None);
        let all = t.feed(&sse(
            "message_start",
            json!({"type": "message_start", "message": {"model": "claude-sonnet-4-6", "usage": {}}}),
        ));
        let chunks = parsed_chunks(&all);
        assert_eq!(chunks[0]["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn feed_across_arbitrary_chunk_boundaries_still_parses() {
        let mut t = ChatSseTranslator::new(false, None);
        let full = sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
        );
        let mut all = t.feed(&sse("message_start", json!({"type": "message_start", "message": {"usage": {}}})));
        // Split the next event mid-line to prove buffering across feed() calls.
        let (first, second) = full.split_at(full.len() / 2);
        all.extend(t.feed(first));
        all.extend(t.feed(second));

        let chunks = parsed_chunks(&all);
        assert_eq!(chunks.last().unwrap()["choices"][0]["delta"]["content"], "hi");
    }
}
