//! Client-protocol translation for the relay.
//!
//! Upstream servers speak only Anthropic Messages (`/v1/messages`). Clients may
//! speak Anthropic Messages, OpenAI Chat Completions, or the OpenAI Responses
//! API. This module translates at two seams in `routes::proxy`: the request body
//! on the way in, and the response (JSON or SSE) on the way out.
//!
//! Everything between those two seams — routing, failover, billing,
//! instrumentation — sees only Anthropic shape.

use serde_json::{Value, json};

pub mod chat;
pub mod chat_sse;
pub mod responses;
pub mod responses_sse;

/// Anthropic requires `max_tokens`; both OpenAI protocols treat their
/// equivalent as optional. This is the fill value when the client omits it.
pub const DEFAULT_MAX_TOKENS: i64 = 4096;

/// Which wire protocol the client is speaking, determined from the request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientProtocol {
    /// Native Anthropic Messages — no translation.
    Anthropic,
    /// OpenAI Chat Completions (`/v1/chat/completions`).
    ChatCompletions,
    /// OpenAI Responses API (`/v1/responses`).
    Responses,
}

impl ClientProtocol {
    /// Classify from the request path alone, before any body parsing.
    ///
    /// Every unrecognised `/v1/*` path classifies as `Anthropic`, which keeps
    /// today's pass-through behaviour for endpoints this module does not
    /// translate — notably `/v1/messages/count_tokens`, whose waterfall must
    /// keep forwarding to the client's original path.
    pub fn from_path(path: &str) -> Self {
        match path {
            "/v1/chat/completions" => Self::ChatCompletions,
            "/v1/responses" => Self::Responses,
            _ => Self::Anthropic,
        }
    }

    /// Whether a request on this protocol needs body/response translation.
    pub fn needs_translation(self) -> bool {
        !matches!(self, Self::Anthropic)
    }
}

/// A request that cannot be represented as an Anthropic Messages call.
///
/// Carries the pieces an error envelope needs so the caller can render it in
/// whichever protocol the client is speaking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateError {
    pub error_type: String,
    pub message: String,
}

impl TranslateError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            error_type: "invalid_request_error".to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error_type, self.message)
    }
}

/// Build an error body in the envelope the given protocol expects.
///
/// Anthropic uses a top-level `type` discriminator; both OpenAI protocols nest
/// everything under `error`, and Responses adds an `object` discriminator.
pub fn error_envelope(protocol: ClientProtocol, error_type: &str, message: &str) -> Value {
    match protocol {
        ClientProtocol::Anthropic => json!({
            "type": "error",
            "error": { "type": error_type, "message": message }
        }),
        ClientProtocol::ChatCompletions => json!({
            "error": {
                "message": message,
                "type": error_type,
                "param": null,
                "code": null
            }
        }),
        ClientProtocol::Responses => json!({
            "object": "error",
            "error": {
                "message": message,
                "type": error_type,
                "param": null,
                "code": null
            }
        }),
    }
}

/// Pull an Anthropic error body's `error.message` out, for re-wrapping in
/// another protocol's envelope.
///
/// Falls back to the raw text when the body is not an Anthropic error envelope,
/// so an upstream returning HTML or a bare string still surfaces something
/// actionable rather than being swallowed.
pub fn anthropic_error_message(body: &[u8]) -> String {
    if let Ok(json) = serde_json::from_slice::<Value>(body)
        && let Some(message) = json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
    {
        return message.to_string();
    }
    String::from_utf8_lossy(body).trim().to_string()
}

/// Pull an Anthropic error body's `error.type` out, defaulting to `api_error`.
pub fn anthropic_error_type(body: &[u8]) -> String {
    if let Ok(json) = serde_json::from_slice::<Value>(body)
        && let Some(error_type) = json
            .get("error")
            .and_then(|e| e.get("type"))
            .and_then(|t| t.as_str())
    {
        return error_type.to_string();
    }
    "api_error".to_string()
}

