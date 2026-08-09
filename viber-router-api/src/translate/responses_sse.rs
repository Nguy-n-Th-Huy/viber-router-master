//! Anthropic Messages SSE stream -> OpenAI Responses API named-event stream.
//!
//! Same incremental `feed`/`finish` shape as `chat_sse.rs`, but the Responses
//! protocol names each event type (`response.output_text.delta`, ...) instead
//! of using one generic chunk shape, carries a running `sequence_number`, and
//! has no `[DONE]` sentinel — the stream simply ends after `response.completed`
//! or `response.failed`.

use serde_json::{Value, json};
use uuid::Uuid;

/// The Responses output item currently being streamed.
///
/// Responses is item-oriented where Anthropic is block-oriented: each Anthropic
/// content block becomes one `output[]` item, announced with
/// `response.output_item.added` before anything can refer to it and closed with
/// `response.output_item.done`. Only one is ever open at a time, because
/// Anthropic streams its blocks one after another.
/// **Invariant: one item, one content part.** Every `message` item this
/// translator produces holds exactly one `output_text` part, so the
/// `content_index` on every emitted event is always 0. Two Anthropic text
/// blocks become two items at `output_index` 0 and 1 — never one item with
/// parts 0 and 1. Anything that starts merging blocks into a single item has to
/// make `content_index` a tracked value instead of the literal it is now;
/// `two_text_blocks_are_two_items_each_with_one_part` fails if that slips.
enum OpenItem {
    Message {
        /// The item's own id. Deltas must carry *this*, not the response id —
        /// a client looks the item up by it, and a response id finds nothing.
        item_id: String,
        output_index: usize,
        content_index: u64,
        text: String,
    },
    FunctionCall {
        item_id: String,
        output_index: usize,
        content_index: u64,
        /// Anthropic's `toolu_...` id, which is what the client echoes back as
        /// `call_id` when it returns the tool result. Distinct from `item_id`.
        call_id: String,
        name: String,
        arguments: String,
    },
}

impl OpenItem {
    fn content_index(&self) -> u64 {
        match self {
            Self::Message { content_index, .. } | Self::FunctionCall { content_index, .. } => {
                *content_index
            }
        }
    }
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
    /// The item currently being streamed, if any. Anthropic streams one
    /// content block at a time, so at most one item is ever open.
    open: Option<OpenItem>,
    /// Items already closed with `response.output_item.done`, in order.
    closed: Vec<Value>,
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
            open: None,
            closed: Vec::new(),
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

