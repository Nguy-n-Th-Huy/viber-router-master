//! OpenAI Chat Completions <-> Anthropic Messages translation.

use serde_json::{Map, Value, json};

use super::{
    TranslateError, apply_common_sampling, resolve_max_tokens, tool_arguments_from_input,
    tool_input_from_arguments,
};

/// Instruction appended to the system prompt for `response_format: json_object`.
///
/// Anthropic Messages has no native JSON mode, so this is best-effort. The
/// stronger `json_schema` form is implemented as a forced tool call instead.
const JSON_OBJECT_INSTRUCTION: &str =
    "You must respond with a single valid JSON object and nothing else. \
     Do not wrap it in markdown code fences and do not add any prose.";

/// Translate a Chat Completions request into an Anthropic Messages request.
///
/// Returns the Anthropic body plus the name of the synthetic tool created for a
/// `json_schema` response format, which the response translator needs in order
/// to unwrap that tool's arguments back into message content.
pub fn request_to_anthropic(src: &Value) -> Result<(Value, Option<String>), TranslateError> {
    let mut out = Map::new();

    let model = src
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| TranslateError::invalid_request("Missing required field: model"))?;
    out.insert("model".to_string(), json!(model));

    reject_unsupported(src)?;

    let messages = src
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or_else(|| TranslateError::invalid_request("Missing required field: messages"))?;

    let (system_text, anthropic_messages) = convert_messages(messages)?;
    out.insert("messages".to_string(), Value::Array(anthropic_messages));

    out.insert(
        "max_tokens".to_string(),
        json!(resolve_max_tokens(
            src.get("max_completion_tokens"),
            src.get("max_tokens")
        )),
    );

    let mut out = Value::Object(out);
    apply_common_sampling(&mut out, src);

    let mut system_parts: Vec<String> = system_text.into_iter().collect();

    let mut tools = convert_tools(src.get("tools"))?;
    let mut tool_choice = convert_tool_choice(src.get("tool_choice"))?;

    let json_schema_tool = apply_response_format(
        src.get("response_format"),
        &mut tools,
        &mut tool_choice,
        &mut system_parts,
    )?;

    if !system_parts.is_empty() {
        out["system"] = json!(system_parts.join("\n\n"));
    }
    if !tools.is_empty() {
        out["tools"] = Value::Array(tools);
    }
    if let Some(choice) = tool_choice {
        out["tool_choice"] = choice;
    }

    Ok((out, json_schema_tool))
}

/// Reject parameters Anthropic cannot express, so a client is told rather than
/// silently getting different behaviour than it asked for.
///
/// `presence_penalty`, `frequency_penalty`, `logit_bias`, `seed`, and `user` are
/// deliberately *not* rejected: SDKs send them at default values routinely, and
/// failing those requests would break clients that never meant to ask for
/// anything unusual.
fn reject_unsupported(src: &Value) -> Result<(), TranslateError> {
    if let Some(n) = src.get("n").and_then(|v| v.as_i64())
        && n != 1
    {
        return Err(TranslateError::invalid_request(format!(
            "Unsupported parameter: n = {n}. Anthropic Messages returns a single \
             completion; only n = 1 can be honoured."
        )));
    }

    for field in ["logprobs", "top_logprobs"] {
        let requested = match src.get(field) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(_)) => true,
            _ => false,
        };
        if requested {
            return Err(TranslateError::invalid_request(format!(
                "Unsupported parameter: {field}. Anthropic Messages does not return \
                 token log probabilities."
            )));
        }
    }

    Ok(())
}