/// Parse a `max_tokens`-like field, falling back to [`DEFAULT_MAX_TOKENS`].
fn resolve_max_tokens(primary: Option<&Value>, fallback: Option<&Value>) -> i64 {
    primary
        .and_then(|v| v.as_i64())
        .or_else(|| fallback.and_then(|v| v.as_i64()))
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

/// Copy `temperature`, `top_p`, and `stream` across unchanged, and normalise
/// `stop` (string or array) into Anthropic's `stop_sequences`.
///
/// Shared by both OpenAI protocols: the field names and semantics are identical
/// in each, so duplicating this would be two places to fix one bug.
fn apply_common_sampling(out: &mut Value, src: &Value) {
    for field in ["temperature", "top_p"] {
        if let Some(v) = src.get(field)
            && !v.is_null()
        {
            out[field] = v.clone();
        }
    }
    if let Some(stream) = src.get("stream").and_then(|v| v.as_bool()) {
        out["stream"] = json!(stream);
    }
    match src.get("stop") {
        Some(Value::String(s)) => out["stop_sequences"] = json!([s]),
        Some(Value::Array(arr)) if !arr.is_empty() => out["stop_sequences"] = json!(arr),
        _ => {}
    }
}

/// Whether the client asked for a usage report in its stream.
///
/// Anthropic always reports usage, so this is the only signal for whether the
/// translated stream should surface a usage chunk. OpenAI clients that did not
/// ask must not receive one.
pub fn wants_stream_usage(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|json| {
            json.get("stream_options")?
                .get("include_usage")?
                .as_bool()
        })
        .unwrap_or(false)
}

/// What the outbound seam needs to remember from the inbound request.
///
/// Two facts about the *request* are only actionable when translating the
/// *response*, so they are captured once on the way in and carried across:
/// whether the client asked for a streamed usage report, and the name of the
/// synthetic tool standing in for a `json_schema` response format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationContext {
    pub protocol: ClientProtocol,
    pub include_usage: bool,
    pub json_schema_tool: Option<String>,
    /// The model name exactly as the client sent it, captured before per-server
    /// `model_mappings` rewrites it.
    ///
    /// Echoed back in the translated response instead of the upstream's own
    /// model name: OpenAI SDKs and many client-side routers assert that the
    /// response `model` matches what they asked for, and a per-server mapping
    /// (`gpt-4o` -> `claude-sonnet-4-6`) would otherwise surface the mapped
    /// name and break that assertion.
    pub client_model: Option<String>,
}

impl TranslationContext {
    pub fn anthropic() -> Self {
        Self {
            protocol: ClientProtocol::Anthropic,
            include_usage: false,
            json_schema_tool: None,
            client_model: None,
        }
    }
}

