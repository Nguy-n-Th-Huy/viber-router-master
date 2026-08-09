//! Anthropic Messages SSE stream -> OpenAI Chat Completions SSE stream.
//!
//! Mirrors the buffering and event-framing shape of `SseUsageParser`: bytes are
//! fed in as they arrive, complete `event\ndata: ...\n\n` blocks are parsed out
//! of an internal buffer, and each translated chunk is returned immediately
//! rather than held until the stream ends.

use serde_json::{Value, json};
use uuid::Uuid;

use super::{chat::map_stop_reason, tool_arguments_from_input};

/// One in-flight Anthropic `tool_use` content block being streamed.
struct ToolCallState {
    /// Anthropic content-block index this call was started at. Kept only for
    /// lookup; the OpenAI-visible index is `openai_index`.
    content_index: u64,
    /// OpenAI's `tool_calls[].index`, assigned consecutively among tool calls
    /// only — text blocks between tool blocks do not consume an index.
    openai_index: usize,
    /// Accumulated `input_json_delta` fragments, held rather than forwarded.
    ///
    /// Chat Completions has no event carrying a tool call's *final* arguments —
    /// a client concatenates the fragments itself. An individual fragment is
    /// never valid JSON on its own, so if the stream is cut mid-argument the
    /// client is left holding something like `{"command":["pwsh",` and its
    /// strict parser fails. Forwarding fragments as they arrive makes that
    /// unavoidable, so they are buffered here and emitted as one validated
    /// fragment when the block closes.
    accumulated: String,
    /// The `input` carried on `content_block_start`, pre-normalised by
    /// `tool_arguments_from_input` (so always valid JSON, `"{}"` when there was
    /// none). Some upstreams put the whole tool input there and stream no
    /// `input_json_delta`; it is also the fallback when the accumulated
    /// fragments do not parse.
    start_input: String,
    /// Whether the single arguments fragment has been emitted for this call.
    closed: bool,
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
            // A stream cut before message_delta leaves tool calls with their
            // arguments still buffered. Flush them (resolved, so always valid
            // JSON) rather than dropping the call entirely.
            self.close_pending_tool_calls(&mut out);
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
            "content_block_stop" => self.on_content_block_stop(json, out),
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
        let start_input = tool_arguments_from_input(block.get("input"));
        self.tool_calls.push(ToolCallState {
            content_index,
            openai_index,
            accumulated: String::new(),
            start_input,
            closed: false,
        });

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
                // Buffered, not forwarded — see `accumulated`'s doc comment.
                let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let Some(call) = self
                    .tool_calls
                    .iter_mut()
                    .rev()
                    .find(|c| c.content_index == content_index)
                else {
                    return;
                };
                let partial = delta.get("partial_json").and_then(|p| p.as_str()).unwrap_or("");
                call.accumulated.push_str(partial);
            }
            _ => {}
        }
    }

    /// Resolve a tool call's final `arguments`, mirroring
    /// `ResponsesSseTranslator::resolve_arguments`: the accumulated fragments
    /// win, but only if they parse as a JSON object; a cut mid-argument leaves
    /// something like `{"command":["pwsh",`, which a client's strict parser
    /// rejects before it ever looks at whether the call finished. Falls back to
    /// `start_input` (already valid JSON, `"{}"` when there was none).
    fn resolve_arguments(accumulated: &str, start_input: &str) -> String {
        let is_object = |s: &str| matches!(serde_json::from_str::<Value>(s), Ok(v) if v.is_object());
        if is_object(accumulated) { accumulated.to_string() } else { start_input.to_string() }
    }

    /// Emit the single arguments fragment for every tool call not yet closed.
    fn close_pending_tool_calls(&mut self, out: &mut Vec<u8>) {
        let pending: Vec<(usize, String)> = self
            .tool_calls
            .iter_mut()
            .filter(|c| !c.closed)
            .map(|c| {
                c.closed = true;
                (c.openai_index, Self::resolve_arguments(&c.accumulated, &c.start_input))
            })
            .collect();
        for (openai_index, arguments) in pending {
            self.push_chunk(
                json!({"tool_calls": [{"index": openai_index, "function": {"arguments": arguments}}]}),
                None,
                None,
                out,
            );
        }
    }

    /// Emit this one tool call's resolved arguments as a single fragment. Only
    /// this call, so a sibling tool call still mid-stream is untouched.
    fn on_content_block_stop(&mut self, json: &Value, out: &mut Vec<u8>) {
        let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        let Some(call) = self.tool_calls.iter_mut().find(|c| c.content_index == content_index) else {
            return;
        };
        if call.closed {
            return;
        }
        call.closed = true;
        let openai_index = call.openai_index;
        let arguments = Self::resolve_arguments(&call.accumulated, &call.start_input);
        self.push_chunk(
            json!({"tool_calls": [{"index": openai_index, "function": {"arguments": arguments}}]}),
            None,
            None,
            out,
        );
    }

    fn on_message_delta(&mut self, json: &Value, out: &mut Vec<u8>) {
        // Anthropic closes every block before message_delta, so this normally
        // finds nothing; it matters only if a content_block_stop went missing,
        // and must run before the finish chunk so fragments precede it.
        self.close_pending_tool_calls(out);

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

    /// Concatenate every `arguments` fragment for one tool index, the way a
    /// client does. The opening announcement's `""` contributes nothing.
    fn assembled_arguments(bytes: &[u8], index: u64) -> String {
        parsed_chunks(bytes)
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["tool_calls"].as_array().cloned())
            .flatten()
            .filter(|tc| tc["index"] == index)
            .filter_map(|tc| tc["function"]["arguments"].as_str().map(str::to_string))
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

        // Fragments are buffered and emitted as one validated value, so a
        // stream cut mid-argument can never leave the client holding a partial
        // fragment its strict parser rejects.
        let assembled = assembled_arguments(&all, 0);
        assert_eq!(assembled, "{\"city\":\"Hanoi\"}");
        serde_json::from_str::<Value>(&assembled).expect("must parse");

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
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"b\":2}"}}),
        )));
        // Arguments are flushed when the call closes; nothing closes the blocks
        // here, so finish() is what releases them.
        all.extend(t.finish());

        let chunks = parsed_chunks(&all);
        // chunks[0] = role, [1] = announce c1 (index 0), [2] = announce c2 (index 1).
        assert_eq!(chunks[1]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(chunks[1]["choices"][0]["delta"]["tool_calls"][0]["id"], "c1");
        assert_eq!(chunks[2]["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
        assert_eq!(chunks[2]["choices"][0]["delta"]["tool_calls"][0]["id"], "c2");

        // Each call keeps its own arguments under its own index: c1 streamed
        // nothing, c2 streamed a value.
        assert_eq!(assembled_arguments(&all, 0), "{}");
        assert_eq!(assembled_arguments(&all, 1), "{\"b\":2}");
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

    /// A second baseline for the truncation path: the recorded stream ends
    /// mid-`input_json_delta`, so this pins that the single emitted arguments
    /// fragment is the start event's input rather than the partial value, and
    /// that `[DONE]` still terminates the stream.
    #[test]
    fn golden_master_truncated_stream_byte_by_byte() {
        let fixture = include_str!("testdata/anthropic_stream_truncated.sse");

        let mut t = ChatSseTranslator::new(true, None);
        let mut actual = Vec::new();
        for byte in fixture.as_bytes() {
            actual.extend(t.feed(&[*byte]));
        }
        actual.extend(t.finish());

        crate::translate::assert_tool_arguments_always_valid(&actual, "truncated golden");

        let actual = String::from_utf8(actual).expect("translated stream is utf-8");
        let normalised = normalise_completion_id(&actual);

        let expected = include_str!("testdata/chat_completions_stream_truncated.golden");
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
    /// A tool taking no parameters streams no `input_json_delta`, so a client
    /// concatenating `function.arguments` fragments ends up with `""` — which
    /// then fails the inbound seam's JSON parse when echoed back on the next
    /// turn. One `"{}"` fragment must be emitted so the concatenation is valid.
    #[test]
    fn tool_call_with_no_argument_deltas_yields_an_empty_object() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {}}}))));
        all.extend(t.feed(&sse("content_block_start", json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}}
        }))));
        // No input_json_delta at all.
        all.extend(t.feed(&sse("content_block_stop", json!({"type": "content_block_stop", "index": 0}))));
        all.extend(t.feed(&sse("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}}))));

        let assembled = assembled_arguments(&all, 0);
        assert_eq!(assembled, "{}", "assembled arguments must be valid JSON");
        serde_json::from_str::<Value>(&assembled).expect("arguments must parse");
    }

    /// Sweeps the invariant across every tool-call shape this translator can
    /// produce. Mirrors `responses_sse`'s test of the same name.
    #[test]
    fn no_stream_shape_ever_emits_unparseable_arguments() {
        let shapes: Vec<(&str, Vec<Value>, bool)> = vec![
            ("start input only", vec![json!({"type": "tool_use", "id": "t", "name": "f", "input": {"a": 1}})], false),
            ("no input at all", vec![json!({"type": "tool_use", "id": "t", "name": "f"})], false),
            ("null input", vec![json!({"type": "tool_use", "id": "t", "name": "f", "input": Value::Null})], false),
            ("empty input", vec![json!({"type": "tool_use", "id": "t", "name": "f", "input": {}})], false),
            ("cut mid-argument", vec![json!({"type": "tool_use", "id": "t", "name": "f", "input": {"a": 1}})], true),
            (
                "text then tool",
                vec![
                    json!({"type": "text", "text": ""}),
                    json!({"type": "tool_use", "id": "t", "name": "f", "input": {"a": 1}}),
                ],
                false,
            ),
        ];

        for (label, blocks, cut) in shapes {
            for with_deltas in [false, true] {
                for partial in [false, true] {
                    let mut t = ChatSseTranslator::new(false, None);
                    let mut all = Vec::new();
                    all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"usage": {}}}))));
                    for (i, block) in blocks.iter().enumerate() {
                        let i = i as u64;
                        all.extend(t.feed(&sse(
                            "content_block_start",
                            json!({"type": "content_block_start", "index": i, "content_block": block}),
                        )));
                        if block["type"] == "tool_use" && with_deltas {
                            let frag = if partial { "{\"a\":" } else { "{\"a\":2}" };
                            all.extend(t.feed(&sse(
                                "content_block_delta",
                                json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": frag}}),
                            )));
                        }
                        if !cut {
                            all.extend(t.feed(&sse("content_block_stop", json!({"type": "content_block_stop", "index": i}))));
                        }
                    }
                    if cut {
                        all.extend(t.finish());
                    } else {
                        all.extend(t.feed(&sse(
                            "message_delta",
                            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}}),
                        )));
                    }
                    crate::translate::assert_tool_arguments_always_valid(
                        &all,
                        &format!("{label} (deltas={with_deltas}, partial={partial}, cut={cut})"),
                    );
                }
            }
        }
    }

    #[test]
    fn fixture_emits_only_valid_arguments() {
        let upstream = include_bytes!("testdata/anthropic_stream.sse");
        let mut t = ChatSseTranslator::new(true, Some("claude-opus-4-6"));
        let mut all = Vec::new();
        for byte in upstream.iter() {
            all.extend(t.feed(&[*byte]));
        }
        all.extend(t.finish());
        crate::translate::assert_tool_arguments_always_valid(&all, "recorded fixture, byte by byte");
    }

    /// The reported failure's real shape: the stream dies mid-argument, and
    /// buffering (rather than forwarding fragments live) is what keeps the
    /// client from ever seeing the unparseable partial value.
    #[test]
    fn stream_cut_mid_argument_falls_back_to_the_start_event_input() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse("content_block_start", json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {"command": ["pwsh", "ls"]}}
        }))));
        all.extend(t.feed(&sse("content_block_delta", json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"command\":[\"pwsh\","}
        }))));
        all.extend(t.finish());

        let assembled = assembled_arguments(&all, 0);
        let parsed: Value = serde_json::from_str(&assembled).expect("must never emit an unparseable fragment");
        assert_eq!(parsed["command"][1], "ls", "falls back to the start event's input");
    }

    #[test]
    fn stream_cut_mid_argument_with_no_start_input_falls_back_to_empty_object() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse("content_block_start", json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}}
        }))));
        all.extend(t.feed(&sse("content_block_delta", json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}
        }))));
        all.extend(t.finish());

        assert_eq!(assembled_arguments(&all, 0), "{}");
    }

    /// Same gap as the Responses translator: an upstream that puts the whole
    /// tool input on `content_block_start` and streams no `input_json_delta`
    /// must not lose that input.
    #[test]
    fn tool_input_on_content_block_start_is_used_when_no_deltas_stream() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {}}}))));
        all.extend(t.feed(&sse("content_block_start", json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {"command": ["ls"]}}
        }))));
        all.extend(t.feed(&sse("content_block_stop", json!({"type": "content_block_stop", "index": 0}))));
        all.extend(t.feed(&sse("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}}))));

        let assembled = assembled_arguments(&all, 0);
        let parsed: Value = serde_json::from_str(&assembled).expect("must be valid JSON");
        assert_eq!(parsed["command"][0], "ls");
    }

    #[test]
    fn chat_streamed_deltas_win_over_the_start_event_input() {
        let mut t = ChatSseTranslator::new(false, None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse("message_start", json!({"type": "message_start", "message": {"model": "m", "usage": {}}}))));
        all.extend(t.feed(&sse("content_block_start", json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {"stale": true}}
        }))));
        all.extend(t.feed(&sse("content_block_delta", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"fresh\":true}"}}))));
        all.extend(t.feed(&sse("content_block_stop", json!({"type": "content_block_stop", "index": 0}))));

        let assembled = assembled_arguments(&all, 0);
        assert_eq!(assembled, "{\"fresh\":true}");
    }

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