/// Extract plain text from a `system`/`developer` message's content, which may
/// be a string or an array of `{"text": ...}` parts.
fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Parse a Chat Completions `image_url.url` into an Anthropic image block.
///
/// A `data:` URL becomes a `base64` source; anything else is treated as a
/// fetchable URL. An empty media type (a data URL with no type before
/// `;base64,`) falls back to `application/octet-stream` rather than erroring —
/// Anthropic still needs *a* media type, and guessing a generic one is more
/// useful than rejecting a request over a detail the client omitted.
fn image_block_from_image_url(url: &str, msg_index: usize) -> Result<Value, TranslateError> {
    if let Some(rest) = url.strip_prefix("data:") {
        let Some((media_type, data)) = rest.split_once(";base64,") else {
            return Err(TranslateError::invalid_request(format!(
                "messages[{msg_index}]: image_url is a data URL but has no ';base64,' \
                 separator"
            )));
        };
        let media_type = if media_type.is_empty() {
            "application/octet-stream"
        } else {
            media_type
        };
        return Ok(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data }
        }));
    }
    Ok(json!({
        "type": "image",
        "source": { "type": "url", "url": url }
    }))
}

/// Translate one `content` value (string or array of parts) into Anthropic
/// content blocks.
fn content_blocks(content: Option<&Value>, msg_index: usize) -> Result<Vec<Value>, TranslateError> {
    match content {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) => {
            if s.is_empty() {
                Ok(vec![])
            } else {
                Ok(vec![json!({"type": "text", "text": s})])
            }
        }
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for part in parts {
                let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("text");
                match part_type {
                    "text" => {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str())
                            && !text.is_empty()
                        {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                    }
                    "image_url" => {
                        let url = part
                            .get("image_url")
                            .and_then(|iu| iu.get("url"))
                            .and_then(|u| u.as_str())
                            .ok_or_else(|| {
                                TranslateError::invalid_request(format!(
                                    "messages[{msg_index}]: image_url part missing image_url.url"
                                ))
                            })?;
                        blocks.push(image_block_from_image_url(url, msg_index)?);
                    }
                    other => {
                        return Err(TranslateError::invalid_request(format!(
                            "messages[{msg_index}]: unsupported content part type '{other}'"
                        )));
                    }
                }
            }
            Ok(blocks)
        }
        Some(other) => Err(TranslateError::invalid_request(format!(
            "messages[{msg_index}]: content must be a string or an array, got {other}"
        ))),
    }
}

/// Translate an assistant message's `tool_calls` into Anthropic `tool_use`
/// blocks, preserving call order.
fn tool_use_blocks(tool_calls: &[Value], msg_index: usize) -> Result<Vec<Value>, TranslateError> {
    let mut blocks = Vec::with_capacity(tool_calls.len());
    for tc in tool_calls {
        let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
        let function = tc.get("function").cloned().unwrap_or(json!({}));
        let name = function.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let input = tool_input_from_arguments(function.get("arguments"), || {
            format!("messages[{msg_index}].tool_calls id '{id}'")
        });
        blocks.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
    }
    Ok(blocks)
}

/// Translate a `role: "tool"` (or legacy `role: "function"`) message's content
/// into one Anthropic `tool_result` block.
fn tool_result_block(msg: &Value, msg_index: usize) -> Result<Value, TranslateError> {
    let tool_call_id = msg
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .or_else(|| msg.get("name").and_then(|v| v.as_str()))
        .ok_or_else(|| {
            TranslateError::invalid_request(format!(
                "messages[{msg_index}]: tool result message missing tool_call_id"
            ))
        })?;
    // Anthropic accepts a plain string as tool_result content, so a string
    // content is passed through as-is rather than wrapped in a text block.
    let content = match msg.get("content") {
        Some(Value::String(s)) => json!(s),
        Some(other) => other.clone(),
        None => json!(""),
    };
    Ok(json!({
        "type": "tool_result",
        "tool_use_id": tool_call_id,
        "content": content
    }))
}

