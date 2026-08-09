//! OpenAI Responses API <-> Anthropic Messages translation.

use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    TranslateError, apply_common_sampling, resolve_max_tokens, tool_arguments_from_input,
    tool_input_from_arguments,
};

/// Anthropic's floor for `thinking.budget_tokens`. A budget below this is
/// rejected upstream, so a request that resolves lower gets no thinking block
/// rather than a 400.
const MIN_THINKING_BUDGET: i64 = 1024;

/// Map a Responses `reasoning.effort` to an Anthropic thinking budget.
///
/// `None` means the effort value is not one we recognise; the caller then omits
/// `thinking` entirely rather than guessing a budget, which would silently
/// suppress `temperature`/`top_p`.
fn effort_to_budget(effort: &str) -> Option<i64> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" | "low" => Some(2048),
        "medium" => Some(8192),
        "high" => Some(16384),
        "xhigh" | "max" => Some(24576),
        _ => None,
    }
}

/// Translate a Responses request into an Anthropic Messages request.
pub fn request_to_anthropic(src: &Value) -> Result<Value, TranslateError> {
    reject_unsupported(src)?;

    let mut out = Map::new();
    let model = src
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| TranslateError::invalid_request("Missing required field: model"))?;
    out.insert("model".to_string(), json!(model));

    let mut system_parts: Vec<String> = Vec::new();
    if let Some(instructions) = src.get("instructions").and_then(|v| v.as_str())
        && !instructions.trim().is_empty()
    {
        system_parts.push(instructions.trim().to_string());
    }

    let mut messages = convert_input(src.get("input"), &mut system_parts)?;
    if let Some(first) = messages.first()
        && first.get("role").and_then(|r| r.as_str()) != Some("user")
    {
        messages.insert(
            0,
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "(continuing the conversation)"}]
            }),
        );
    }
    out.insert("messages".to_string(), Value::Array(messages));

    let max_tokens = resolve_max_tokens(src.get("max_output_tokens"), None);
    let mut out = Value::Object(out);

    // Thinking is resolved before sampling so that a request with thinking
    // enabled can raise max_tokens above the budget, which Anthropic requires.
    let mut resolved_max_tokens = max_tokens;
    if let Some(effort) = src.pointer("/reasoning/effort").and_then(|e| e.as_str())
        && let Some(budget) = effort_to_budget(effort)
        && budget >= MIN_THINKING_BUDGET
    {
        out["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        // Anthropic requires max_tokens strictly greater than budget_tokens;
        // leave extra headroom so the visible answer is not squeezed to nothing.
        if resolved_max_tokens <= budget {
            resolved_max_tokens = budget + super::DEFAULT_MAX_TOKENS;
        }
    }
    out["max_tokens"] = json!(resolved_max_tokens);

    apply_common_sampling(&mut out, src);

    let tools = convert_tools(src.get("tools"))?;
    if !tools.is_empty() {
        out["tools"] = Value::Array(tools);
        // Anthropic 400s on a tool_choice with no tools, so it is only ever set
        // alongside a non-empty tools array.
        if let Some(choice) = convert_tool_choice(src.get("tool_choice"))? {
            out["tool_choice"] = choice;
        }
    }

    if !system_parts.is_empty() {
        out["system"] = json!(system_parts.join("\n\n"));
    }

    Ok(out)
}

/// Reject Responses features the relay cannot honour.
fn reject_unsupported(src: &Value) -> Result<(), TranslateError> {
    if src.get("store").and_then(|v| v.as_bool()) == Some(true) {
        return Err(TranslateError::invalid_request(
            "Unsupported parameter: store = true. This relay does not persist \
             response state.",
        ));
    }
    if let Some(prev) = src.get("previous_response_id")
        && !prev.is_null()
    {
        return Err(TranslateError::invalid_request(
            "Unsupported parameter: previous_response_id. This relay does not \
             persist responses, so conversation chaining is not available; send the \
             full conversation in 'input' instead.",
        ));
    }
    Ok(())
}

