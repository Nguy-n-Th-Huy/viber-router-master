//! Anthropic Messages SSE stream -> OpenAI Responses API named-event stream.
//!
//! Same incremental `feed`/`finish` shape as `chat_sse.rs`, but the Responses
//! protocol names each event type (`response.output_text.delta`, ...) instead
//! of using one generic chunk shape, carries a running `sequence_number`, and
//! has no `[DONE]` sentinel — the stream simply ends after `response.completed`
//! or `response.failed`.

use serde_json::{Value, json};
use uuid::Uuid;

/// One in-flight Anthropic `tool_use` content block being streamed.
struct ToolCallState {
    content_index: u64,
    call_id: String,
    name: String,
    arguments: String,
}

/// Translates one Anthropic Messages SSE stream into Responses API events.
pub struct ResponsesSseTranslator {
    buffer: Vec<u8>,
    response_id: String,
    /// The model reported on every event: the client's requested name when one
    /// was supplied, otherwise the upstream's own
    /// `message_start.message.model`.
    model: String,
    /// When true, `model` is the client's requested name and `message_start`
    /// must not overwrite it with the upstream's (possibly mapped) name.
    client_model_pinned: bool,
    sequence_number: u64,
    tool_calls: Vec<ToolCallState>,
    text_parts: Vec<String>,
    input_tokens: i64,
    cache_read_tokens: Option<i64>,
    started: bool,
    ended: bool,
}