/// Translate the Chat Completions `messages` array into Anthropic `system`
/// text plus an Anthropic `messages` array.
///
/// `system`/`developer` messages are extracted regardless of position.
/// Consecutive `tool`/`function` messages are merged into a single Anthropic
/// `user` message, since Anthropic requires every tool result answering one
/// assistant turn to arrive together. A leading synthetic user message is
/// inserted if the conversation would otherwise not start with `user` —
/// Anthropic requires the first message to be from the user.
fn convert_messages(messages: &[Value]) -> Result<(Vec<String>, Vec<Value>), TranslateError> {
    let mut system_parts = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    let flush_tool_results = |out: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            out.push(json!({"role": "user", "content": std::mem::take(pending)}));
        }
    };

    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");

        if role == "tool" || role == "function" {
            pending_tool_results.push(tool_result_block(msg, i)?);
            continue;
        }
        flush_tool_results(&mut out, &mut pending_tool_results);

        match role {
            "system" | "developer" => {
                let text = extract_text(msg.get("content"));
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            "assistant" => {
                let mut content = content_blocks(msg.get("content"), i)?;
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array())
                    && !tool_calls.is_empty()
                {
                    content.extend(tool_use_blocks(tool_calls, i)?);
                }
                out.push(json!({"role": "assistant", "content": content}));
            }
            _ => {
                let content = content_blocks(msg.get("content"), i)?;
                out.push(json!({"role": "user", "content": content}));
            }
        }
    }
    flush_tool_results(&mut out, &mut pending_tool_results);

    if needs_synthetic_leading_user(&out) {
        out.insert(
            0,
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "(continuing the conversation)"}]
            }),
        );
    }

    Ok((system_parts, out))
}

/// Whether a synthetic leading user message must be inserted.
///
/// Anthropic requires the first message to be `user`, but a `user` first message
/// is not sufficient on its own: a conversation that opens with a tool result
/// (a resumed or compacted session) produces a `user` message whose only block
/// is a `tool_result` with no preceding `tool_use` to answer, which upstream
/// rejects. Both shapes need the synthetic turn in front.
fn needs_synthetic_leading_user(messages: &[Value]) -> bool {
    let Some(first) = messages.first() else {
        return false;
    };
    if first.get("role").and_then(|r| r.as_str()) != Some("user") {
        return true;
    }
    let Some(blocks) = first.get("content").and_then(|c| c.as_array()) else {
        return false;
    };
    !blocks.is_empty()
        && blocks
            .iter()
            .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
}

/// Translate `tools[].function` entries into Anthropic tool definitions.
fn convert_tools(tools: Option<&Value>) -> Result<Vec<Value>, TranslateError> {
    let Some(tools) = tools.and_then(|t| t.as_array()) else {
        return Ok(vec![]);
    };
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        let function = tool.get("function").ok_or_else(|| {
            TranslateError::invalid_request("tools[]: entry missing 'function'")
        })?;
        let name = function
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| TranslateError::invalid_request("tools[].function: missing name"))?;
        let mut entry = json!({
            "name": name,
            "input_schema": function.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
        });
        if let Some(desc) = function.get("description") {
            entry["description"] = desc.clone();
        }
        out.push(entry);
    }
    Ok(out)
}

/// Translate `tool_choice` into Anthropic's `tool_choice` shape.
///
/// `"none"` is passed through as `{"type":"none"}` rather than dropped: the
/// caller still keeps the tool *definitions* so a later turn in the same
/// conversation can use them, only this turn is forced not to.
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
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .ok_or_else(|| {
                    TranslateError::invalid_request(
                        "tool_choice: object form must be {\"type\":\"function\",\"function\":{\"name\":...}}",
                    )
                })?;
            Ok(Some(json!({"type": "tool", "name": name})))
        }
        Some(other) => Err(TranslateError::invalid_request(format!(
            "tool_choice: unsupported value {other}"
        ))),
    }
}