    /// Flush any buffered partial event, and synthesize `response.failed` if
    /// the connection dropped without a proper terminal event.
    ///
    /// Unlike Chat Completions, there is no `[DONE]` sentinel here — a client
    /// normally learns the stream is over from `response.completed` or
    /// `response.failed` alone. If the upstream connection drops before
    /// either arrives, the client is left waiting forever with no signal at
    /// all, so this synthesizes the failure rather than staying silent.
    pub fn finish(mut self) -> Vec<u8> {
        let mut out = self.drain_events();
        if self.started && !self.ended {
            self.on_error(
                &json!({"error": {"type": "api_error", "message": "upstream connection closed unexpectedly"}}),
                &mut out,
            );
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
        if self.ended {
            return;
        }
        let event_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match event_type {
            "message_start" => self.on_message_start(json, out),
            "content_block_start" => self.on_content_block_start(json, out),
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

    /// Announce a new output item from an explicit `content_block_start`.
    ///
    /// Only the two block types that have a Responses equivalent open an item:
    /// `text` becomes a `message` (with a content part), `tool_use` becomes a
    /// `function_call` (without one — its arguments stream through
    /// `response.function_call_arguments.*` instead).
    ///
    /// Every other block type is ignored outright, which matters for
    /// `thinking` and `redacted_thinking`: opening a `message` for them would
    /// emit an empty assistant turn a client renders as a blank bubble, and
    /// would push the real answer's `output_index` up by one. The same applies
    /// to `server_tool_use` and to any block type Anthropic adds later — an
    /// unrecognised block contributes nothing rather than a wrong something.
    ///
    /// Anthropic's reasoning text is therefore not surfaced on `/v1/responses`
    /// at all. Its `thinking_delta` is not translated either, so nothing is
    /// half-emitted; the visible answer and the usage numbers (which include
    /// thinking tokens) are unaffected.
    fn on_content_block_start(&mut self, json: &Value, out: &mut Vec<u8>) {
        let block = json.get("content_block").cloned().unwrap_or_default();
        let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);

        // Anthropic always closes a block before starting the next, so this is
        // belt-and-braces: without it, a missing `content_block_stop` would
        // silently drop the open item instead of closing it.
        if let Some(open) = self.open.take() {
            self.close_item(open, out);
        }

        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => self.open_message(content_index, out),
            Some("tool_use") => {
                let call_id = block.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                self.open_function_call(content_index, call_id, name, out);
            }
            _ => {}
        }
    }

    /// Open a `message` item: `response.output_item.added` then
    /// `response.content_part.added` for its (only) text part.
    fn open_message(&mut self, content_index: u64, out: &mut Vec<u8>) {
        let item_id = format!("msg_{}", Uuid::new_v4().simple());
        let output_index = self.closed.len();
        self.push_event(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "type": "message", "id": item_id, "role": "assistant",
                    "content": [], "status": "in_progress"
                }
            }),
            out,
        );
        self.push_event(
            "response.content_part.added",
            json!({
                "item_id": item_id, "output_index": output_index, "content_index": 0,
                "part": {"type": "output_text", "text": ""}
            }),
            out,
        );
        self.open = Some(OpenItem::Message { item_id, output_index, content_index, text: String::new() });
    }

    /// Open a `function_call` item: just `response.output_item.added`.
    fn open_function_call(&mut self, content_index: u64, call_id: String, name: String, out: &mut Vec<u8>) {
        let item_id = format!("fc_{}", Uuid::new_v4().simple());
        let output_index = self.closed.len();
        self.push_event(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "type": "function_call", "id": item_id, "call_id": call_id,
                    "name": name, "arguments": "", "status": "in_progress"
                }
            }),
            out,
        );
        self.open = Some(OpenItem::FunctionCall {
            item_id,
            output_index,
            content_index,
            call_id,
            name,
            arguments: String::new(),
        });
    }

    fn on_content_block_delta(&mut self, json: &Value, out: &mut Vec<u8>) {
        let delta = json.get("delta").cloned().unwrap_or_default();
        let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        match delta.get("type").and_then(|t| t.as_str()) {
            Some("text_delta") => {
                let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                // Tolerate a delta with no preceding content_block_start by
                // opening the message it implies, rather than emitting a
                // delta against an item_id the client was never told about.
                if self.open.is_none() {
                    self.open_message(content_index, out);
                }
                let Some(OpenItem::Message { item_id, output_index, text: acc, .. }) = &mut self.open
                else {
                    return;
                };
                acc.push_str(&text);
                // Copy out before pushing: push_event needs &mut self too.
                let (item_id, output_index) = (item_id.clone(), *output_index);
                self.push_event(
                    "response.output_text.delta",
                    json!({
                        "delta": text, "item_id": item_id, "output_index": output_index,
                        "content_index": 0
                    }),
                    out,
                );
            }
            Some("input_json_delta") => {
                let partial = delta.get("partial_json").and_then(|p| p.as_str()).unwrap_or("");
                let Some(OpenItem::FunctionCall {
                    item_id,
                    output_index,
                    content_index: open_index,
                    arguments,
                    ..
                }) = &mut self.open
                else {
                    return;
                };
                if *open_index != content_index {
                    return;
                }
                arguments.push_str(partial);
                let (item_id, output_index) = (item_id.clone(), *output_index);
                self.push_event(
                    "response.function_call_arguments.delta",
                    json!({"item_id": item_id, "output_index": output_index, "delta": partial}),
                    out,
                );
            }
            _ => {}
        }
    }

    /// Close the currently open item: the matching `.done` event(s), then
    /// `response.output_item.done`, then move it into `closed`.
    fn on_content_block_stop(&mut self, json: &Value, out: &mut Vec<u8>) {
        let content_index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        if self.open.as_ref().map(|o| o.content_index()) != Some(content_index) {
            return;
        }
        let Some(open) = self.open.take() else { return };
        self.close_item(open, out);
    }

    fn close_item(&mut self, open: OpenItem, out: &mut Vec<u8>) {
        match open {
            OpenItem::Message { item_id, output_index, text, .. } => {
                self.push_event(
                    "response.output_text.done",
                    json!({"item_id": item_id, "output_index": output_index, "content_index": 0, "text": text}),
                    out,
                );
                self.push_event(
                    "response.content_part.done",
                    json!({
                        "item_id": item_id, "output_index": output_index, "content_index": 0,
                        "part": {"type": "output_text", "text": text}
                    }),
                    out,
                );
                let item = json!({
                    "type": "message", "id": item_id, "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": text}]
                });
                self.push_event(
                    "response.output_item.done",
                    json!({"output_index": output_index, "item": item.clone()}),
                    out,
                );
                self.closed.push(item);
            }
            OpenItem::FunctionCall { item_id, output_index, call_id, name, arguments, .. } => {
                // A parameterless tool streams no input_json_delta, leaving this
                // empty. `arguments` is a JSON string by contract, and a client
                // that stores `""` and echoes it back gets a 400 from the
                // inbound seam, so report the empty object it actually means.
                let arguments = if arguments.trim().is_empty() {
                    "{}".to_string()
                } else {
                    arguments
                };
                self.push_event(
                    "response.function_call_arguments.done",
                    json!({"item_id": item_id, "output_index": output_index, "arguments": arguments}),
                    out,
                );
                let item = json!({
                    "type": "function_call", "id": item_id, "call_id": call_id, "name": name,
                    "arguments": arguments, "status": "completed"
                });
                self.push_event(
                    "response.output_item.done",
                    json!({"output_index": output_index, "item": item.clone()}),
                    out,
                );
                self.closed.push(item);
            }
        }
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

    /// Close whatever item is still open, without emitting its `.done` events.
    ///
    /// Used only when the stream is ending abnormally (an error, or the
    /// connection dropping): the client gets the item's partial content in
    /// the terminal response's `output[]`, but no `output_item.done` for an
    /// item that never actually finished streaming.
    fn take_open_item_unfinished(&mut self) -> Option<Value> {
        match self.open.take()? {
            OpenItem::Message { item_id, text, .. } => Some(json!({
                "type": "message", "id": item_id, "role": "assistant", "status": "incomplete",
                "content": [{"type": "output_text", "text": text}]
            })),
            OpenItem::FunctionCall { item_id, call_id, name, arguments, .. } => {
                let arguments = if arguments.trim().is_empty() { "{}".to_string() } else { arguments };
                Some(json!({
                    "type": "function_call", "id": item_id, "call_id": call_id, "name": name,
                    "arguments": arguments, "status": "incomplete"
                }))
            }
        }
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

        // Anthropic emits content_block_stop for every block before
        // message_delta, so nothing should still be open here; closing
        // defensively avoids losing a block if it ever doesn't.
        if let Some(open) = self.open.take() {
            self.close_item(open, out);
        }
        let usage = self.final_usage(output_tokens);
        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "model": self.model,
            "status": status,
            "output": std::mem::take(&mut self.closed),
            "usage": usage
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
        if let Some(unfinished) = self.take_open_item_unfinished() {
            self.closed.push(unfinished);
        }
        let response = json!({
            "id": self.response_id,
            "object": "response",
            "model": self.model,
            "status": "failed",
            "output": std::mem::take(&mut self.closed),
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
        // The full documented sequence: an item and its content part must be
        // announced before any delta can refer to them, and both are closed
        // before the response completes.
        assert_eq!(
            events,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
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

    /// The bug this sequence exists to prevent: a delta that names an id the
    /// client was never told about is unresolvable, and the client rejects the
    /// stream with "text part <id> not found".
    #[test]
    fn text_deltas_carry_the_announced_item_id_not_the_response_id() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}))));

        let events = parsed_events(&all);
        let response_id = events[0]["response"]["id"].as_str().unwrap().to_string();
        let added = events.iter().find(|e| e["type"] == "response.output_item.added").unwrap();
        let announced_id = added["item"]["id"].as_str().unwrap().to_string();

        assert!(announced_id.starts_with("msg_"), "item id was {announced_id}");
        assert_ne!(announced_id, response_id, "item id must not be the response id");

        // Every event that names an item must name that same announced id.
        for kind in [
            "response.content_part.added",
            "response.output_text.delta",
        ] {
            let e = events.iter().find(|e| e["type"] == kind).unwrap();
            assert_eq!(e["item_id"], announced_id.as_str(), "{kind} carried the wrong item_id");
            assert_eq!(e["content_index"], 0, "{kind} is missing content_index");
        }
    }

    /// Anthropic sends `thinking`, `redacted_thinking`, and `server_tool_use`
    /// blocks that have no Responses equivalent here. They must produce no
    /// output item at all — not an empty `message`, which a client would render
    /// as a blank assistant turn and which would shift the real answer's
    /// `output_index`.
    #[test]
    fn thinking_blocks_produce_no_output_item() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        // Anthropic block 0: thinking, with its own delta type.
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "step one"}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 0}))));
        // Anthropic block 1: the visible answer.
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "answer"}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 1}))));
        all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}))));

        let events = parsed_events(&all);
        let added: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "response.output_item.added")
            .collect();
        assert_eq!(added.len(), 1, "thinking must not announce an item");
        assert_eq!(added[0]["output_index"], 0, "the answer must still be item 0");

        let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
        let output = completed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 1, "no empty message item may be emitted");
        assert_eq!(output[0]["content"][0]["text"], "answer");
        // The reasoning text is deliberately not surfaced anywhere.
        assert!(
            !String::from_utf8(all.clone()).unwrap().contains("step one"),
            "reasoning text must not be emitted as output text"
        );
    }

    #[test]
    fn redacted_thinking_and_server_tool_use_also_produce_no_item() {
        for block_type in ["redacted_thinking", "server_tool_use", "some_future_block"] {
            let mut t = ResponsesSseTranslator::new(None);
            let mut all = Vec::new();
            all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
            all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 0, "content_block": {"type": block_type}}))));
            all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 0}))));
            all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {}}))));

            let events = parsed_events(&all);
            assert!(
                !events.iter().any(|e| e["type"] == "response.output_item.added"),
                "{block_type} must not announce an item"
            );
            let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
            assert_eq!(
                completed["response"]["output"].as_array().unwrap().len(),
                0,
                "{block_type} must not appear in output"
            );
        }
    }

    /// Pins the invariant documented on `OpenItem`: two separate Anthropic text
    /// blocks are two separate items (`output_index` 0 and 1), each with its
    /// single part at `content_index` 0 — not one item with two content parts.
    /// `content_index` is a hardcoded 0 in the implementation *because* this
    /// holds; if blocks ever get merged into one item, this test is what breaks.
    /// A tool that takes no parameters produces a `tool_use` block with no
    /// `input_json_delta` at all, so the accumulated argument string is empty.
    /// It must be reported as `"{}"`, never `""`: `arguments` is a JSON string
    /// by contract, and a client that echoes `""` back on the next turn gets a
    /// 400 from the inbound seam ("unparseable arguments: EOF while parsing").
    #[test]
    fn tool_call_with_no_argument_deltas_reports_empty_object() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}}
        }))));
        // No input_json_delta: the tool takes no parameters.
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 0}))));
        all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {}}))));

        let events = parsed_events(&all);
        let done = events.iter().find(|e| e["type"] == "response.function_call_arguments.done").unwrap();
        assert_eq!(done["arguments"], "{}", "arguments must be valid JSON");

        let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
        let args = completed["response"]["output"][0]["arguments"].as_str().unwrap();
        assert_eq!(args, "{}");
        // The value a client echoes back must survive a round trip.
        serde_json::from_str::<Value>(args).expect("arguments must parse as JSON");
    }

    #[test]
    fn two_text_blocks_are_two_items_each_with_one_part() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "one"}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 0}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "two"}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 1}))));
        all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}))));

        let events = parsed_events(&all);
        let deltas: Vec<&Value> = events.iter().filter(|e| e["type"] == "response.output_text.delta").collect();
        assert_eq!(deltas[0]["output_index"], 0);
        assert_eq!(deltas[0]["content_index"], 0);
        assert_eq!(deltas[1]["output_index"], 1);
        assert_eq!(deltas[1]["content_index"], 0, "second item's part is still its own part 0, not part 1");

        let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
        let output = completed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2, "two blocks must be two items, not one item with two parts");
        assert_eq!(output[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(output[1]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn output_index_increments_across_text_then_tool_then_text() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        // Anthropic block 0: text
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "a"}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 0}))));
        // Anthropic block 1: tool_use
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "f"}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{}"}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 1}))));
        // Anthropic block 2: text again
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "b"}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_stop", "index": 2}))));
        all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}))));

        let events = parsed_events(&all);
        let added: Vec<(u64, String)> = events
            .iter()
            .filter(|e| e["type"] == "response.output_item.added")
            .map(|e| {
                (
                    e["output_index"].as_u64().unwrap(),
                    e["item"]["type"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            added,
            vec![
                (0, "message".to_string()),
                (1, "function_call".to_string()),
                (2, "message".to_string())
            ]
        );

        // A function_call item never announces a content part.
        let fc_id = events
            .iter()
            .find(|e| e["type"] == "response.output_item.added" && e["item"]["type"] == "function_call")
            .unwrap()["item"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            !events
                .iter()
                .any(|e| e["type"].as_str().unwrap().starts_with("response.content_part")
                    && e["item_id"] == fc_id.as_str()),
            "function_call must not get content_part events"
        );

        let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
        let output = completed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["content"][0]["text"], "a");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[2]["content"][0]["text"], "b");
    }

    #[test]
    fn dropped_upstream_stream_still_gets_a_terminal_event() {
        // No message_delta, no error — the connection just goes away. Without a
        // synthesized terminal the client waits forever, since Responses has
        // no [DONE] sentinel.
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))));
        all.extend(t.feed(&sse(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}))));
        all.extend(t.finish());

        let events = parsed_events(&all);
        let last = events.last().unwrap();
        assert_eq!(last["type"], "response.failed");
        assert_eq!(last["response"]["status"], "failed");
        // The partial text the client already received is still accounted for.
        assert_eq!(last["response"]["output"][0]["content"][0]["text"], "partial");
    }

    #[test]
    fn a_completed_stream_gets_no_synthesized_terminal_on_finish() {
        let mut t = ResponsesSseTranslator::new(None);
        let mut all = Vec::new();
        all.extend(t.feed(&sse(json!({"type": "message_start", "message": {"usage": {}}}))));
        all.extend(t.feed(&sse(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}))));
        all.extend(t.finish());

        let has_failed = parsed_events(&all).iter().any(|e| e["type"] == "response.failed");
        assert!(!has_failed, "must not synthesize response.failed after a clean completion");
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
        let added = events.iter().find(|e| e["type"] == "response.output_item.added").unwrap();
        let item_id = added["item"]["id"].as_str().unwrap().to_string();
        assert!(item_id.starts_with("fc_"));
        assert_eq!(added["item"]["call_id"], "toolu_1");

        // Deltas are keyed by item_id, not call_id — call_id only reappears in
        // the final output item, for the client to correlate the tool result.
        let delta = events.iter().find(|e| e["type"] == "response.function_call_arguments.delta").unwrap();
        assert_eq!(delta["item_id"], item_id.as_str());
        assert_eq!(delta["delta"], "{\"city\":\"Hanoi\"}");

        let done = events.iter().find(|e| e["type"] == "response.function_call_arguments.done").unwrap();
        assert_eq!(done["item_id"], item_id.as_str());
        assert_eq!(done["arguments"], "{\"city\":\"Hanoi\"}");

        let item_done = events.iter().find(|e| e["type"] == "response.output_item.done").unwrap();
        assert_eq!(item_done["item"]["status"], "completed");

        let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
        assert_eq!(
            completed["response"]["output"][0],
            json!({
                "type": "function_call", "id": item_id, "call_id": "toolu_1",
                "name": "get_weather", "arguments": "{\"city\":\"Hanoi\"}", "status": "completed"
            })
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
        let normalised = normalise_ids(&actual);

        let expected = include_str!("testdata/responses_stream.golden");
        assert_eq!(normalised, expected);
    }

    /// Replace every generated id with a stable placeholder so the baseline is
    /// reproducible.
    ///
    /// `resp_` must be one id throughout, so it becomes `resp_GOLDEN` and any
    /// second value is a bug. Item ids (`msg_`, `fc_`) are legitimately
    /// distinct per item, so each new one gets the next number in order of
    /// first appearance — which also pins that a delta reuses the id its
    /// `output_item.added` announced rather than inventing a fresh one.
    fn normalise_ids(stream: &str) -> String {
        let mut response_id: Option<String> = None;
        let mut items: Vec<String> = Vec::new();
        let mut out = String::with_capacity(stream.len());
        let mut rest = stream;

        while let Some(offset) = next_id_start(rest) {
            let id_start = offset + 1; // past the opening quote
            let id_end = id_start + rest[id_start..].find('"').expect("id is quoted");
            let id = &rest[id_start..id_end];

            let placeholder = if id.starts_with("resp_") {
                match &response_id {
                    None => response_id = Some(id.to_string()),
                    Some(first) => assert_eq!(first, id, "all events must share one response id"),
                }
                "resp_GOLDEN".to_string()
            } else {
                let prefix = if id.starts_with("msg_") { "msg" } else { "fc" };
                let n = match items.iter().position(|seen| seen == id) {
                    Some(n) => n,
                    None => {
                        items.push(id.to_string());
                        items.len() - 1
                    }
                };
                format!("{prefix}_ITEM{n}")
            };

            out.push_str(&rest[..id_start]);
            out.push_str(&placeholder);
            rest = &rest[id_end..];
        }
        out.push_str(rest);
        assert!(response_id.is_some(), "stream contained no response id");
        out
    }

    /// Offset of the opening quote of the next generated id, if any.
    fn next_id_start(s: &str) -> Option<usize> {
        ["\"resp_", "\"msg_", "\"fc_"]
            .iter()
            .filter_map(|p| s.find(p))
            .min()
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
