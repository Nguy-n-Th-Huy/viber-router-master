#[derive(Debug, Clone)]
pub struct ParsedUsage {
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_creation_tokens: Option<i32>,
    pub cache_read_tokens: Option<i32>,
}

pub struct SseUsageParser {
    buffer: Vec<u8>,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    cache_creation_tokens: Option<i32>,
    cache_read_tokens: Option<i32>,
}

impl SseUsageParser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            input_tokens: None,
            output_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
        self.process_events();
    }

    pub fn finish(mut self) -> Option<ParsedUsage> {
        // Process any remaining data
        self.process_events();
        match (self.input_tokens, self.output_tokens) {
            (Some(input), Some(output)) => Some(ParsedUsage {
                input_tokens: input,
                output_tokens: output,
                cache_creation_tokens: self.cache_creation_tokens,
                cache_read_tokens: self.cache_read_tokens,
            }),
            _ => None,
        }
    }

    fn process_events(&mut self) {
        loop {
            let delimiter = find_double_newline(&self.buffer);
            let pos = match delimiter {
                Some(p) => p,
                None => break,
            };

            // Copy the event bytes out to avoid borrow conflict
            let event_bytes = self.buffer[..pos].to_vec();
            self.parse_event(&event_bytes);

            let skip = pos + 2;
            self.buffer.drain(..skip);
        }
    }

    fn parse_event(&mut self, event_bytes: &[u8]) {
        let event_str = match std::str::from_utf8(event_bytes) {
            Ok(s) => s,
            Err(_) => return,
        };

        // Extract the data line(s)
        for line in event_str.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                self.parse_data(data);
            }
        }
    }

    fn parse_data(&mut self, data: &str) {
        let json: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return,
        };

        let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type {
            "message_start" => {
                if let Some(usage) = json.get("message").and_then(|m| m.get("usage")) {
                    self.input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                    self.cache_creation_tokens = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                    self.cache_read_tokens = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                }
            }
            "message_delta" => {
                if let Some(usage) = json.get("usage") {
                    self.output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                    // Some upstreams (e.g. proxies/gateways) send input_tokens: 0 in
                    // message_start and the real count in message_delta. Overwrite if
                    // present and non-zero so we capture the actual value.
                    if let Some(input) = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        && (input > 0 || self.input_tokens.is_none())
                    {
                        self.input_tokens = Some(input);
                    }
                    if let Some(cc) = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        && (cc > 0 || self.cache_creation_tokens.is_none())
                    {
                        self.cache_creation_tokens = Some(cc);
                    }
                    if let Some(cr) = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        && (cr > 0 || self.cache_read_tokens.is_none())
                    {
                        self.cache_read_tokens = Some(cr);
                    }
                }
            }
            _ => {}
        }
    }
}


fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complete_event_single_chunk() {
        let mut parser = SseUsageParser::new();
        let data = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":50}}\n\n";
        parser.feed(data);
        let result = parser.finish().unwrap();
        assert_eq!(result.input_tokens, 100);
        assert_eq!(result.output_tokens, 50);
    }

    #[test]
    fn test_event_split_across_chunks() {
        let mut parser = SseUsageParser::new();
        let full = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":200}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":75}}\n\n";
        // Split in the middle
        let mid = full.len() / 2;
        parser.feed(&full[..mid]);
        parser.feed(&full[mid..]);
        let result = parser.finish().unwrap();
        assert_eq!(result.input_tokens, 200);
        assert_eq!(result.output_tokens, 75);
    }

    #[test]
    fn test_message_start_with_cache_tokens() {
        let mut parser = SseUsageParser::new();
        let data = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":500,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":200}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":300}}\n\n";
        parser.feed(data);
        let result = parser.finish().unwrap();
        assert_eq!(result.input_tokens, 500);
        assert_eq!(result.output_tokens, 300);
        assert_eq!(result.cache_creation_tokens, Some(100));
        assert_eq!(result.cache_read_tokens, Some(200));
    }

    #[test]
    fn test_message_delta_with_output_tokens() {
        let mut parser = SseUsageParser::new();
        let data = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":999}}\n\n";
        parser.feed(data);
        let result = parser.finish().unwrap();
        assert_eq!(result.output_tokens, 999);
    }

    #[test]
    fn test_missing_message_delta_returns_none() {
        let mut parser = SseUsageParser::new();
        let data = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n";
        parser.feed(data);
        assert!(parser.finish().is_none());
    }

    #[test]
    fn test_non_usage_events_ignored() {
        let mut parser = SseUsageParser::new();
        let data = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0}\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\n";
        parser.feed(data);
        assert!(parser.finish().is_none());
    }

    #[test]
    fn test_content_block_delta_does_not_interfere() {
        let mut parser = SseUsageParser::new();
        let data = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":50}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hello\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\" world\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":25}}\n\n";
        parser.feed(data);
        let result = parser.finish().unwrap();
        assert_eq!(result.input_tokens, 50);
        assert_eq!(result.output_tokens, 25);
    }

    #[test]
    fn test_upstream_zero_start_real_delta() {
        // Non-standard upstream: sends input_tokens:0 in message_start,
        // real count in message_delta
        let mut parser = SseUsageParser::new();
        let data = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":20,\"output_tokens\":191}}\n\n";
        parser.feed(data);
        let result = parser.finish().unwrap();
        assert_eq!(result.input_tokens, 20);
        assert_eq!(result.output_tokens, 191);
    }

    #[test]
    fn test_upstream_zero_start_real_delta_with_cache() {
        let mut parser = SseUsageParser::new();
        let data = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":500,\"output_tokens\":300,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":200}}\n\n";
        parser.feed(data);
        let result = parser.finish().unwrap();
        assert_eq!(result.input_tokens, 500);
        assert_eq!(result.output_tokens, 300);
        assert_eq!(result.cache_creation_tokens, Some(100));
        assert_eq!(result.cache_read_tokens, Some(200));
    }

    #[test]
    fn test_standard_upstream_not_overwritten_by_delta() {
        // Standard Anthropic: real input in message_start, no input in message_delta
        let mut parser = SseUsageParser::new();
        let data = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":150,\"cache_read_input_tokens\":50}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":80}}\n\n";
        parser.feed(data);
        let result = parser.finish().unwrap();
        assert_eq!(result.input_tokens, 150);
        assert_eq!(result.output_tokens, 80);
        assert_eq!(result.cache_read_tokens, Some(50));
    }

}