/// Apply `response_format` by either appending a system instruction
/// (`json_object`) or forcing a synthetic tool call (`json_schema`).
///
/// Returns the synthetic tool's name when one was created, so the response
/// translator can unwrap its `tool_use.input` back into `message.content`.
fn apply_response_format(
    response_format: Option<&Value>,
    tools: &mut Vec<Value>,
    tool_choice: &mut Option<Value>,
    system_parts: &mut Vec<String>,
) -> Result<Option<String>, TranslateError> {
    let Some(rf) = response_format else {
        return Ok(None);
    };
    match rf.get("type").and_then(|t| t.as_str()) {
        Some("json_object") => {
            system_parts.push(JSON_OBJECT_INSTRUCTION.to_string());
            Ok(None)
        }
        Some("json_schema") => {
            if !tools.is_empty() {
                return Err(TranslateError::invalid_request(
                    "response_format: 'json_schema' cannot be combined with the request's \
                     own 'tools' — the relay cannot tell the resulting tool call apart \
                     from a real one",
                ));
            }
            let schema_spec = rf.get("json_schema").ok_or_else(|| {
                TranslateError::invalid_request("response_format: missing json_schema")
            })?;
            let name = schema_spec
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("structured_output")
                .to_string();
            let schema = schema_spec
                .get("schema")
                .cloned()
                .unwrap_or(json!({"type": "object"}));
            tools.push(json!({"name": name, "input_schema": schema}));
            *tool_choice = Some(json!({"type": "tool", "name": name}));
            Ok(Some(name))
        }
        Some("text") | None => Ok(None),
        Some(other) => Err(TranslateError::invalid_request(format!(
            "response_format: unsupported type '{other}'"
        ))),
    }
}

/// Map an Anthropic `stop_reason` to a Chat Completions `finish_reason`.
///
/// Shared by the non-streaming and streaming translators so the two paths can
/// never disagree on this table.
pub(crate) fn map_stop_reason(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("end_turn") | Some("stop_sequence") => "stop",
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    }
}

/// Translate a non-streaming Anthropic Messages response into a Chat
/// Completions response.
///
/// `json_schema_tool`, when set, names the synthetic tool `request_to_anthropic`
/// forced for a `json_schema` response format; a matching `tool_use` block is
/// unwrapped back into `message.content` as a JSON string instead of being
/// exposed as a tool call, so the `json_schema` request shape stays invisible
/// on the way out.
/// `client_model`, when set, is echoed as the response `model` in place of the
/// upstream's own name, so a per-server model mapping stays invisible to a
/// client that asserts on getting back the model it asked for.
pub fn anthropic_to_response(
    anthropic: &Value,
    json_schema_tool: Option<&str>,
    client_model: Option<&str>,
) -> Value {
    let content_blocks = anthropic
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut schema_json: Option<String> = None;

    for block in &content_blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                }
            }
            Some("tool_use") => {
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if json_schema_tool == Some(name) {
                    schema_json = Some(
                        serde_json::to_string(block.get("input").unwrap_or(&json!({})))
                            .unwrap_or_else(|_| "{}".to_string()),
                    );
                    continue;
                }
                let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let arguments = tool_arguments_from_input(block.get("input"));
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }));
            }
            _ => {}
        }
    }

    // A synthetic schema tool call is a successful structured completion, not a
    // real tool-use turn, so it always reports "stop" regardless of the
    // upstream stop_reason (which will be "tool_use").
    let finish_reason = if schema_json.is_some() {
        "stop"
    } else {
        map_stop_reason(anthropic.get("stop_reason").and_then(|s| s.as_str()))
    };

    let mut message = json!({"role": "assistant"});
    if let Some(json_str) = schema_json {
        message["content"] = json!(json_str);
    } else if !tool_calls.is_empty() {
        message["content"] = if text_parts.is_empty() {
            Value::Null
        } else {
            json!(text_parts.join(""))
        };
        message["tool_calls"] = json!(tool_calls);
    } else {
        message["content"] = json!(text_parts.join(""));
    }

    let usage = build_usage(anthropic.get("usage"));

    json!({
        "id": anthropic.get("id").and_then(|i| i.as_str()).unwrap_or(""),
        "object": "chat.completion",
        "model": client_model.or_else(|| anthropic.get("model").and_then(|m| m.as_str())).unwrap_or(""),
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason
        }],
        "usage": usage
    })
}