/// Translate one `input` item's content parts into Anthropic content blocks.
///
/// `input_text`/`output_text` parts become text blocks; `input_image` becomes
/// an image block, by `image_url` when present or a placeholder note when only
/// `file_id` is given (the relay has no file store to resolve it against).
fn message_content_blocks(parts: &[Value]) -> Vec<Value> {
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        match part.get("type").and_then(|t| t.as_str()) {
            Some("input_text") | Some("output_text") => {
                if let Some(text) = part.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
            Some("input_image") => {
                if let Some(url) = part.get("image_url").and_then(|u| u.as_str()) {
                    if let Some(rest) = url.strip_prefix("data:")
                        && let Some((media_type, data)) = rest.split_once(";base64,")
                    {
                        blocks.push(json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": media_type, "data": data}
                        }));
                    } else {
                        blocks.push(json!({"type": "image", "source": {"type": "url", "url": url}}));
                    }
                }
            }
            _ => {}
        }
    }
    blocks
}

/// Translate the Responses `input` (string or array) into Anthropic messages,
/// extracting any `system`/`developer` array items into `system_parts`.
fn convert_input(input: Option<&Value>, system_parts: &mut Vec<String>) -> Result<Vec<Value>, TranslateError> {
    match input {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) => {
            if s.is_empty() {
                Ok(vec![])
            } else {
                Ok(vec![json!({"role": "user", "content": [{"type": "text", "text": s}]})])
            }
        }
        Some(Value::Array(items)) => convert_input_items(items, system_parts),
        Some(other) => Err(TranslateError::invalid_request(format!(
            "input: must be a string or an array, got {other}"
        ))),
    }
}

fn convert_input_items(items: &[Value], system_parts: &mut Vec<String>) -> Result<Vec<Value>, TranslateError> {
    let mut out: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    let flush = |out: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            out.push(json!({"role": "user", "content": std::mem::take(pending)}));
        }
    };

    for (i, item) in items.iter().enumerate() {
        // A bare string item is shorthand for a user text turn.
        if let Value::String(s) = item {
            flush(&mut out, &mut pending_tool_results);
            out.push(json!({"role": "user", "content": [{"type": "text", "text": s}]}));
            continue;
        }

        match item.get("type").and_then(|t| t.as_str()) {
            Some("function_call") => {
                flush(&mut out, &mut pending_tool_results);
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let parsed_input = tool_input_from_arguments(item.get("arguments"), || {
                    format!("input[{i}] function_call '{call_id}'")
                });
                out.push(json!({
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": call_id, "name": name, "input": parsed_input}]
                }));
            }
            Some("function_call_output") => {
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let content = match item.get("output") {
                    Some(Value::String(s)) => json!(s),
                    Some(other) => other.clone(),
                    None => json!(""),
                };
                pending_tool_results
                    .push(json!({"type": "tool_result", "tool_use_id": call_id, "content": content}));
            }
            Some("message") | None => {
                let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                if role == "system" || role == "developer" {
                    if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str())
                                && !text.is_empty()
                            {
                                system_parts.push(text.to_string());
                            }
                        }
                    } else if let Some(text) = item.get("content").and_then(|c| c.as_str())
                        && !text.is_empty()
                    {
                        system_parts.push(text.to_string());
                    }
                    continue;
                }
                flush(&mut out, &mut pending_tool_results);
                let anthropic_role = if role == "assistant" { "assistant" } else { "user" };
                let content = match item.get("content") {
                    Some(Value::Array(parts)) => message_content_blocks(parts),
                    Some(Value::String(s)) if !s.is_empty() => {
                        vec![json!({"type": "text", "text": s})]
                    }
                    _ => vec![],
                };
                out.push(json!({"role": anthropic_role, "content": content}));
            }
            Some(other) => {
                return Err(TranslateError::invalid_request(format!(
                    "input[{i}]: unsupported item type '{other}'"
                )));
            }
        }
    }
    flush(&mut out, &mut pending_tool_results);
    Ok(out)
}