/// Translate a client request body into an Anthropic Messages body, returning
/// the translated bytes together with what the response side will need.
///
/// `Anthropic` returns the bytes untouched — the relay's existing behaviour,
/// bit for bit.
pub fn request_to_anthropic(
    protocol: ClientProtocol,
    body: &[u8],
) -> Result<(Vec<u8>, TranslationContext), TranslateError> {
    if protocol == ClientProtocol::Anthropic {
        return Ok((body.to_vec(), TranslationContext::anthropic()));
    }

    let src: Value = serde_json::from_slice(body)
        .map_err(|e| TranslateError::invalid_request(format!("Invalid JSON body: {e}")))?;

    let (out, json_schema_tool) = match protocol {
        ClientProtocol::ChatCompletions => chat::request_to_anthropic(&src)?,
        ClientProtocol::Responses => (responses::request_to_anthropic(&src)?, None),
        ClientProtocol::Anthropic => unreachable!("handled above"),
    };

    let bytes = serde_json::to_vec(&out).map_err(|e| {
        TranslateError::invalid_request(format!("Failed to serialise request: {e}"))
    })?;

    // Read from `src` (the client's own body), not from `out`: the translated
    // body is what per-server model_mappings will rewrite, and the point of
    // this field is to remember what the client asked for.
    let client_model = src
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string);

    Ok((
        bytes,
        TranslationContext {
            protocol,
            include_usage: wants_stream_usage(body),
            json_schema_tool,
            client_model,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_path_classifies_chat_completions() {
        assert_eq!(
            ClientProtocol::from_path("/v1/chat/completions"),
            ClientProtocol::ChatCompletions
        );
    }

    #[test]
    fn from_path_classifies_responses() {
        assert_eq!(
            ClientProtocol::from_path("/v1/responses"),
            ClientProtocol::Responses
        );
    }

    #[test]
    fn from_path_classifies_messages_as_anthropic() {
        assert_eq!(
            ClientProtocol::from_path("/v1/messages"),
            ClientProtocol::Anthropic
        );
    }

    #[test]
    fn from_path_classifies_count_tokens_as_anthropic() {
        // The count-tokens waterfall must keep forwarding to the client's own
        // path; classifying it as an OpenAI protocol would rewrite it.
        assert_eq!(
            ClientProtocol::from_path("/v1/messages/count_tokens"),
            ClientProtocol::Anthropic
        );
    }

    #[test]
    fn from_path_classifies_unknown_v1_path_as_anthropic() {
        assert_eq!(
            ClientProtocol::from_path("/v1/models"),
            ClientProtocol::Anthropic
        );
    }

    #[test]
    fn needs_translation_only_for_openai_protocols() {
        assert!(!ClientProtocol::Anthropic.needs_translation());
        assert!(ClientProtocol::ChatCompletions.needs_translation());
        assert!(ClientProtocol::Responses.needs_translation());
    }

    #[test]
    fn anthropic_error_envelope_uses_top_level_type() {
        let body = error_envelope(ClientProtocol::Anthropic, "not_found_error", "Not found");
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "not_found_error");
        assert_eq!(body["error"]["message"], "Not found");
    }

    #[test]
    fn chat_error_envelope_nests_under_error() {
        let body = error_envelope(
            ClientProtocol::ChatCompletions,
            "authentication_error",
            "Invalid API key",
        );
        assert!(body.get("type").is_none());
        assert_eq!(body["error"]["message"], "Invalid API key");
        assert_eq!(body["error"]["type"], "authentication_error");
        assert!(body["error"]["param"].is_null());
        assert!(body["error"]["code"].is_null());
    }

    #[test]
    fn responses_error_envelope_adds_object_discriminator() {
        let body = error_envelope(ClientProtocol::Responses, "rate_limit_error", "Slow down");
        assert_eq!(body["object"], "error");
        assert_eq!(body["error"]["message"], "Slow down");
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[test]
    fn anthropic_error_message_reads_nested_message() {
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}"#;
        assert_eq!(anthropic_error_message(body), "bad model");
        assert_eq!(anthropic_error_type(body), "invalid_request_error");
    }

    #[test]
    fn anthropic_error_message_falls_back_to_raw_text() {
        let body = b"  <html>502 Bad Gateway</html>  ";
        assert_eq!(anthropic_error_message(body), "<html>502 Bad Gateway</html>");
        assert_eq!(anthropic_error_type(body), "api_error");
    }

    #[test]
    fn wants_stream_usage_reads_nested_flag() {
        assert!(wants_stream_usage(
            br#"{"stream":true,"stream_options":{"include_usage":true}}"#
        ));
        assert!(!wants_stream_usage(
            br#"{"stream":true,"stream_options":{"include_usage":false}}"#
        ));
        assert!(!wants_stream_usage(br#"{"stream":true}"#));
        assert!(!wants_stream_usage(b"not json"));
    }

    #[test]
    fn anthropic_request_passes_through_untouched() {
        let body = br#"{"model":"claude-opus-4-6","messages":[]}"#;
        let (out, ctx) = request_to_anthropic(ClientProtocol::Anthropic, body).unwrap();
        assert_eq!(out, body.to_vec());
        assert_eq!(ctx.protocol, ClientProtocol::Anthropic);
        assert!(!ctx.include_usage);
        assert!(ctx.json_schema_tool.is_none());
    }

    #[test]
    fn malformed_json_on_translated_protocol_errors() {
        let err = request_to_anthropic(ClientProtocol::ChatCompletions, b"{oh no").unwrap_err();
        assert_eq!(err.error_type, "invalid_request_error");
        assert!(err.message.contains("Invalid JSON"));
    }
}