/// Build a Chat Completions `usage` object from an Anthropic `usage` object.
pub(crate) fn build_usage(usage: Option<&Value>) -> Value {
    let usage = usage.cloned().unwrap_or(json!({}));
    let input = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let output = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let mut out = json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output
    });
    if let Some(cached) = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()) {
        out["prompt_tokens_details"] = json!({"cached_tokens": cached});
    }
    out
}

/// Translate an upstream Anthropic error body into a Chat Completions error
/// body.
pub fn anthropic_error_to_response(body: &[u8]) -> Value {
    let message = super::anthropic_error_message(body);
    let error_type = super::anthropic_error_type(body);
    super::error_envelope(super::ClientProtocol::ChatCompletions, &error_type, &message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_request() -> Value {
        json!({
            "model": "claude-opus-4-6",
            "messages": [{"role": "user", "content": "hi"}]
        })
    }

    // --- system/developer extraction ---

    #[test]
    fn system_message_becomes_top_level_system() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "hi"}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["system"], "Be terse.");
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["role"], "user");
    }

    #[test]
    fn developer_message_not_first_is_still_extracted() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "developer", "content": "Follow the rules."}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["system"], "Follow the rules.");
        // Only the user message remains inline.
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    // --- multi-turn / ordering ---

    #[test]
    fn multi_turn_preserves_order_and_roles() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "1"},
                {"role": "assistant", "content": "2"},
                {"role": "user", "content": "3"}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], json!([{"type": "text", "text": "1"}]));
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["role"], "user");
    }

    // --- content parts ---

    #[test]
    fn base64_image_part_translates() {
        let src = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(
            out["messages"][0]["content"][0],
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}})
        );
    }

    #[test]
    fn remote_image_url_part_translates() {
        let src = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]}]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(
            out["messages"][0]["content"][0],
            json!({"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}})
        );
    }

    #[test]
    fn unparseable_data_url_errors_with_message_index() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "ok"},
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png,notbase64"}}
                ]}
            ]
        });
        let err = request_to_anthropic(&src).unwrap_err();
        assert!(err.message.contains("messages[1]"));
    }

    // --- tool_calls -> tool_use ---

    #[test]
    fn assistant_tool_call_becomes_tool_use_block() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Hanoi\"}"}}
                ]}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(
            out["messages"][1]["content"],
            json!([{"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "Hanoi"}}])
        );
    }

    #[test]
    fn text_blocks_come_before_tool_use_blocks() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": "Sure, checking.", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
                ]}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        let content = out["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
    }

    /// Unparseable stored arguments are coerced, not rejected. This reverses an
    /// earlier choice to 400 here: the truncated fragments a client can have on
    /// disk were produced by this relay's own pre-buffering Chat translator, so
    /// refusing them made the user pay permanently for that bug — every later
    /// turn re-sends the same history and gets the same 400, with no way out
    /// short of discarding the session.
    #[test]
    fn unparseable_tool_call_arguments_are_coerced_not_rejected() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_bad", "type": "function", "function": {"name": "f", "arguments": "{not json"}}
                ]}
            ]
        });
        let (out, _) = request_to_anthropic(&src).expect("must not be rejected");
        assert_eq!(out["messages"][1]["content"][0]["input"], json!({}));
    }

    /// Every `arguments` shape a client can echo back from its stored history
    /// must yield an object `input`, because Anthropic requires `tool_use.input`
    /// to be one — and because an inbound `tool_calls[]` entry is always
    /// *history* (the matching `role: "tool"` message carries what the tool
    /// really returned), so nothing executes on the strength of these values.
    /// Rejecting them would brick a session permanently: every later turn
    /// re-sends the same stored history.
    #[test]
    fn every_stored_arguments_shape_becomes_an_object_input() {
        let cases = [
            ("empty", json!("")),
            ("whitespace", json!("   ")),
            ("truncated fragment", json!("{\"command\":[\"pwsh\",")),
            ("bare null", json!("null")),
            ("bare number", json!("123")),
            ("bare string", json!("\"hello\"")),
            ("array", json!("[1,2]")),
            ("not even a string", json!(42)),
        ];
        for (label, args) in cases {
            let src = json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": "go"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": args}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "done"}
                ]
            });
            let (out, _) = request_to_anthropic(&src)
                .unwrap_or_else(|e| panic!("{label}: must not be rejected: {}", e.message));
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

    /// A well-formed object is passed through untouched — the tolerance above
    /// must not flatten real arguments.
    #[test]
    fn well_formed_stored_arguments_are_preserved() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{\"city\":\"Hanoi\",\"n\":2}"}}
                ]}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(
            out["messages"][1]["content"][0]["input"],
            json!({"city": "Hanoi", "n": 2})
        );
    }

    #[test]
    fn empty_tool_call_arguments_are_treated_as_no_arguments() {
        for args in ["", "   "] {
            let src = json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": "go"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": args}}
                    ]}
                ]
            });
            let (out, _) = request_to_anthropic(&src).expect("empty arguments must be accepted");
            let block = &out["messages"][1]["content"][0];
            assert_eq!(block["type"], "tool_use");
            assert_eq!(block["input"], json!({}), "empty arguments become an empty input");
        }
    }

    // --- tool result messages ---

    #[test]
    fn two_tool_results_merge_into_one_user_message() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "c2", "type": "function", "function": {"name": "g", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "result1"},
                {"role": "tool", "tool_call_id": "c2", "content": "result2"}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3); // user, assistant, merged-tool-results-user
        let merged = &msgs[2];
        assert_eq!(merged["role"], "user");
        let content = merged["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["tool_use_id"], "c1");
        assert_eq!(content[0]["content"], "result1");
        assert_eq!(content[1]["tool_use_id"], "c2");
    }

    #[test]
    fn tool_result_string_content_passed_through_as_string() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "tool", "tool_call_id": "c1", "content": "72F and sunny"}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["content"][0]["content"], "72F and sunny");
    }

    #[test]
    fn legacy_function_role_treated_as_tool_result() {
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "function", "name": "get_weather", "content": "sunny"}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn leading_tool_message_gets_synthetic_user_prefix() {
        // A resumed conversation could start with a tool result; Anthropic
        // requires the first message to be role "user".
        let src = json!({
            "model": "m",
            "messages": [
                {"role": "tool", "tool_call_id": "c1", "content": "result"}
            ]
        });
        let (out, _) = request_to_anthropic(&src).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
    }

    // --- sampling params ---

    #[test]
    fn stop_as_single_string_becomes_stop_sequences_array() {
        let mut src = simple_request();
        src["stop"] = json!("STOP");
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["stop_sequences"], json!(["STOP"]));
    }

    #[test]
    fn stop_as_array_passes_through() {
        let mut src = simple_request();
        src["stop"] = json!(["STOP", "END"]);
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["stop_sequences"], json!(["STOP", "END"]));
    }

    #[test]
    fn temperature_and_top_p_pass_through() {
        let mut src = simple_request();
        src["temperature"] = json!(0.5);
        src["top_p"] = json!(0.9);
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["temperature"], 0.5);
        assert_eq!(out["top_p"], 0.9);
    }

    #[test]
    fn stream_options_include_usage_does_not_error() {
        let mut src = simple_request();
        src["stream"] = json!(true);
        src["stream_options"] = json!({"include_usage": true});
        assert!(request_to_anthropic(&src).is_ok());
    }

    // --- max_tokens ---

    #[test]
    fn max_completion_tokens_wins_over_max_tokens() {
        let mut src = simple_request();
        src["max_tokens"] = json!(999);
        src["max_completion_tokens"] = json!(2048);
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["max_tokens"], 2048);
    }

    #[test]
    fn max_tokens_defaults_when_absent() {
        let src = simple_request();
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["max_tokens"], crate::translate::DEFAULT_MAX_TOKENS);
    }

    // --- tools / tool_choice ---

    #[test]
    fn tool_definition_translates() {
        let mut src = simple_request();
        src["tools"] = json!([{
            "type": "function",
            "function": {"name": "get_weather", "description": "d", "parameters": {"type": "object"}}
        }]);
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(
            out["tools"][0],
            json!({"name": "get_weather", "description": "d", "input_schema": {"type": "object"}})
        );
    }

    #[test]
    fn named_tool_choice_maps_to_tool_type() {
        let mut src = simple_request();
        src["tools"] = json!([{"type": "function", "function": {"name": "get_weather", "parameters": {}}}]);
        src["tool_choice"] = json!({"type": "function", "function": {"name": "get_weather"}});
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["tool_choice"], json!({"type": "tool", "name": "get_weather"}));
    }

    #[test]
    fn required_tool_choice_maps_to_any() {
        let mut src = simple_request();
        src["tools"] = json!([{"type": "function", "function": {"name": "f", "parameters": {}}}]);
        src["tool_choice"] = json!("required");
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["tool_choice"], json!({"type": "any"}));
    }

    #[test]
    fn none_tool_choice_keeps_tools_but_disables_use() {
        let mut src = simple_request();
        src["tools"] = json!([{"type": "function", "function": {"name": "f", "parameters": {}}}]);
        src["tool_choice"] = json!("none");
        let (out, _) = request_to_anthropic(&src).unwrap();
        assert_eq!(out["tool_choice"], json!({"type": "none"}));
        assert_eq!(out["tools"].as_array().unwrap().len(), 1);
    }

    // --- response_format ---

    #[test]
    fn json_object_appends_system_instruction() {
        let mut src = simple_request();
        src["response_format"] = json!({"type": "json_object"});
        let (out, tool) = request_to_anthropic(&src).unwrap();
        assert!(tool.is_none());
        assert!(out["system"].as_str().unwrap().contains("JSON"));
        assert!(out.get("tools").is_none());
    }

    #[test]
    fn json_schema_with_no_tools_becomes_forced_synthetic_tool() {
        let mut src = simple_request();
        src["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": "x", "schema": {"type": "object", "properties": {"a": {"type": "string"}}}}
        });
        let (out, tool) = request_to_anthropic(&src).unwrap();
        assert_eq!(tool, Some("x".to_string()));
        assert_eq!(out["tools"][0]["name"], "x");
        assert_eq!(out["tool_choice"], json!({"type": "tool", "name": "x"}));
    }

    #[test]
    fn json_schema_with_client_tools_errors() {
        let mut src = simple_request();
        src["tools"] = json!([{"type": "function", "function": {"name": "f", "parameters": {}}}]);
        src["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": "x", "schema": {}}
        });
        let err = request_to_anthropic(&src).unwrap_err();
        assert!(err.message.contains("json_schema"));
    }

    // --- unsupported params ---

    #[test]
    fn n_greater_than_one_errors() {
        let mut src = simple_request();
        src["n"] = json!(2);
        let err = request_to_anthropic(&src).unwrap_err();
        assert!(err.message.contains('n'));
    }

    #[test]
    fn n_equal_one_is_accepted() {
        let mut src = simple_request();
        src["n"] = json!(1);
        assert!(request_to_anthropic(&src).is_ok());
    }

    #[test]
    fn logprobs_errors() {
        let mut src = simple_request();
        src["logprobs"] = json!(true);
        let err = request_to_anthropic(&src).unwrap_err();
        assert!(err.message.contains("logprobs"));
    }

    #[test]
    fn seed_and_user_are_silently_ignored() {
        let mut src = simple_request();
        src["seed"] = json!(42);
        src["user"] = json!("u1");
        assert!(request_to_anthropic(&src).is_ok());
    }

    // --- non-streaming response ---

    #[test]
    fn plain_text_response_maps_stop_and_content() {
        let anthropic = json!({
            "id": "msg_1", "model": "claude-opus-4-6", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 3, "output_tokens": 2}
        });
        let out = anthropic_to_response(&anthropic, None, None);
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
        assert_eq!(out["choices"][0]["message"]["role"], "assistant");
    }

    #[test]
    fn tool_use_response_maps_to_tool_calls() {
        let anthropic = json!({
            "id": "msg_1", "model": "m", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Hanoi"}}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let out = anthropic_to_response(&anthropic, None, None);
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0],
            json!({"id": "toolu_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Hanoi\"}"}})
        );
    }

    /// Chat Completions counterpart: `arguments` must decode to an object for
    /// every shape `input` can take, including `null` and an absent key.
    #[test]
    fn tool_use_input_always_serializes_to_a_json_object_string() {
        let cases = [
            (Some(json!({"city": "Hanoi"})), json!({"city": "Hanoi"})),
            (Some(json!({})), json!({})),
            (Some(Value::Null), json!({})),
            (None, json!({})),
        ];
        for (input, expected) in cases {
            let mut block = json!({"type": "tool_use", "id": "toolu_1", "name": "f"});
            if let Some(input) = input {
                block["input"] = input;
            }
            let anthropic = json!({
                "id": "msg_1", "model": "m", "stop_reason": "tool_use",
                "content": [block],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            });
            let out = anthropic_to_response(&anthropic, None, None);
            let args = out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap();
            let parsed: Value = serde_json::from_str(args).expect("arguments must parse");
            assert_eq!(parsed, expected);
            assert!(parsed.is_object());
        }
    }

    #[test]
    fn synthetic_schema_tool_unwraps_to_content_string() {
        let anthropic = json!({
            "id": "msg_1", "model": "m", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "x", "input": {"a": "b"}}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let out = anthropic_to_response(&anthropic, Some("x"), None);
        assert_eq!(out["choices"][0]["message"]["content"], "{\"a\":\"b\"}");
        assert!(out["choices"][0]["message"].get("tool_calls").is_none());
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn usage_maps_with_cache_read() {
        let usage = build_usage(Some(&json!({
            "input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 3
        })));
        assert_eq!(
            usage,
            json!({
                "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
                "prompt_tokens_details": {"cached_tokens": 3}
            })
        );
    }

    #[test]
    fn usage_omits_cache_details_when_absent() {
        let usage = build_usage(Some(&json!({"input_tokens": 10, "output_tokens": 5})));
        assert!(usage.get("prompt_tokens_details").is_none());
    }

    #[test]
    fn stop_reason_mapping_table() {
        assert_eq!(map_stop_reason(Some("end_turn")), "stop");
        assert_eq!(map_stop_reason(Some("stop_sequence")), "stop");
        assert_eq!(map_stop_reason(Some("max_tokens")), "length");
        assert_eq!(map_stop_reason(Some("tool_use")), "tool_calls");
    }

    // --- error translation ---

    #[test]
    fn anthropic_error_translates_to_chat_envelope() {
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}"#;
        let out = anthropic_error_to_response(body);
        assert!(out.get("type").is_none());
        assert_eq!(out["error"]["message"], "bad model");
        assert_eq!(out["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn non_json_error_body_falls_back_to_raw_text() {
        let out = anthropic_error_to_response(b"upstream exploded");
        assert_eq!(out["error"]["message"], "upstream exploded");
    }
}