/// Translate Responses' flat function tool shape
/// (`{"type":"function","name":n,"parameters":p}`) into Anthropic tools.
fn convert_tools(tools: Option<&Value>) -> Result<Vec<Value>, TranslateError> {
    let Some(tools) = tools.and_then(|t| t.as_array()) else {
        return Ok(vec![]);
    };
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        if tool.get("type").and_then(|t| t.as_str()) != Some("function") {
            continue; // Hosted tools (web_search, etc.) have no Anthropic equivalent.
        }
        let name = tool
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| TranslateError::invalid_request("tools[]: function tool missing name"))?;
        let mut entry = json!({
            "name": name,
            "input_schema": tool.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
        });
        if let Some(desc) = tool.get("description") {
            entry["description"] = desc.clone();
        }
        out.push(entry);
    }
    Ok(out)
}

/// Translate `tool_choice`, reusing the same mapping as Chat Completions for
/// the string forms; the object form here is already flat (`{"type":"function","name":n}`)
/// rather than nested under a `function` key.
fn convert_tool_choice(tool_choice: Option<&Value>) -> Result<Option<Value>, TranslateError> {
    match tool_choice {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Ok(Some(json!({"type": "auto"}))),
            "none" => Ok(Some(json!({"type": "none"}))),
            "required" => Ok(Some(json!({"type": "any"}))),
            other => Err(TranslateError::invalid_request(format!(
                "tool_choice: unsupported value '{other}'"
            ))),
        },
        Some(obj @ Value::Object(_)) => {
            let name = obj
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| {
                    TranslateError::invalid_request(
                        "tool_choice: object form must be {\"type\":\"function\",\"name\":...}",
                    )
                })?;
            Ok(Some(json!({"type": "tool", "name": name})))
        }
        Some(other) => Err(TranslateError::invalid_request(format!(
            "tool_choice: unsupported value {other}"
        ))),
    }
}

/// Map an Anthropic `stop_reason` to a Responses completion status.
fn response_status(stop_reason: Option<&str>) -> (&'static str, Option<Value>) {
    if stop_reason == Some("max_tokens") {
        (
            "incomplete",
            Some(json!({"reason": "max_output_tokens"})),
        )
    } else {
        ("completed", None)
    }
}

/// Build a Responses `usage` object from an Anthropic `usage` object.
fn build_usage(usage: Option<&Value>) -> Value {
    let usage = usage.cloned().unwrap_or(json!({}));
    let input = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let output = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let mut out = json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": input + output
    });
    if let Some(cached) = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()) {
        out["input_tokens_details"] = json!({"cached_tokens": cached});
    }
    out
}

/// Build the `output[]` array from Anthropic `content` blocks: one item per
/// block, in the order the blocks arrived — text blocks become `message` items,
/// `tool_use` blocks become `function_call` items.
///
/// Each item carries its own `id` and `status`, matching what the streaming
/// translator announces through `response.output_item.added`/`.done`, so a
/// client sees the same item shape whichever way it called.
fn build_output(content_blocks: &[Value]) -> Vec<Value> {
    let mut output = Vec::new();

    for block in content_blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                let Some(text) = block.get("text").and_then(|t| t.as_str()) else {
                    continue;
                };
                output.push(json!({
                    "type": "message",
                    "id": format!("msg_{}", Uuid::new_v4().simple()),
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text}]
                }));
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let arguments = tool_arguments_from_input(block.get("input"));
                output.push(json!({
                    "type": "function_call",
                    "id": format!("fc_{}", Uuid::new_v4().simple()),
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                    "status": "completed"
                }));
            }
            _ => {}
        }
    }
    output
}

/// Translate a non-streaming Anthropic Messages response into a Responses
/// response object.
///
/// `client_model`, when set, is echoed as the response `model` in place of the
/// upstream's own name, so a per-server model mapping stays invisible to the
/// caller.
pub fn anthropic_to_response(anthropic: &Value, client_model: Option<&str>) -> Value {
    let content_blocks = anthropic
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let (status, incomplete_details) =
        response_status(anthropic.get("stop_reason").and_then(|s| s.as_str()));

    let mut out = json!({
        "id": anthropic.get("id").and_then(|i| i.as_str()).unwrap_or(""),
        "object": "response",
        "model": client_model.or_else(|| anthropic.get("model").and_then(|m| m.as_str())).unwrap_or(""),
        "status": status,
        "output": build_output(&content_blocks),
        "usage": build_usage(anthropic.get("usage"))
    });
    if let Some(details) = incomplete_details {
        out["incomplete_details"] = details;
    }
    out
}