impl ResponsesSseTranslator {
    /// `client_model`, when set, is reported as the `model` on every event in
    /// place of the upstream's own name.
    pub fn new(client_model: Option<&str>) -> Self {
        Self {
            buffer: Vec::new(),
            response_id: format!("resp_{}", Uuid::new_v4().simple()),
            model: client_model.unwrap_or_default().to_string(),
            client_model_pinned: client_model.is_some(),
            sequence_number: 0,
            tool_calls: Vec::new(),
            text_parts: Vec::new(),
            input_tokens: 0,
            cache_read_tokens: None,
            started: false,
            ended: false,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(chunk);
        self.drain_events()
    }

    /// Flush any buffered partial event.
    ///
    /// Unlike Chat Completions, there is no terminator to emit here: a stream
    /// that ends without `message_stop` has already emitted whatever
    /// `response.completed`/`response.failed` it was going to emit (or neither,
    /// if the upstream connection simply dropped) and closing the SSE body is
    /// the client's only signal either way.
    pub fn finish(mut self) -> Vec<u8> {
        self.drain_events()
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
        if self.ended {
            return;
        }
        let event_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match event_type {
            "message_start" => self.on_message_start(json, out),
            "content_block_start" => self.on_content_block_start(json),
            "content_block_delta" => self.on_content_block_delta(json, out),
            "content_block_stop" => self.on_content_block_stop(json, out),
            "message_delta" => self.on_message_delta(json, out),
            "error" => self.on_error(json, out),
            _ => {}
        }
    }

    fn next_seq(&mut self) -> u64 {
        let n = self.sequence_number;
        self.sequence_number += 1;
        n
    }

    fn push_event(&mut self, event_type: &str, mut payload: Value, out: &mut Vec<u8>) {
        payload["type"] = json!(event_type);
        payload["sequence_number"] = json!(self.next_seq());
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(serde_json::to_string(&payload).unwrap_or_default().as_bytes());
        out.extend_from_slice(b"\n\n");
    }

    fn in_progress_response(&self) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "model": self.model,
            "status": "in_progress",
            "output": []
        })
    }

    fn on_message_start(&mut self, json: &Value, out: &mut Vec<u8>) {
        let message = json.get("message");
        if !self.client_model_pinned
            && let Some(model) = message.and_then(|m| m.get("model")).and_then(|m| m.as_str())
        {
            self.model = model.to_string();
        }
        if let Some(usage) = message.and_then(|m| m.get("usage")) {
            self.input_tokens = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            self.cache_read_tokens = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_i64());
        }
        if self.started {
            return;
        }
        self.started = true;
        self.push_event("response.created", json!({"response": self.in_progress_response()}), out);
        self.push_event("response.in_progress", json!({"response": self.in_progress_response()}), out);
    }

    fn on_content_block_start(&mut self, json: &Value) {
        let block = json.get("content_block").cloned().unwrap_or_default();
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            return;
        }
        let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        self.tool_calls.push(ToolCallState {
            content_index,
            call_id: block.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string(),
            name: block.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            arguments: String::new(),
        });
    }

    fn on_content_block_delta(&mut self, json: &Value, out: &mut Vec<u8>) {
        let delta = json.get("delta").cloned().unwrap_or_default();
        let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        match delta.get("type").and_then(|t| t.as_str()) {
            Some("text_delta") => {
                let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                self.text_parts.push(text.to_string());
                self.push_event(
                    "response.output_text.delta",
                    json!({"delta": text, "output_index": 0, "item_id": self.response_id}),
                    out,
                );
            }
            Some("input_json_delta") => {
                let partial = delta.get("partial_json").and_then(|p| p.as_str()).unwrap_or("");
                let Some(call) = self
                    .tool_calls
                    .iter_mut()
                    .rev()
                    .find(|c| c.content_index == content_index)
                else {
                    return;
                };
                call.arguments.push_str(partial);
                let call_id = call.call_id.clone();
                self.push_event(
                    "response.function_call_arguments.delta",
                    json!({"call_id": call_id, "delta": partial}),
                    out,
                );
            }
            _ => {}
        }
    }

    fn on_content_block_stop(&mut self, json: &Value, out: &mut Vec<u8>) {
        let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        let Some(call) = self.tool_calls.iter().find(|c| c.content_index == content_index) else {
            return;
        };
        self.push_event(
            "response.function_call_arguments.done",
            json!({"call_id": call.call_id, "arguments": call.arguments}),
            out,
        );
    }

    fn final_output(&self) -> Vec<Value> {
        let mut output = Vec::new();
        if !self.text_parts.is_empty() {
            output.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": self.text_parts.join("")}]
            }));
        }
        for call in &self.tool_calls {
            output.push(json!({
                "type": "function_call",
                "call_id": call.call_id,
                "name": call.name,
                "arguments": call.arguments
            }));
        }
        output
    }

    fn final_usage(&self, output_tokens: i64) -> Value {
        let mut usage = json!({
            "input_tokens": self.input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": self.input_tokens + output_tokens
        });
        if let Some(cached) = self.cache_read_tokens {
            usage["input_tokens_details"] = json!({"cached_tokens": cached});
        }
        usage
    }

    fn on_message_delta(&mut self, json: &Value, out: &mut Vec<u8>) {
        let stop_reason = json.pointer("/delta/stop_reason").and_then(|s| s.as_str());
        let output_tokens = json
            .pointer("/usage/output_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let (status, incomplete_details) = if stop_reason == Some("max_tokens") {
            ("incomplete", Some(json!({"reason": "max_output_tokens"})))
        } else {
            ("completed", None)
        };

        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "model": self.model,
            "status": status,
            "output": self.final_output(),
            "usage": self.final_usage(output_tokens)
        });
        if let Some(details) = incomplete_details {
            response["incomplete_details"] = details;
        }
        self.ended = true;
        self.push_event("response.completed", json!({"response": response}), out);
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
        let response = json!({
            "id": self.response_id,
            "object": "response",
            "model": self.model,
            "status": "failed",
            "output": self.final_output(),
            "error": {"type": error_type, "message": message}
        });
        self.ended = true;
        self.push_event("response.failed", json!({"response": response}), out);
    }
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(data: Value) -> Vec<u8> {
        format!("event: {}\ndata: {data}\n\n", data["type"].as_str().unwrap()).into_bytes()
    }

    fn parsed_events(bytes: &[u8]) -> Vec<Value> {
        std::str::from_utf8(bytes)
            .unwrap()
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .map(|d| serde_json::from_str(d).unwrap())
            .collect()
    }

    #[test]
    fn plain_text_stream_emits_expected_event_sequence() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))));
        for word in ["Hel", "lo"] {
            all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": word}}))));
        }
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 0}))));
        all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}))));
        all.extend(t.finish());

        let events: Vec<String> = parsed_events(&all)
            .iter()
            .map(|e| e["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            events,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.completed"
            ]
        );

        let deltas: String = parsed_events(&all)
            .iter()
            .filter(|e| e["type"] == "response.output_text.delta")
            .map(|e| e["delta"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(deltas, "Hello");

        let completed = parsed_events(&all).into_iter().last().unwrap();
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "Hello"
        );
    }

    #[test]
    fn streaming_function_call_emits_delta_then_done_then_completed_output() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather"}
        }))));
        all.extend(t.feed(&sse(json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"city\":\"Hanoi\"}"}
        }))));
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 0}))));
        all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {}}))));

        let events = parsed_events(&all);
        let delta = events.iter().find(|e| e["type"] == "response.function_call_arguments.delta").unwrap();
        assert_eq!(delta["call_id"], "toolu_1");
        assert_eq!(delta["delta"], "{\"city\":\"Hanoi\"}");

        let done = events.iter().find(|e| e["type"] == "response.function_call_arguments.done").unwrap();
        assert_eq!(done["call_id"], "toolu_1");
        assert_eq!(done["arguments"], "{\"city\":\"Hanoi\"}");

        let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
        assert_eq!(
            completed["response"]["output"][0],
            json!({"type": "function_call", "call_id": "toolu_1", "name": "get_weather", "arguments": "{\"city\":\"Hanoi\"}"})
        );
    }

    #[test]
    fn sequence_number_is_contiguous_from_zero() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}))));
        all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}))));

        let seqs: Vec<u64> = parsed_events(&all)
            .iter()
            .map(|e| e["sequence_number"].as_u64().unwrap())
            .collect();
        let expected: Vec<u64> = (0..seqs.len() as u64).collect();
        assert_eq!(seqs, expected);
    }

    #[test]
    fn mid_stream_error_emits_failed_and_no_completed() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}))));
        all.extend(t.feed(&sse(json!({"type": "error", "error": {"type": "overloaded_error", "message": "overloaded"}}))));

        let events = parsed_events(&all);
        assert!(events.iter().any(|e| e["type"] == "response.output_text.delta"));
        assert!(!events.iter().any(|e| e["type"] == "response.completed"));
        let failed = events.last().unwrap();
        assert_eq!(failed["type"], "response.failed");
        assert_eq!(failed["response"]["status"], "failed");
        assert_eq!(failed["response"]["error"]["message"], "overloaded");
    }

    /// Golden master over the same captured Anthropic stream `chat_sse` uses,
    /// fed one byte at a time so every event boundary is crossed mid-chunk.
    #[test]
    fn golden_master_full_stream_byte_by_byte() {
        let fixture = include_str!("testdata/anthropic_stream.sse");

        let mut t = ResponsesSseTranslator::new(None);
        let mut actual = Vec::new();
        for byte in fixture.as_bytes() {
            actual.extend(t.feed(&[*byte]));
        }
        actual.extend(t.finish());

        let actual = String::from_utf8(actual).expect("translated stream is utf-8");
        let normalised = normalise_response_id(&actual);

        let expected = include_str!("testdata/responses_stream.golden");
        assert_eq!(normalised, expected);
    }

    /// Replace the generated `resp_<uuid>` with a fixed placeholder, asserting
    /// every event carried the same id.
    fn normalise_response_id(stream: &str) -> String {
        let mut seen: Option<String> = None;
        let mut out = String::with_capacity(stream.len());
        let mut rest = stream;
        while let Some(start) = rest.find("\"resp_") {
            let id_start = start + 1;
            let id_end = id_start + rest[id_start..].find('"').expect("response id is quoted");
            let id = &rest[id_start..id_end];
            match &seen {
                None => seen = Some(id.to_string()),
                Some(first) => assert_eq!(first, id, "all events must share one response id"),
            }
            out.push_str(&rest[..id_start]);
            out.push_str("resp_GOLDEN");
            rest = &rest[id_end..];
        }
        out.push_str(rest);
        assert!(seen.is_some(), "stream contained no response id");
        out
    }

    #[test]
    fn client_model_is_echoed_on_every_event_over_the_upstream_name() {
        let mut t = ResponsesSseTranslator::new(Some("gpt-4o"));
        let all = t.feed(&sse(json!({
            "type": "message_start",
            "message": {"model": "claude-sonnet-4-6", "usage": {}}
        })));
        for event in parsed_events(&all) {
            assert_eq!(event["response"]["model"], "gpt-4o");
        }
    }

    #[test]
    fn upstream_model_is_used_when_client_model_is_absent() {
        let mut t = ResponsesSseTranslator::new(None);
        let all = t.feed(&sse(json!({
            "type": "message_start",
            "message": {"model": "claude-sonnet-4-6", "usage": {}}
        })));
        let events = parsed_events(&all);
        assert_eq!(events[0]["response"]["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn incomplete_status_on_max_tokens_stop_reason() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 1}}))));

        let completed = parsed_events(&all).into_iter().last().unwrap();
        assert_eq!(completed["response"]["status"], "incomplete");
        assert_eq!(completed["response"]["incomplete_details"]["reason"], "max_output_tokens");
    }
}
