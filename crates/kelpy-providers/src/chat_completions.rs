//! Chat Completions (`POST /chat/completions`), streamed: Ollama and the many servers that speak OpenAI's earlier
//! protocol.

use serde_json::{Value, json};

use crate::{ChatMessage, ChatRequest, Collected, ProviderError, Step, StopReason};

pub const OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";

/// The request for `chat`. With `report_usage` it asks for the tokens used, in a last chunk: most servers know the
/// option, and the strict ones that do not refuse the request, which is then sent again without it.
pub fn request(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    chat: &ChatRequest<'_>,
    report_usage: bool,
) -> reqwest::RequestBuilder {
    let mut messages = Vec::with_capacity(chat.messages.len() + 1);
    if !chat.system.is_empty() {
        messages.push(json!({ "role": "system", "content": chat.system }));
    }
    messages.extend(chat.messages.iter().map(|message| match message {
        ChatMessage::User { text, images } => {
            let (text, images) = ChatMessage::user_parts(text, images.as_ref());
            if images.is_empty() {
                // As a string: servers for text alone refuse content given as parts.
                json!({ "role": "user", "content": text })
            } else {
                let mut content: Vec<Value> = images
                    .iter()
                    .map(|image| json!({ "type": "image_url", "image_url": { "url": image.data_url() } }))
                    .collect();
                content.push(json!({ "type": "text", "text": text }));
                json!({ "role": "user", "content": content })
            }
        }
        ChatMessage::Assistant { text, calls, .. } if calls.is_empty() => {
            json!({ "role": "assistant", "content": text })
        }
        ChatMessage::Assistant { text, calls, .. } => {
            let calls: Vec<Value> = calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": { "name": call.name, "arguments": call.arguments },
                    })
                })
                .collect();
            json!({ "role": "assistant", "content": text, "tool_calls": calls })
        }
        ChatMessage::Tool(result) => {
            json!({ "role": "tool", "tool_call_id": result.call_id, "content": result.output })
        }
    }));
    let mut body = json!({ "model": model, "messages": messages, "stream": true });
    if report_usage {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if !chat.tools.is_empty() {
        let tools: Vec<Value> = chat
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    let builder = client
        .post(format!(
            "{}/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .json(&body);
    match api_key {
        Some(key) => builder.bearer_auth(key),
        None => builder,
    }
}

/// Reads one chunk. Tool calls come in pieces, by their index: the first piece names the call, the others add to
/// its arguments.
pub fn parse(data: &str, collected: &mut Collected) -> Result<Step, ProviderError> {
    if data.trim() == "[DONE]" {
        return Ok(Step::finished(None));
    }
    let chunk: Value = serde_json::from_str(data)
        .map_err(|error| ProviderError::Protocol(format!("invalid chunk: {error}")))?;
    if let Some(error) = chunk.get("error") {
        let message = error["message"]
            .as_str()
            .or_else(|| error.as_str())
            .unwrap_or("unknown error")
            .to_owned();
        return Err(ProviderError::Api {
            status: None,
            message,
        });
    }
    let choice = &chunk["choices"][0];
    for (position, piece) in choice["delta"]["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let index = piece["index"].as_u64().unwrap_or(position as u64);
        let (id, function) = (piece["id"].as_str(), &piece["function"]);
        if let Some(name) = function["name"].as_str().filter(|name| !name.is_empty()) {
            collected.keep(name.len())?;
            name.clone_into(&mut collected.call(index, id)?.name);
        }
        let arguments = function["arguments"].as_str().unwrap_or_default();
        collected.arguments(index, id, arguments)?;
    }
    Ok(Step {
        text: choice["delta"]["content"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        stop: choice["finish_reason"].as_str().map(|reason| match reason {
            "stop" | "tool_calls" => StopReason::Complete,
            "length" => StopReason::TooLong,
            "content_filter" => StopReason::Refused,
            other => StopReason::Other(other.to_owned()),
        }),
        input_tokens: chunk["usage"]["prompt_tokens"].as_u64(),
        finished: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolCall, ToolResult};

    fn parse(data: &str) -> Result<Step, ProviderError> {
        super::parse(data, &mut Collected::default())
    }

    #[test]
    fn tool_calls_are_put_together_from_their_pieces() {
        let mut collected = Collected::default();
        for chunk in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"disk_usage","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"reason\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"space\"}"}}]}}]}"#,
            // A server that numbers every call 0 tells them apart by their ids.
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_b","function":{"name":"processes","arguments":"{}"}}]}}]}"#,
        ] {
            super::parse(chunk, &mut collected).unwrap();
        }
        let last = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(
            super::parse(last, &mut collected).unwrap().stop,
            Some(StopReason::Complete)
        );
        let call = |id: &str, name: &str, arguments: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        };
        assert_eq!(
            collected.finish().0,
            [
                call("call_a", "disk_usage", r#"{"reason":"space"}"#),
                call("call_b", "processes", "{}"),
            ]
        );
    }

    #[test]
    fn what_calls_gather_across_events_is_limited() {
        // Each piece is far below the limit of an event; together they pass the limit of an answer's calls.
        let piece = "a".repeat(512 << 10);
        let chunk = serde_json::json!({ "choices": [{ "delta": { "tool_calls": [{
            "index": 0, "id": "call_a", "function": { "name": "read_file", "arguments": piece },
        }]}}]})
        .to_string();
        let mut collected = Collected::default();
        let refused = (0..12).find_map(|_| super::parse(&chunk, &mut collected).err());
        assert!(
            matches!(refused, Some(ProviderError::TooLarge(_))),
            "{refused:?}"
        );

        let mut collected = Collected::default();
        let refused = (0..=crate::MAX_CALLS).find_map(|number| {
            let call = format!(
                r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":{number},"id":"call_{number}","function":{{"name":"processes","arguments":"{{}}"}}}}]}}}}]}}"#
            );
            super::parse(&call, &mut collected).err()
        });
        assert!(
            matches!(refused, Some(ProviderError::TooLarge(_))),
            "{refused:?}"
        );
    }

    #[test]
    fn calls_and_results_go_back_as_tool_messages() {
        let client = crate::Provider::new().unwrap().client;
        let messages = [
            ChatMessage::user("Disks?"),
            ChatMessage::Assistant {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "call_a".into(),
                    name: "disk_usage".into(),
                    arguments: "{}".into(),
                }],
                replay: Vec::new(),
            },
            ChatMessage::Tool(ToolResult {
                call_id: "call_a".into(),
                output: "/ 40% used".into(),
                is_error: false,
            }),
        ];
        let chat = ChatRequest {
            system: "",
            messages: &messages,
            tools: &[],
        };
        let built = request(&client, OLLAMA_BASE_URL, None, "llama", &chat, false)
            .build()
            .unwrap();
        let body: Value =
            serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call_a");
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["name"],
            "disk_usage"
        );
        assert_eq!(body["messages"][2]["role"], "tool");
        assert_eq!(body["messages"][2]["tool_call_id"], "call_a");
    }

    #[test]
    fn chunks_finish_reasons_and_errors_are_recognised() {
        let chunk = r#"{"choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        assert_eq!(parse(chunk).unwrap().text.as_deref(), Some("Hi"));
        let last = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#;
        assert_eq!(
            parse(last).unwrap(),
            Step {
                stop: Some(StopReason::TooLong),
                ..Step::default()
            }
        );
        let usage = r#"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":8}}"#;
        assert_eq!(parse(usage).unwrap().input_tokens, Some(120));
        assert_eq!(parse("[DONE]").unwrap(), Step::finished(None));
        assert!(parse(r#"{"error":{"message":"bad model"}}"#).is_err());
        assert!(parse(r#"{"error":"model not found"}"#).is_err());
    }
}