/// Translate an upstream Anthropic error body into a Responses error body.
pub fn anthropic_error_to_response(body: &[u8]) -> Value {
    let message = super::anthropic_error_message(body);
    let error_type = super::anthropic_error_type(body);
    super::error_envelope(super::ClientProtocol::Responses, &error_type, &message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_request() -> Value {
        json!({"model": "claude-opus-4-6", "input": "hi"})
    }

    #[test]
    fn instructions_becomes_system() {
        let src = json!({"model": "m", "instructions": "Be terse.", "input": "hi"});
        let out = request_to_anthropic(&src).unwrap();
        assert_eq!(out["system"], "Be terse.");
    }

    #[test]
    fn string_input_becomes_one_user_message() {
        let src = json!({"model": "m", "input": "What is 2+2?"});
        let out = request_to_anthropic(&src).unwrap();
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "What is 2+2?"}]}])
        );
    }

    #[test]
    fn array_input_with_function_call_and_output() {
        let src = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "weather?"}]},
                {"type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{\"city\":\"Hanoi\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "72F"}
            ]
        });
        let out = request_to_anthropic(&src).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "c1");
    }

    #[test]
    fn input_image_by_url() {
        let src = json!({
            "model": "m",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_image", "image_url": "https://example.com/a.png"}
            ]}]
        });
        let out = request_to_anthropic(&src).unwrap();
        assert_eq!(
            out["messages"][0]["content"][0],
            json!({"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}})
        );
    }

    #[test]
    fn flat_function_tool_shape_maps_to_anthropic_tool() {
        let mut src = simple_request();
        src["tools"] = json!([{"type": "function", "name": "get_weather", "parameters": {"type": "object"}}]);
        let out = request_to_anthropic(&src).unwrap();
        assert_eq!(
            out["tools"][0],
            json!({"name": "get_weather", "input_schema": {"type": "object"}})
        );
    }

    #[test]
    fn max_output_tokens_maps_to_max_tokens() {
        let mut src = simple_request();
        src["max_output_tokens"] = json!(2048);
        let out = request_to_anthropic(&src).unwrap();
        assert_eq!(out["max_tokens"], 2048);
    }

    #[test]
    fn max_tokens_defaults_when_absent() {
        let out = request_to_anthropic(&simple_request()).unwrap();
        assert_eq!(out["max_tokens"], super::super::DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn high_effort_sets_thinking_budget_and_raises_max_tokens() {
        let mut src = simple_request();
        src["reasoning"] = json!({"effort": "high"});
        let out = request_to_anthropic(&src).unwrap();
        assert_eq!(out["thinking"], json!({"type": "enabled", "budget_tokens": 16384}));
        assert!(out["max_tokens"].as_i64().unwrap() > 16384);
    }

    #[test]
    fn unrecognized_effort_sets_no_thinking() {
        let mut src = simple_request();
        src["reasoning"] = json!({"effort": "turbo"});
        let out = request_to_anthropic(&src).unwrap();
        assert!(out.get("thinking").is_none());
    }

    #[test]
    fn store_true_errors() {
        let mut src = simple_request();
        src["store"] = json!(true);
        let err = request_to_anthropic(&src).unwrap_err();
        assert!(err.message.contains("store"));
    }

    #[test]
    fn previous_response_id_errors() {
        let mut src = simple_request();
        src["previous_response_id"] = json!("resp_abc");
        let err = request_to_anthropic(&src).unwrap_err();
        assert!(err.message.contains("previous_response_id"));
    }

    #[test]
    fn store_false_is_accepted() {
        let mut src = simple_request();
        src["store"] = json!(false);
        assert!(request_to_anthropic(&src).is_ok());
    }

    #[test]
    fn tool_choice_is_not_set_when_there_are_no_tools() {
        // Anthropic 400s on tool_choice with no tools, so it must never be sent
        // alongside an empty/absent tools array.
        let mut src = simple_request();
        src["tool_choice"] = json!("auto");
        let out = request_to_anthropic(&src).unwrap();
        assert!(out.get("tool_choice").is_none());
    }

    // --- non-streaming response ---

    #[test]
    fn plain_text_response_is_completed_with_message_output() {
        let anthropic = json!({
            "id": "msg_1", "model": "m", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "4"}],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        });
        let out = anthropic_to_response(&anthropic, None);
        assert_eq!(out["status"], "completed");

        let item = &out["output"][0];
        assert_eq!(out["output"].as_array().unwrap().len(), 1);
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["content"], json!([{"type": "output_text", "text": "4"}]));
        // The item carries its own id, matching what the streaming translator
        // announces; the value is generated, so only the shape is pinned.
        assert!(item["id"].as_str().unwrap().starts_with("msg_"));
    }

    /// Reproduces the reported failure: a client echoed back a `function_call`
    /// whose `arguments` was `""` (a tool with no parameters), and the inbound
    /// seam rejected the whole turn with "unparseable arguments: EOF while
    /// parsing a value at line 1 column 0".
    #[test]
    fn empty_function_call_arguments_are_treated_as_no_arguments() {
        for args in ["", "   "] {
            let src = json!({
                "model": "m",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                    {"type": "function_call", "call_id": "call_1", "name": "get_time", "arguments": args}
                ]
            });
            let out = request_to_anthropic(&src).expect("empty arguments must be accepted");
            let block = &out["messages"][1]["content"][0];
            assert_eq!(block["type"], "tool_use");
            assert_eq!(block["input"], json!({}));
        }
    }

    /// Non-streaming `arguments` must be valid JSON *and* an object for every
    /// shape Anthropic can put in `input`. `null` previously serialized to the
    /// string `"null"`, which parses but is not an object, and round-trips
    /// upstream as `input: null`.
    #[test]
    fn tool_use_input_always_serializes_to_a_json_object_string() {
        let cases = [
            (json!({"city": "Hanoi"}), json!({"city": "Hanoi"})),
            (json!({}), json!({})),
            (Value::Null, json!({})),
        ];
        for (input, expected) in cases {
            let anthropic = json!({
                "id": "msg_1", "model": "m", "stop_reason": "tool_use",
                "content": [{"type": "tool_use", "id": "toolu_1", "name": "f", "input": input}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            });
            let out = anthropic_to_response(&anthropic, None);
            let args = out["output"][0]["arguments"].as_str().unwrap();
            let parsed: Value = serde_json::from_str(args).expect("arguments must parse");
            assert_eq!(parsed, expected);
            assert!(parsed.is_object(), "arguments must decode to an object");
        }
    }

    /// The `input` key absent entirely, not merely null.
    #[test]
    fn tool_use_with_no_input_key_serializes_to_empty_object() {
        let anthropic = json!({
            "id": "msg_1", "model": "m", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "f"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let out = anthropic_to_response(&anthropic, None);
        assert_eq!(out["output"][0]["arguments"], "{}");
    }

    /// This is the exact item from the original bug report: `input[6]` carrying
    /// a `function_call` whose stored `arguments` will not parse. Coerced rather
    /// than rejected — see the matching test in `chat.rs` for why.
    #[test]
    fn every_stored_arguments_shape_becomes_an_object_input() {
        let cases = [
            ("empty", json!("")),
            ("whitespace", json!("   ")),
            ("truncated fragment", json!("{\"a\":")),
            ("bare null", json!("null")),
            ("bare number", json!("123")),
            ("bare string", json!("\"hello\"")),
            ("array", json!("[1,2]")),
            ("not even a string", json!(42)),
            ("outright garbage", json!("{not json")),
        ];
        for (label, args) in cases {
            let src = json!({
                "model": "m",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                    {"type": "function_call", "call_id": "call_2Yf4", "name": "shell", "arguments": args},
                    {"type": "function_call_output", "call_id": "call_2Yf4", "output": "ok"}
                ]
            });
            let out = request_to_anthropic(&src)
                .unwrap_or_else(|e| panic!("{label}: must not be rejected: {}", e.message));
            // input[] here is [message, function_call, function_call_output],
            // so the function_call lands on messages[1] after the leading
            // user turn Anthropic requires.
            let block = &out["messages"][1]["content"][0];
            assert_eq!(block["type"], "tool_use", "{label}");
            assert!(
                block["input"].is_object(),
                "{label}: input must be an object, got {}",
                block["input"]
            );
            assert_eq!(block["input"], json!({}), "{label}");
        }
    }

    /// An `arguments` key absent entirely, not merely empty.
    #[test]
    fn function_call_with_no_arguments_key_becomes_an_object_input() {
        let src = json!({
            "model": "m",
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "f"}
            ]
        });
        let out = request_to_anthropic(&src).expect("must be accepted");
        assert_eq!(out["messages"][1]["content"][0]["input"], json!({}));
    }

    #[test]
    fn well_formed_stored_arguments_are_preserved() {
        let src = json!({
            "model": "m",
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "shell",
                 "arguments": "{\"command\":[\"ls\"],\"timeout_ms\":5000}"}
            ]
        });
        let out = request_to_anthropic(&src).unwrap();
        // messages[0] is the synthetic user turn Anthropic requires when the
        // input starts with an assistant item.
        assert_eq!(
            out["messages"][1]["content"][0]["input"],
            json!({"command": ["ls"], "timeout_ms": 5000})
        );
    }

    /// The non-streaming counterpart of the streaming fix: `thinking` and
    /// `redacted_thinking` blocks must not become empty `message` items.
    #[test]
    fn thinking_blocks_produce_no_output_item() {
        let anthropic = json!({
            "id": "msg_1", "model": "m", "stop_reason": "end_turn",
            "content": [
                {"type": "thinking", "thinking": "step one", "signature": "sig"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "answer"}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let out = anthropic_to_response(&anthropic, None);
        let output = out["output"].as_array().unwrap();
        assert_eq!(output.len(), 1, "only the text block may produce an item");
        assert_eq!(output[0]["content"][0]["text"], "answer");
    }

    #[test]
    fn each_text_block_becomes_its_own_message_item_in_order() {
        // One item per Anthropic block, in block order — the same accounting the
        // streaming translator uses for output_index.
        let anthropic = json!({
            "id": "msg_1", "model": "m", "stop_reason": "end_turn",
            "content": [
                {"type": "text", "text": "first"},
                {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}},
                {"type": "text", "text": "second"}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let out = anthropic_to_response(&anthropic, None);
        let output = out["output"].as_array().unwrap();
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["content"][0]["text"], "first");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[2]["content"][0]["text"], "second");
    }

    #[test]
    fn max_tokens_stop_reason_is_incomplete() {
        let anthropic = json!({
            "id": "msg_1", "model": "m", "stop_reason": "max_tokens",
            "content": [{"type": "text", "text": "cut off"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let out = anthropic_to_response(&anthropic, None);
        assert_eq!(out["status"], "incomplete");
        assert_eq!(out["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn tool_use_response_becomes_function_call_item() {
        let anthropic = json!({
            "id": "msg_1", "model": "m", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Hanoi"}}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let out = anthropic_to_response(&anthropic, None);
        let item = &out["output"][0];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "toolu_1");
        assert_eq!(item["name"], "get_weather");
        assert_eq!(item["arguments"], "{\"city\":\"Hanoi\"}");
        assert_eq!(item["status"], "completed");
        assert!(item["id"].as_str().unwrap().starts_with("fc_"));
    }

    #[test]
    fn usage_maps_with_cached_tokens() {
        let anthropic = json!({
            "id": "msg_1", "model": "m", "stop_reason": "end_turn",
            "content": [], "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 3}
        });
        let out = anthropic_to_response(&anthropic, None);
        assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 3);
        assert_eq!(out["usage"]["total_tokens"], 15);
    }

    #[test]
    fn error_translates_to_responses_envelope() {
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#;
        let out = anthropic_error_to_response(body);
        assert_eq!(out["object"], "error");
        assert_eq!(out["error"]["message"], "bad");
    }
}
